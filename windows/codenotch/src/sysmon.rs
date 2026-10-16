//! The System block's readings: CPU, RAM, disk (space and activity) and internet throughput, sampled
//! once a second on one low-priority thread. Tauri-free on purpose: main.rs only forwards each
//! snapshot to the pages, and `codenotch.exe sysmon` prints the same numbers without a UI.
//! Nothing here needs administrator rights and nothing here ever panics: every failed reading is
//! `None`, never an invented number.

use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const TICK: Duration = Duration::from_secs(1);
/// Wait between priming the counters and the first sample, so that snapshot already holds deltas
const PRIME_WAIT: Duration = Duration::from_millis(500);
const MAX_OTHER_DRIVES: usize = 4;
/// Consecutive failed activity reads before the disk backend is opened again
const DISK_RETRY_AFTER: u32 = 5;

// ---- snapshot (JSON shape = the pages' contract) ----

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct SysSnapshot {
    pub at: u64,
    pub cpu: Option<Cpu>,
    pub ram: Option<Ram>,
    pub disk: Option<Disk>,
    pub net: Option<Net>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Cpu {
    pub frac: f64,
    pub name: String,
    pub threads: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Ram {
    pub frac: f64,
    pub used: u64,
    pub total: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DriveInfo {
    pub drive: String,
    pub frac: f64,
    pub used: u64,
    pub total: u64,
}

/// `busy`, `read_bytes_s` and `write_bytes_s` are `None` when no activity counter can be read;
/// the capacity fields are still reported then.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Disk {
    pub drive: String,
    pub kind: String,
    pub frac: f64,
    pub used: u64,
    pub total: u64,
    pub busy: Option<f64>,
    pub read_bytes_s: Option<f64>,
    pub write_bytes_s: Option<f64>,
    pub others: Vec<DriveInfo>,
}

/// While `connected` is false the rates are 0, `link_bits_s` is `None`, `kind` is "other" and `name` is empty.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Net {
    pub connected: bool,
    pub kind: String,
    pub name: String,
    pub down_bytes_s: f64,
    pub up_bytes_s: f64,
    pub link_bits_s: Option<u64>,
}

// ---- pure computations (everything a unit test can pin down without touching Windows) ----

#[derive(Clone, Copy, Debug, PartialEq)]
struct CpuTimes {
    idle: u64,
    /// GetSystemTimes' kernel time already includes the idle time
    kernel: u64,
    user: u64,
}

/// Busy share between two readings; `None` when the clock did not advance (two reads inside one
/// scheduler tick would otherwise show a fake 0 %). Only the fallback for the CPU figure: the number
/// the notch normally shows is `utility_share`.
fn cpu_frac(prev: CpuTimes, cur: CpuTimes) -> Option<f64> {
    let idle = cur.idle.saturating_sub(prev.idle);
    let total = cur.kernel.saturating_sub(prev.kernel).saturating_add(cur.user.saturating_sub(prev.user));
    if total == 0 {
        return None;
    }
    Some((1.0 - idle as f64 / total as f64).clamp(0.0, 1.0))
}

/// Task Manager (Windows 11) does not show busy time as its CPU figure but a share of the machine's
/// maximum capacity: busy time weighted by the clock the cores ran at, over what they could do at
/// their top clocks. `% Processor Utility` is the first half (about 160 % when every core is busy on
/// a laptop that turbos), so the figure shown is that over the highest utility the CPU can reach.
/// Measured on the owner's i7-1355U against Task Manager, 25 paired readings from idle to pegged:
/// Task Manager is about utility / 2.2, give or take 6 points, while plain busy time ran 17 points
/// too high and stood at 100 when Task Manager said 81. The 220 is only where `max_seen` starts:
/// it grows if this CPU is seen clocking higher.
const START_MAX_UTILITY: f64 = 220.0;

fn utility_share(utility: f64, max_seen: &mut f64) -> f64 {
    if !utility.is_finite() {
        return 0.0;
    }
    *max_seen = max_seen.max(utility);
    (utility / *max_seen).clamp(0.0, 1.0)
}

fn ratio(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        return 0.0;
    }
    (part as f64 / whole as f64).clamp(0.0, 1.0)
}

/// Growth of a monotonic counter; a reset or a vanished adapter (cur < prev) counts as 0 for that tick
fn counter_delta(prev: u64, cur: u64) -> u64 {
    cur.saturating_sub(prev)
}

fn per_second(delta: u64, secs: f64) -> f64 {
    if secs > 0.0 {
        delta as f64 / secs
    } else {
        0.0
    }
}

/// Task Manager's "Active time": the share of the interval the disk was not idle
fn busy_frac(d_idle: u64, d_query: u64) -> Option<f64> {
    if d_query == 0 {
        return None;
    }
    Some((1.0 - d_idle as f64 / d_query as f64).clamp(0.0, 1.0))
}

/// IOCTL_DISK_PERFORMANCE counters: bytes plus idle/query time in 100 ns units
#[derive(Clone, Copy, Debug, PartialEq)]
struct DiskPerf {
    read: u64,
    write: u64,
    idle: u64,
    query: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Activity {
    busy: Option<f64>,
    read: Option<f64>,
    write: Option<f64>,
}

fn perf_activity(prev: &DiskPerf, cur: &DiskPerf, secs: f64) -> Activity {
    Activity {
        busy: busy_frac(counter_delta(prev.idle, cur.idle), counter_delta(prev.query, cur.query)),
        read: Some(per_second(counter_delta(prev.read, cur.read), secs)),
        write: Some(per_second(counter_delta(prev.write, cur.write), secs)),
    }
}

/// What the disk backend read: raw counters (rates need the previous reading) or ready rates (PDH)
#[derive(Clone, Copy, Debug, PartialEq)]
enum DiskRaw {
    Perf(DiskPerf),
    Rates(Activity),
}

fn disk_activity(prev: Option<&DiskRaw>, cur: Option<&DiskRaw>, secs: Option<f64>) -> Activity {
    match (prev, cur, secs) {
        (Some(DiskRaw::Perf(p)), Some(DiskRaw::Perf(c)), Some(s)) => perf_activity(p, c, s),
        // A PDH rate is already averaged over the interval since its previous collect
        (Some(_), Some(DiskRaw::Rates(a)), _) => *a,
        _ => Activity::default(),
    }
}

/// One row of the interface table, reduced to what the filter and the rates need
#[derive(Clone, Debug, PartialEq)]
struct NetRow {
    key: u64,
    alias: String,
    if_type: u32,
    up: bool,
    hardware: bool,
    no_media: bool,
    in_octets: u64,
    out_octets: u64,
    link_bits: u64,
}

const IF_TYPE_ETHERNET: u32 = 6;
const IF_TYPE_LOOPBACK: u32 = 24;
const IF_TYPE_WIFI: u32 = 71;
const IF_TYPE_TUNNEL: u32 = 131;

/// Only real, connected hardware counts: Hyper-V, VirtualBox and VPN adapters carry the same bytes
/// the physical adapter already counted, so summing them would double the speed shown.
fn counts(r: &NetRow) -> bool {
    r.up && r.hardware && !r.no_media && r.if_type != IF_TYPE_LOOPBACK && r.if_type != IF_TYPE_TUNNEL
}

fn net_kind(if_type: u32) -> &'static str {
    match if_type {
        IF_TYPE_WIFI => "wifi",
        IF_TYPE_ETHERNET => "ethernet",
        _ => "other",
    }
}

/// Drivers report a disconnected or unknown link as 0 or all ones
fn known_link(bits: u64) -> Option<u64> {
    (bits != 0 && bits != u64::MAX).then_some(bits)
}

fn counted_counters(rows: &[NetRow]) -> HashMap<u64, (u64, u64)> {
    rows.iter().filter(|r| counts(r)).map(|r| (r.key, (r.in_octets, r.out_octets))).collect()
}

/// Rates over the counted adapters. An adapter missing from `prev` (it just came up) contributes 0:
/// its lifetime total is not an interval delta. The reported adapter is the one that moved the most
/// bytes this interval, or the fastest link when nothing moved.
fn net_summary(prev: &HashMap<u64, (u64, u64)>, rows: &[NetRow], secs: Option<f64>) -> Net {
    let (mut down, mut up) = (0u64, 0u64);
    let mut best: Option<(u64, u64, &NetRow)> = None;
    let mut link: Option<u64> = None;
    for r in rows.iter().filter(|r| counts(r)) {
        let (d_in, d_out) = prev
            .get(&r.key)
            .map(|&(i, o)| (counter_delta(i, r.in_octets), counter_delta(o, r.out_octets)))
            .unwrap_or((0, 0));
        down = down.saturating_add(d_in);
        up = up.saturating_add(d_out);
        let speed = known_link(r.link_bits);
        link = link.max(speed);
        let rank = (d_in.saturating_add(d_out), speed.unwrap_or(0));
        if best.is_none_or(|(moved, fast, _)| rank > (moved, fast)) {
            best = Some((rank.0, rank.1, r));
        }
    }
    let Some((_, _, shown)) = best else {
        return Net { connected: false, kind: "other".into(), name: String::new(), down_bytes_s: 0.0, up_bytes_s: 0.0, link_bits_s: None };
    };
    let secs = secs.unwrap_or(0.0);
    Net {
        connected: true,
        kind: net_kind(shown.if_type).into(),
        name: shown.alias.clone(),
        down_bytes_s: per_second(down, secs),
        up_bytes_s: per_second(up, secs),
        link_bits_s: link,
    }
}

/// Human-readable size for the probe output; 1024-based like Windows' own "GB"
fn fmt_bytes(n: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n.max(0.0);
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{v:.0} {}", UNITS[i])
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

fn fmt_pct(frac: f64) -> String {
    format!("{:.0}%", frac * 100.0)
}

/// Megabits per second (1 Mb = 1e6 bits), the unit internet speeds are quoted in
fn fmt_mbits(bytes_s: f64) -> String {
    format!("{:.1} Mb/s", bytes_s * 8.0 / 1e6)
}

fn probe_line(secs: u32, s: &SysSnapshot) -> String {
    let mut o = format!("{secs:>3}s");
    match &s.cpu {
        Some(c) => o += &format!("  CPU {}", fmt_pct(c.frac)),
        None => o += "  CPU -",
    }
    match &s.ram {
        Some(r) => o += &format!("  RAM {} / {} ({})", fmt_bytes(r.used as f64), fmt_bytes(r.total as f64), fmt_pct(r.frac)),
        None => o += "  RAM -",
    }
    match &s.disk {
        Some(d) => {
            o += &format!("  {} {} / {} ({})", d.drive, fmt_bytes(d.used as f64), fmt_bytes(d.total as f64), fmt_pct(d.frac));
            match (d.busy, d.read_bytes_s, d.write_bytes_s) {
                (Some(b), Some(r), Some(w)) => o += &format!(" busy {} R {}/s W {}/s", fmt_pct(b), fmt_bytes(r), fmt_bytes(w)),
                _ => o += " activity -",
            }
        }
        None => o += "  DISK -",
    }
    match &s.net {
        Some(n) if n.connected => {
            o += &format!("  NET {} ({}) down {} up {}", n.name, n.kind, fmt_mbits(n.down_bytes_s), fmt_mbits(n.up_bytes_s));
            if let Some(l) = n.link_bits_s {
                o += &format!(" link {:.0} Mb/s", l as f64 / 1e6);
            }
        }
        Some(_) => o += "  NET offline",
        None => o += "  NET -",
    }
    o
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

// ---- Windows readings ----

#[cfg(windows)]
mod win {
    use super::{counts, Activity, CpuTimes, DiskPerf, DiskRaw, NetRow};
    use windows::core::{w, HSTRING, PCWSTR};
    use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
    use windows::Win32::NetworkManagement::IpHelper::{FreeMibTable, GetIfEntry2, GetIfTable2Ex, MibIfTableNormalWithoutStatistics, MIB_IF_ROW2, MIB_IF_TABLE2};
    use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, QueryDosDeviceW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
        OPEN_EXISTING,
    };
    use windows::Win32::System::IO::DeviceIoControl;
    use windows::Win32::System::Ioctl::{
        DEVICE_SEEK_PENALTY_DESCRIPTOR, DISK_PERFORMANCE, IOCTL_DISK_PERFORMANCE, IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery,
        StorageDeviceSeekPenaltyProperty, STORAGE_PROPERTY_QUERY,
    };
    use windows::Win32::System::Performance::{
        PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterValue, PdhOpenQueryW, PDH_FMT_COUNTERVALUE, PDH_FMT_DOUBLE,
    };
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    use windows::Win32::System::Threading::{GetCurrentThread, GetSystemTimes, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL};

    const DRIVE_FIXED: u32 = 3;

    pub fn lower_thread_priority() {
        unsafe {
            let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
        }
    }

    fn ticks(t: FILETIME) -> u64 {
        (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime)
    }

    pub fn cpu_times() -> Option<CpuTimes> {
        let (mut idle, mut kernel, mut user) = (FILETIME::default(), FILETIME::default(), FILETIME::default());
        unsafe { GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)) }.ok()?;
        Some(CpuTimes { idle: ticks(idle), kernel: ticks(kernel), user: ticks(user) })
    }

    pub fn cpu_name() -> Option<String> {
        let mut data = vec![0u16; 256];
        let mut size = (data.len() * 2) as u32;
        unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                w!(r"HARDWARE\DESCRIPTION\System\CentralProcessor\0"),
                w!("ProcessorNameString"),
                RRF_RT_REG_SZ,
                None,
                Some(data.as_mut_ptr().cast()),
                Some(&mut size),
            )
        }
        .is_ok()
        .then(|| {
            let chars = (size as usize / 2).min(data.len());
            String::from_utf16_lossy(&data[..chars]).trim_end_matches('\0').trim().to_string()
        })
    }

    /// (total, available) physical memory in bytes
    pub fn memory() -> Option<(u64, u64)> {
        let mut m = MEMORYSTATUSEX { dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
        unsafe { GlobalMemoryStatusEx(&mut m) }.ok()?;
        Some((m.ullTotalPhys, m.ullAvailPhys))
    }

    /// "C:" or whatever %SystemDrive% says
    pub fn system_drive() -> String {
        let d = std::env::var("SystemDrive").unwrap_or_default();
        let b = d.as_bytes();
        if b.len() == 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
            d.to_ascii_uppercase()
        } else {
            "C:".into()
        }
    }

    /// (total, free-to-the-caller) bytes of the volume behind "X:"
    pub fn disk_space(drive: &str) -> Option<(u64, u64)> {
        let root = HSTRING::from(format!("{drive}\\"));
        let (mut free, mut total) = (0u64, 0u64);
        unsafe { GetDiskFreeSpaceExW(&root, Some(&mut free), Some(&mut total), None) }.ok()?;
        (total > 0).then_some((total, free))
    }

    /// A `subst` drive reports itself as fixed and with its target's numbers, but it is only a folder of another drive:
    /// listing it would show that drive twice. Its DOS device name is a path (`\??\C:\dir`), a real volume's is `\Device\...`
    pub fn is_subst(drive: &str) -> bool {
        let mut target = [0u16; 512];
        let n = unsafe { QueryDosDeviceW(&HSTRING::from(drive), Some(&mut target)) } as usize;
        let prefix: Vec<u16> = r"\??\".encode_utf16().collect();
        n >= prefix.len() && target[..prefix.len()] == prefix[..]
    }

    /// Fixed local drives other than `system`, in letter order
    pub fn fixed_drives(system: &str, max: usize) -> Vec<String> {
        let mask = unsafe { GetLogicalDrives() };
        (0..26u8)
            .filter(|i| mask & (1 << i) != 0)
            .map(|i| format!("{}:", (b'A' + i) as char))
            .filter(|d| d != system && unsafe { GetDriveTypeW(&HSTRING::from(format!("{d}\\"))) } == DRIVE_FIXED && !is_subst(d))
            .take(max)
            .collect()
    }

    /// A volume handle opened with no access rights: enough for the two IOCTLs below and it needs no
    /// administrator. Held as an integer so the sampler stays `Send`; closed on drop.
    pub struct Volume(isize);

    impl Volume {
        pub fn open(drive: &str) -> Option<Self> {
            let path = HSTRING::from(format!(r"\\.\{drive}"));
            let h = unsafe { CreateFileW(&path, 0, FILE_SHARE_READ | FILE_SHARE_WRITE, None, OPEN_EXISTING, FILE_FLAGS_AND_ATTRIBUTES(0), HANDLE::default()) }.ok()?;
            Some(Volume(h.0 as isize))
        }

        fn handle(&self) -> HANDLE {
            HANDLE(self.0 as *mut core::ffi::c_void)
        }

        pub fn perf(&self) -> Option<DiskPerf> {
            let mut p = DISK_PERFORMANCE::default();
            let mut got = 0u32;
            unsafe {
                DeviceIoControl(
                    self.handle(),
                    IOCTL_DISK_PERFORMANCE,
                    None,
                    0,
                    Some((&mut p as *mut DISK_PERFORMANCE).cast()),
                    std::mem::size_of::<DISK_PERFORMANCE>() as u32,
                    Some(&mut got),
                    None,
                )
            }
            .ok()?;
            let n = |v: i64| v.max(0) as u64;
            Some(DiskPerf { read: n(p.BytesRead), write: n(p.BytesWritten), idle: n(p.IdleTime), query: n(p.QueryTime) })
        }

        /// `Some(true)` when the device has no seek penalty (an SSD)
        pub fn is_ssd(&self) -> Option<bool> {
            let q = STORAGE_PROPERTY_QUERY { PropertyId: StorageDeviceSeekPenaltyProperty, QueryType: PropertyStandardQuery, AdditionalParameters: [0] };
            let mut d = DEVICE_SEEK_PENALTY_DESCRIPTOR::default();
            let mut got = 0u32;
            unsafe {
                DeviceIoControl(
                    self.handle(),
                    IOCTL_STORAGE_QUERY_PROPERTY,
                    Some((&q as *const STORAGE_PROPERTY_QUERY).cast()),
                    std::mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
                    Some((&mut d as *mut DEVICE_SEEK_PENALTY_DESCRIPTOR).cast()),
                    std::mem::size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>() as u32,
                    Some(&mut got),
                    None,
                )
            }
            .ok()?;
            Some(d.IncursSeekPenalty.0 == 0)
        }
    }

    impl Drop for Volume {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.handle());
            }
        }
    }

    /// Fallback when the volume IOCTL is refused: the same numbers through the performance counters.
    /// The counter paths are the English ones (`PdhAddEnglishCounterW`) because the counter names
    /// are localized and this app runs on Portuguese, German... Windows too.
    pub struct Pdh {
        query: isize,
        idle: isize,
        read: isize,
        write: isize,
    }

    impl Pdh {
        pub fn open() -> Option<Self> {
            let mut query = 0isize;
            if unsafe { PdhOpenQueryW(PCWSTR::null(), 0, &mut query) } != 0 {
                return None;
            }
            // From here on `Drop` closes the query, whichever counter fails
            let mut me = Pdh { query, idle: 0, read: 0, write: 0 };
            me.idle = me.counter(w!(r"\PhysicalDisk(_Total)\% Idle Time"))?;
            me.read = me.counter(w!(r"\PhysicalDisk(_Total)\Disk Read Bytes/sec"))?;
            me.write = me.counter(w!(r"\PhysicalDisk(_Total)\Disk Write Bytes/sec"))?;
            // Rate counters need one collection before the first readable value
            (unsafe { PdhCollectQueryData(query) } == 0).then_some(me)
        }

        fn counter(&self, path: PCWSTR) -> Option<isize> {
            let mut h = 0isize;
            (unsafe { PdhAddEnglishCounterW(self.query, path, 0, &mut h) } == 0).then_some(h)
        }

        pub fn read(&self) -> Option<DiskRaw> {
            if unsafe { PdhCollectQueryData(self.query) } != 0 {
                return None;
            }
            let idle = counter_value(self.idle);
            Some(DiskRaw::Rates(Activity {
                // "% Idle Time" can read a hair over 100
                busy: idle.map(|i| (1.0 - i / 100.0).clamp(0.0, 1.0)),
                read: counter_value(self.read),
                write: counter_value(self.write),
            }))
        }
    }

    impl Drop for Pdh {
        fn drop(&mut self) {
            unsafe {
                PdhCloseQuery(self.query);
            }
        }
    }

    /// The formatted value of a counter after a collection; `None` unless PDH says the data is valid
    fn counter_value(counter: isize) -> Option<f64> {
        let mut v = PDH_FMT_COUNTERVALUE::default();
        let status = unsafe { PdhGetFormattedCounterValue(counter, PDH_FMT_DOUBLE, None, &mut v) };
        // CStatus 0 = valid data, 1 = valid and new data
        let x = unsafe { v.Anonymous.doubleValue };
        (status == 0 && v.CStatus <= 1 && x.is_finite()).then_some(x.max(0.0))
    }

    /// `\Processor Information(_Total)\% Processor Utility` (the English path, like the disk counters):
    /// the raw material of the CPU figure, see `utility_share`. A rate counter, so the first `read`
    /// after `open` has no value.
    pub struct CpuUtility {
        query: isize,
        counter: isize,
    }

    impl CpuUtility {
        pub fn open() -> Option<Self> {
            let mut query = 0isize;
            if unsafe { PdhOpenQueryW(PCWSTR::null(), 0, &mut query) } != 0 {
                return None;
            }
            // From here on `Drop` closes the query, whether or not this Windows has the counter
            let mut me = CpuUtility { query, counter: 0 };
            let path = w!(r"\Processor Information(_Total)\% Processor Utility");
            (unsafe { PdhAddEnglishCounterW(query, path, 0, &mut me.counter) } == 0).then_some(me)
        }

        /// Percent, which passes 100 when the cores run above their nominal clock
        pub fn read(&self) -> Option<f64> {
            if unsafe { PdhCollectQueryData(self.query) } != 0 {
                return None;
            }
            counter_value(self.counter)
        }
    }

    impl Drop for CpuUtility {
        fn drop(&mut self) {
            unsafe {
                PdhCloseQuery(self.query);
            }
        }
    }

    /// Releases the table GetIfTable2Ex allocated, on every path out
    struct MibTable(*mut MIB_IF_TABLE2);

    impl Drop for MibTable {
        fn drop(&mut self) {
            unsafe { FreeMibTable(self.0.cast_const().cast()) }
        }
    }

    fn net_row(r: &MIB_IF_ROW2) -> NetRow {
        let len = r.Alias.iter().position(|&c| c == 0).unwrap_or(r.Alias.len());
        // The flags are a C bitfield: bit 0 HardwareInterface, bit 4 NotMediaConnected
        let flags = r.InterfaceAndOperStatusFlags._bitfield;
        NetRow {
            key: unsafe { r.InterfaceLuid.Value },
            alias: String::from_utf16_lossy(&r.Alias[..len]),
            if_type: r.Type,
            up: r.OperStatus == IfOperStatusUp,
            hardware: flags & 0x01 != 0,
            no_media: flags & 0x10 != 0,
            in_octets: r.InOctets,
            out_octets: r.OutOctets,
            link_bits: r.ReceiveLinkSpeed,
        }
    }

    /// The connected physical adapters with their current counters. GetIfTable2 asks the driver of
    /// every interface for statistics (about 40-250 ms here, with Hyper-V and VirtualBox installed),
    /// far too much for a 1 Hz tick: the table is read without statistics (under 1 ms) and only the
    /// adapters that count are refreshed one by one with GetIfEntry2 (about 1 ms each).
    pub fn net_rows() -> Option<Vec<NetRow>> {
        let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
        if unsafe { GetIfTable2Ex(MibIfTableNormalWithoutStatistics, &mut table) }.0 != 0 || table.is_null() {
            return None;
        }
        let table = MibTable(table);
        // The table is a C flexible array: NumEntries rows start at `Table`
        let rows = unsafe {
            let n = (*table.0).NumEntries as usize;
            std::slice::from_raw_parts(std::ptr::addr_of!((*table.0).Table).cast::<MIB_IF_ROW2>(), n)
        };
        Some(
            rows.iter()
                .filter(|r| counts(&net_row(r)))
                .filter_map(|r| {
                    let mut fresh = MIB_IF_ROW2 { InterfaceLuid: r.InterfaceLuid, InterfaceIndex: r.InterfaceIndex, ..Default::default() };
                    // An adapter that vanished between the two calls simply drops out of this tick
                    (unsafe { GetIfEntry2(&mut fresh) }.0 == 0).then(|| net_row(&fresh))
                })
                .collect(),
        )
    }
}

#[cfg(not(windows))]
mod win {
    use super::{CpuTimes, DiskPerf, DiskRaw, NetRow};

    pub fn lower_thread_priority() {}
    pub fn cpu_times() -> Option<CpuTimes> {
        None
    }
    pub fn cpu_name() -> Option<String> {
        None
    }
    pub fn memory() -> Option<(u64, u64)> {
        None
    }
    pub fn system_drive() -> String {
        "C:".into()
    }
    pub fn disk_space(_drive: &str) -> Option<(u64, u64)> {
        None
    }
    pub fn fixed_drives(_system: &str, _max: usize) -> Vec<String> {
        Vec::new()
    }
    pub struct Volume;
    impl Volume {
        pub fn open(_drive: &str) -> Option<Self> {
            None
        }
        pub fn perf(&self) -> Option<DiskPerf> {
            None
        }
        pub fn is_ssd(&self) -> Option<bool> {
            None
        }
    }
    pub struct Pdh;
    impl Pdh {
        pub fn open() -> Option<Self> {
            None
        }
        pub fn read(&self) -> Option<DiskRaw> {
            None
        }
    }
    pub struct CpuUtility;
    impl CpuUtility {
        pub fn open() -> Option<Self> {
            None
        }
        pub fn read(&self) -> Option<f64> {
            None
        }
    }
    pub fn net_rows() -> Option<Vec<NetRow>> {
        None
    }
}

// ---- sampler ----

/// Where the activity numbers come from
enum Backend {
    Volume(win::Volume),
    Pdh(win::Pdh),
    None,
}

impl Backend {
    /// The volume IOCTL first (cheap, no counter database); PDH only when it is refused
    fn open(drive: &str) -> Backend {
        if let Some(v) = win::Volume::open(drive) {
            if v.perf().is_some() {
                return Backend::Volume(v);
            }
        }
        win::Pdh::open().map_or(Backend::None, Backend::Pdh)
    }

    fn read(&self) -> Option<DiskRaw> {
        match self {
            Backend::Volume(v) => v.perf().map(DiskRaw::Perf),
            Backend::Pdh(p) => p.read(),
            Backend::None => None,
        }
    }
}

/// Everything the next sample is measured against
struct Prev {
    at: Instant,
    cpu: Option<CpuTimes>,
    net: HashMap<u64, (u64, u64)>,
    disk: Option<DiskRaw>,
}

/// CPU name and thread count never change while the app runs
fn cpu_info() -> &'static (String, u32) {
    static INFO: OnceLock<(String, u32)> = OnceLock::new();
    INFO.get_or_init(|| {
        let name = win::cpu_name().filter(|n| !n.is_empty()).unwrap_or_else(|| "CPU".into());
        let threads = std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(1);
        (name, threads)
    })
}

fn detect_kind(drive: &str) -> &'static str {
    match win::Volume::open(drive).and_then(|v| v.is_ssd()) {
        Some(true) => "ssd",
        Some(false) => "hdd",
        None => "unknown",
    }
}

pub struct Sampler {
    drive: String,
    kind: &'static str,
    backend: Backend,
    /// Where the CPU figure comes from; `None` where the counter does not exist, and busy time stands in
    cpu_util: Option<win::CpuUtility>,
    /// The highest `% Processor Utility` seen, which the CPU figure is a share of (see `utility_share`)
    max_utility: f64,
    disk_failures: u32,
    prev: Option<Prev>,
}

impl Default for Sampler {
    fn default() -> Self {
        Self::new()
    }
}

impl Sampler {
    /// Resolves the static facts and primes every baseline, so the next `sample` already has deltas
    pub fn new() -> Self {
        let drive = win::system_drive();
        let mut s = Sampler { kind: detect_kind(&drive), backend: Backend::open(&drive), drive, cpu_util: win::CpuUtility::open(), max_utility: START_MAX_UTILITY, disk_failures: 0, prev: None };
        s.prime();
        s
    }

    fn is_primed(&self) -> bool {
        self.prev.is_some()
    }

    /// Takes the baselines the next `sample` is measured against
    fn prime(&mut self) {
        if matches!(self.backend, Backend::None) {
            self.backend = Backend::open(&self.drive);
        }
        if self.cpu_util.is_none() {
            self.cpu_util = win::CpuUtility::open();
        }
        // The counter keeps its own baseline: this read is the one the next sample is measured against
        if let Some(u) = &self.cpu_util {
            let _ = u.read();
        }
        self.prev = Some(Prev {
            at: Instant::now(),
            cpu: win::cpu_times(),
            net: win::net_rows().map(|r| counted_counters(&r)).unwrap_or_default(),
            disk: self.backend.read(),
        });
    }

    /// Forgets the baselines: after a pause the first sample must not average over the gap
    pub fn reset(&mut self) {
        self.prev = None;
    }

    pub fn sample(&mut self) -> SysSnapshot {
        let now = Instant::now();
        let cpu_now = win::cpu_times();
        let net_now = win::net_rows();
        let disk_now = self.backend.read();
        self.track_disk_health(disk_now.is_some());
        let secs = self.prev.as_ref().map(|p| now.duration_since(p.at).as_secs_f64()).filter(|s| *s > 0.0);
        let empty = HashMap::new();
        let prev_net = self.prev.as_ref().map_or(&empty, |p| &p.net);

        // Read every tick, primed or not, so the counter's own baseline keeps moving; on an unprimed
        // sampler the value is dropped (it would span the whole pause)
        let utility = self.cpu_util.as_ref().and_then(|u| u.read()).filter(|_| self.prev.is_some());
        let busy = match (self.prev.as_ref().and_then(|p| p.cpu), cpu_now) {
            (Some(p), Some(c)) => cpu_frac(p, c),
            _ => None,
        };
        let shown = match utility {
            Some(u) => Some(utility_share(u, &mut self.max_utility)),
            None => busy,
        };
        let cpu = shown.map(|frac| {
            let (name, threads) = cpu_info();
            Cpu { frac, name: name.clone(), threads: *threads }
        });
        let ram = win::memory().filter(|&(total, _)| total > 0).map(|(total, avail)| {
            let used = total.saturating_sub(avail);
            Ram { frac: ratio(used, total), used, total }
        });
        let act = disk_activity(self.prev.as_ref().and_then(|p| p.disk.as_ref()), disk_now.as_ref(), secs);
        let disk = self.disk(act);
        let net = net_now.as_ref().map(|rows| net_summary(prev_net, rows, secs));

        let next_net = net_now.as_deref().map(counted_counters).unwrap_or_default();
        self.prev = Some(Prev { at: now, cpu: cpu_now, net: next_net, disk: disk_now });
        SysSnapshot { at: now_ms(), cpu, ram, disk, net }
    }

    /// A backend that used to answer and now keeps failing (volume remounted, counters reloaded) is reopened
    fn track_disk_health(&mut self, ok: bool) {
        if ok || matches!(self.backend, Backend::None) {
            self.disk_failures = 0;
            return;
        }
        self.disk_failures += 1;
        if self.disk_failures >= DISK_RETRY_AFTER {
            self.disk_failures = 0;
            self.backend = Backend::open(&self.drive);
        }
    }

    fn disk(&self, act: Activity) -> Option<Disk> {
        let (total, free) = win::disk_space(&self.drive)?;
        let used = total.saturating_sub(free);
        let others = win::fixed_drives(&self.drive, MAX_OTHER_DRIVES)
            .into_iter()
            .filter_map(|d| {
                let (total, free) = win::disk_space(&d)?;
                let used = total.saturating_sub(free);
                Some(DriveInfo { drive: d, frac: ratio(used, total), used, total })
            })
            .collect();
        Some(Disk {
            drive: self.drive.clone(),
            kind: self.kind.into(),
            frac: ratio(used, total),
            used,
            total,
            busy: act.busy,
            read_bytes_s: act.read,
            write_bytes_s: act.write,
            others,
        })
    }
}

// ---- thread and shared state ----

static LATEST: Mutex<Option<SysSnapshot>> = Mutex::new(None);

/// The last snapshot the sampler thread emitted; every section is `None` before the first one
pub fn latest() -> SysSnapshot {
    LATEST.lock().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_default()
}

/// One background thread, one sample a second while `should_run()` says so. While it says no the
/// thread only sleeps (no reading, no callback) and forgets its baselines.
pub fn spawn(should_run: impl Fn() -> bool + Send + 'static, on_sample: impl Fn(&SysSnapshot) + Send + 'static) {
    let _ = std::thread::Builder::new().name("codenotch-sysmon".into()).spawn(move || {
        win::lower_thread_priority(); // the readings always yield to foreground work
        let mut sampler = Sampler::new();
        std::thread::sleep(PRIME_WAIT); // the first snapshot must already span an interval
        loop {
            if !should_run() {
                sampler.reset();
                std::thread::sleep(TICK);
                continue;
            }
            if !sampler.is_primed() {
                sampler.prime();
                std::thread::sleep(PRIME_WAIT);
            }
            let started = Instant::now();
            let snap = sampler.sample();
            *LATEST.lock().unwrap_or_else(|e| e.into_inner()) = Some(snap.clone());
            on_sample(&snap);
            // Sleep out the rest of the second; the floor keeps a slow callback from turning this into a busy loop
            std::thread::sleep(TICK.saturating_sub(started.elapsed()).max(Duration::from_millis(100)));
        }
    });
}

/// `codenotch.exe sysmon [seconds]`: one line a second, then the last snapshot as the pages receive it
pub fn probe(seconds: u32) -> String {
    let mut s = Sampler::new();
    let mut out = String::new();
    let mut last = SysSnapshot::default();
    for i in 1..=seconds.max(1) {
        std::thread::sleep(if i == 1 { PRIME_WAIT } else { TICK });
        last = s.sample();
        out += &probe_line(i, &last);
        out.push('\n');
    }
    out += &serde_json::to_string_pretty(&last).unwrap_or_default();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn times(idle: u64, kernel: u64, user: u64) -> CpuTimes {
        CpuTimes { idle, kernel, user }
    }

    #[test]
    fn cpu_fraction_from_deltas() {
        // 100 ticks of kernel (60 of them idle) + 100 of user = 200 total, 60 idle
        let f = cpu_frac(times(0, 0, 0), times(60, 100, 100)).unwrap();
        assert!((f - 0.7).abs() < 1e-9, "{f}");
        assert_eq!(cpu_frac(times(0, 0, 0), times(100, 100, 0)), Some(0.0));
        assert_eq!(cpu_frac(times(0, 0, 0), times(0, 50, 50)), Some(1.0));
    }

    #[test]
    fn cpu_fraction_edge_cases() {
        // No time passed: no reading rather than a fake 0 %
        assert_eq!(cpu_frac(times(5, 10, 10), times(5, 10, 10)), None);
        // Idle larger than kernel+user (a torn read) clamps instead of going negative
        assert_eq!(cpu_frac(times(0, 0, 0), times(500, 100, 100)), Some(0.0));
        // Counters that went backwards count as no progress
        assert_eq!(cpu_frac(times(10, 10, 10), times(0, 0, 0)), None);
    }

    #[test]
    fn ratio_is_clamped_and_safe() {
        assert_eq!(ratio(1, 0), 0.0);
        assert_eq!(ratio(5, 10), 0.5);
        assert_eq!(ratio(20, 10), 1.0);
    }

    #[test]
    fn cpu_is_a_share_of_the_highest_utility_seen() {
        let mut max = START_MAX_UTILITY;
        // Every core busy at about 160 %: what Task Manager shows as roughly 70 %, not 100
        assert!((utility_share(160.0, &mut max) - 160.0 / 220.0).abs() < 1e-9);
        assert_eq!(utility_share(0.0, &mut max), 0.0);
        assert_eq!(max, START_MAX_UTILITY);
        // A CPU that clocks higher than the start value raises the ceiling and never passes 100 %
        assert_eq!(utility_share(250.0, &mut max), 1.0);
        assert_eq!(max, 250.0);
        assert!((utility_share(125.0, &mut max) - 0.5).abs() < 1e-9);
        // Garbage never moves the ceiling
        assert_eq!(utility_share(f64::NAN, &mut max), 0.0);
        assert_eq!(max, 250.0);
    }

    #[test]
    fn counter_reset_counts_as_zero() {
        assert_eq!(counter_delta(100, 250), 150);
        assert_eq!(counter_delta(100, 100), 0);
        assert_eq!(counter_delta(1000, 20), 0);
    }

    #[test]
    fn rates_divide_by_the_interval() {
        assert_eq!(per_second(1000, 2.0), 500.0);
        assert_eq!(per_second(1000, 0.0), 0.0);
        assert_eq!(per_second(0, 1.0), 0.0);
    }

    #[test]
    fn busy_is_clamped_and_needs_time() {
        assert_eq!(busy_frac(0, 0), None);
        assert!((busy_frac(900, 1000).unwrap() - 0.1).abs() < 1e-9);
        assert_eq!(busy_frac(2000, 1000), Some(0.0));
        assert_eq!(busy_frac(0, 1000), Some(1.0));
    }

    #[test]
    fn disk_activity_from_two_perf_readings() {
        let a = DiskPerf { read: 1_000, write: 500, idle: 100, query: 1_000 };
        let b = DiskPerf { read: 3_000, write: 500, idle: 1_100, query: 11_000 };
        let act = perf_activity(&a, &b, 2.0);
        assert_eq!(act.read, Some(1_000.0));
        assert_eq!(act.write, Some(0.0));
        assert!((act.busy.unwrap() - 0.9).abs() < 1e-9);
        // A reset counter is 0 for that tick, and a stalled clock has no busy value
        let reset = perf_activity(&b, &a, 1.0);
        assert_eq!((reset.read, reset.write, reset.busy), (Some(0.0), Some(0.0), None));
    }

    #[test]
    fn disk_activity_needs_a_previous_reading() {
        let p = DiskRaw::Perf(DiskPerf { read: 0, write: 0, idle: 0, query: 10 });
        assert_eq!(disk_activity(None, Some(&p), Some(1.0)), Activity::default());
        assert_eq!(disk_activity(Some(&p), None, Some(1.0)), Activity::default());
        assert_eq!(disk_activity(Some(&p), Some(&p), None), Activity::default());
        let pdh = DiskRaw::Rates(Activity { busy: Some(0.5), read: Some(1.0), write: Some(2.0) });
        assert_eq!(disk_activity(Some(&p), Some(&pdh), None).busy, Some(0.5));
        assert_eq!(disk_activity(None, Some(&pdh), None), Activity::default());
    }

    fn row(key: u64, alias: &str, if_type: u32) -> NetRow {
        NetRow { key, alias: alias.into(), if_type, up: true, hardware: true, no_media: false, in_octets: 0, out_octets: 0, link_bits: 1_000_000_000 }
    }

    #[test]
    fn only_physical_connected_adapters_count() {
        let wifi = row(1, "Wi-Fi", 71);
        assert!(counts(&wifi));
        assert!(counts(&row(2, "Ethernet", 6)));
        // VirtualBox Host-Only and Hyper-V vEthernet: up, Ethernet type, but not hardware
        assert!(!counts(&NetRow { hardware: false, ..row(3, "VirtualBox Host-Only Network", 6) }));
        assert!(!counts(&NetRow { hardware: false, ..row(4, "vEthernet (Default Switch)", 6) }));
        assert!(!counts(&row(5, "Loopback", 24)));
        assert!(!counts(&row(6, "VPN", 131)));
        assert!(!counts(&NetRow { up: false, ..row(7, "Ethernet 2", 6) }));
        assert!(!counts(&NetRow { no_media: true, ..row(8, "Ethernet 3", 6) }));
    }

    #[test]
    fn net_rates_come_from_counted_adapters_only() {
        let mut rows = vec![
            NetRow { in_octets: 1_000, out_octets: 100, ..row(1, "Wi-Fi", 71) },
            NetRow { in_octets: 9_999_999, out_octets: 9_999_999, hardware: false, ..row(2, "vEthernet (Default Switch)", 6) },
        ];
        let prev = counted_counters(&rows);
        assert_eq!(prev.len(), 1);
        rows[0].in_octets += 2_000_000;
        rows[0].out_octets += 500_000;
        rows[1].in_octets += 8_000_000;
        let n = net_summary(&prev, &rows, Some(2.0));
        assert!(n.connected);
        assert_eq!((n.kind.as_str(), n.name.as_str()), ("wifi", "Wi-Fi"));
        assert_eq!(n.down_bytes_s, 1_000_000.0);
        assert_eq!(n.up_bytes_s, 250_000.0);
        assert_eq!(n.link_bits_s, Some(1_000_000_000));
    }

    #[test]
    fn net_reset_and_new_adapters_count_as_zero() {
        let mut wifi = NetRow { in_octets: 5_000, out_octets: 5_000, ..row(1, "Wi-Fi", 71) };
        let prev = counted_counters(std::slice::from_ref(&wifi));
        // Counter reset
        wifi.in_octets = 10;
        wifi.out_octets = 4_000;
        let n = net_summary(&prev, std::slice::from_ref(&wifi), Some(1.0));
        assert_eq!((n.down_bytes_s, n.up_bytes_s), (0.0, 0.0));
        // Adapter that was not there before: its lifetime total is not a delta
        let fresh = NetRow { in_octets: 7_000_000, ..row(9, "Ethernet", 6) };
        let n = net_summary(&HashMap::new(), std::slice::from_ref(&fresh), Some(1.0));
        assert!(n.connected);
        assert_eq!(n.down_bytes_s, 0.0);
        // No interval yet (unprimed): zero rates, still connected
        assert_eq!(net_summary(&prev, std::slice::from_ref(&wifi), None).down_bytes_s, 0.0);
    }

    #[test]
    fn offline_when_nothing_counts() {
        let virt = NetRow { hardware: false, ..row(1, "vEthernet", 6) };
        let n = net_summary(&HashMap::new(), &[virt], Some(1.0));
        assert!(!n.connected);
        assert_eq!((n.down_bytes_s, n.up_bytes_s, n.link_bits_s), (0.0, 0.0, None));
        assert!(!net_summary(&HashMap::new(), &[], Some(1.0)).connected);
    }

    #[test]
    fn the_adapter_that_moved_most_is_reported() {
        let prev: HashMap<u64, (u64, u64)> = [(1, (0, 0)), (2, (0, 0))].into();
        let wifi = NetRow { in_octets: 100, link_bits: 1_200_000_000, ..row(1, "Wi-Fi", 71) };
        let eth = NetRow { in_octets: 900, link_bits: 1_000_000_000, ..row(2, "Ethernet", 6) };
        let n = net_summary(&prev, &[wifi.clone(), eth.clone()], Some(1.0));
        assert_eq!((n.kind.as_str(), n.name.as_str()), ("ethernet", "Ethernet"));
        assert_eq!(n.link_bits_s, Some(1_200_000_000)); // the fastest counted link, not the busiest one's
        // Nothing moved: the fastest link is the one described
        let idle_prev: HashMap<u64, (u64, u64)> = [(1, (100, 0)), (2, (900, 0))].into();
        let n = net_summary(&idle_prev, &[wifi, eth], Some(1.0));
        assert_eq!(n.kind, "wifi");
    }

    #[test]
    fn unknown_link_speeds_are_ignored() {
        assert_eq!(known_link(0), None);
        assert_eq!(known_link(u64::MAX), None);
        assert_eq!(known_link(866_000_000), Some(866_000_000));
        let unknown = NetRow { link_bits: u64::MAX, ..row(1, "Ethernet", 6) };
        assert_eq!(net_summary(&HashMap::new(), &[unknown], Some(1.0)).link_bits_s, None);
    }

    #[cfg(windows)]
    #[test]
    fn a_real_volume_is_not_taken_for_a_subst_alias() {
        // (a `subst` drive answers with a `\??\` path; making one here would show a drive letter to the whole session)
        assert!(!win::is_subst(&win::system_drive()));
    }

    #[test]
    fn adapter_kinds() {
        assert_eq!(net_kind(71), "wifi");
        assert_eq!(net_kind(6), "ethernet");
        assert_eq!(net_kind(243), "other"); // WWAN
    }

    #[test]
    fn sizes_are_formatted_for_reading() {
        assert_eq!(fmt_bytes(0.0), "0 B");
        assert_eq!(fmt_bytes(1023.0), "1023 B");
        assert_eq!(fmt_bytes(1536.0), "1.5 KB");
        assert_eq!(fmt_bytes(13_728_919_552.0), "12.8 GB");
        assert_eq!(fmt_bytes(-5.0), "0 B");
        assert_eq!(fmt_bytes(3.0 * 1024f64.powi(5)), "3072.0 TB");
        assert_eq!(fmt_pct(0.834), "83%");
        assert_eq!(fmt_mbits(10_500_000.0), "84.0 Mb/s");
    }

    fn full_snapshot() -> SysSnapshot {
        SysSnapshot {
            at: 1_790_600_000_000,
            cpu: Some(Cpu { frac: 0.23, name: "13th Gen Intel(R) Core(TM) i7-1355U".into(), threads: 12 }),
            ram: Some(Ram { frac: 0.83, used: 13_728_919_552, total: 16_849_252_352 }),
            disk: Some(Disk {
                drive: "C:".into(),
                kind: "ssd".into(),
                frac: 0.64,
                used: 312_565_428_224,
                total: 490_740_903_936,
                busy: Some(0.04),
                read_bytes_s: Some(12_345_678.0),
                write_bytes_s: None,
                others: vec![DriveInfo { drive: "D:".into(), frac: 0.4, used: 400, total: 1000 }],
            }),
            net: Some(Net { connected: true, kind: "wifi".into(), name: "Wi-Fi".into(), down_bytes_s: 10_500_000.0, up_bytes_s: 1_500_000.0, link_bits_s: Some(1_200_000_000) }),
        }
    }

    #[test]
    fn json_keys_match_the_pages_contract() {
        let v = serde_json::to_value(full_snapshot()).unwrap();
        let keys = |v: &serde_json::Value| {
            let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
            k.sort();
            k
        };
        assert_eq!(keys(&v), ["at", "cpu", "disk", "net", "ram"]);
        assert_eq!(keys(&v["cpu"]), ["frac", "name", "threads"]);
        assert_eq!(keys(&v["ram"]), ["frac", "total", "used"]);
        assert_eq!(keys(&v["disk"]), ["busy", "drive", "frac", "kind", "others", "read_bytes_s", "total", "used", "write_bytes_s"]);
        assert_eq!(keys(&v["disk"]["others"][0]), ["drive", "frac", "total", "used"]);
        assert_eq!(keys(&v["net"]), ["connected", "down_bytes_s", "kind", "link_bits_s", "name", "up_bytes_s"]);
        // Missing readings are explicit nulls, never absent keys
        assert!(v["disk"]["write_bytes_s"].is_null());
        let empty = serde_json::to_value(SysSnapshot::default()).unwrap();
        assert_eq!(keys(&empty), ["at", "cpu", "disk", "net", "ram"]);
        assert!(empty["cpu"].is_null() && empty["ram"].is_null() && empty["disk"].is_null() && empty["net"].is_null());
    }

    #[test]
    fn probe_line_survives_missing_sections() {
        let full = probe_line(3, &full_snapshot());
        assert!(full.contains("CPU 23%") && full.contains("RAM 12.8 GB / 15.7 GB (83%)") && full.contains("NET Wi-Fi (wifi)"), "{full}");
        assert!(full.contains("activity -"), "{full}"); // write rate missing => the whole activity part is withheld
        let empty = probe_line(1, &SysSnapshot::default());
        assert!(empty.contains("CPU -") && empty.contains("DISK -") && empty.contains("NET -"), "{empty}");
        let offline = SysSnapshot { net: Some(Net { connected: false, kind: "other".into(), name: String::new(), down_bytes_s: 0.0, up_bytes_s: 0.0, link_bits_s: None }), ..Default::default() };
        assert!(probe_line(1, &offline).contains("NET offline"));
    }

    /// The real machine, loosely: only invariants, because every section may legitimately be absent
    #[test]
    fn two_real_samples_respect_the_invariants() {
        let mut s = Sampler::new();
        std::thread::sleep(Duration::from_millis(300));
        let a = s.sample();
        std::thread::sleep(Duration::from_millis(300));
        let b = s.sample();
        for snap in [&a, &b] {
            assert!(snap.at > 0);
            if let Some(c) = &snap.cpu {
                assert!((0.0..=1.0).contains(&c.frac) && c.threads >= 1 && !c.name.is_empty());
            }
            if let Some(r) = &snap.ram {
                assert!((0.0..=1.0).contains(&r.frac) && r.used <= r.total && r.total > 0);
            }
            if let Some(d) = &snap.disk {
                assert!((0.0..=1.0).contains(&d.frac) && d.used <= d.total);
                assert!(["ssd", "hdd", "unknown"].contains(&d.kind.as_str()));
                assert!(d.others.len() <= MAX_OTHER_DRIVES && d.others.iter().all(|o| o.used <= o.total && o.drive != d.drive));
                if let Some(x) = d.busy {
                    assert!((0.0..=1.0).contains(&x));
                }
                for x in [d.read_bytes_s, d.write_bytes_s].into_iter().flatten() {
                    assert!(x >= 0.0 && x.is_finite());
                }
            }
            if let Some(n) = &snap.net {
                assert!(n.down_bytes_s >= 0.0 && n.up_bytes_s >= 0.0 && n.down_bytes_s.is_finite() && n.up_bytes_s.is_finite());
                if !n.connected {
                    assert!(n.down_bytes_s == 0.0 && n.up_bytes_s == 0.0 && n.link_bits_s.is_none());
                }
            }
        }
        assert!(b.at >= a.at);
    }

    #[test]
    fn reset_drops_the_baselines_and_sampling_recovers() {
        let mut s = Sampler::new();
        assert!(s.is_primed());
        s.reset();
        assert!(!s.is_primed());
        // Unprimed: no CPU value (nothing to compare against), no invented rates
        let first = s.sample();
        assert!(first.cpu.is_none());
        if let Some(d) = &first.disk {
            assert!(d.busy.is_none() && d.read_bytes_s.is_none());
        }
        assert!(s.is_primed());
    }

    #[test]
    fn latest_is_empty_before_the_first_sample() {
        // The static is shared with nothing else in this test binary
        assert_eq!(latest().at, 0);
    }
}
