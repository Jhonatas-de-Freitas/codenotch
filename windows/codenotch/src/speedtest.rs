//! Internet speed test against Cloudflare's public speed-test endpoints. Runs only when the user asks for it (tray, notch card,
//! settings button, `codenotch.exe speedtest`), never on a timer, and sends nothing but a plain User-Agent and zero-filled test data.
//! Endpoints: GET https://speed.cloudflare.com/__down?bytes=N (N zero bytes) and POST .../__up (body discarded)
//! Plan (same shape as the well-known web tests, bits per second, 1 Mb = 1e6 bits):
//!   - latency: 1 warm-up + 5 timed `bytes=0` requests on one reused connection, median in ms
//!   - download: 4 parallel streams of 25 MB requests, until 10 s or 400 MB
//!   - upload: 3 parallel streams of 4 MB POSTs with an explicit Content-Length, until 8 s or 100 MB
//!   - throughput = bytes/time after a ~1 s warm-up (TCP slow start would otherwise drag the average down)
//!
//! The run is capped at 40 s and a worker stuck in a socket read is cut by the per-request timeout, so nothing hangs the app.
//! Tauri-free on purpose: the module is tested on its own and the CLI reuses the exact same code path as the notch.

use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const BASE: &str = "https://speed.cloudflare.com";
const SERVER: &str = "Cloudflare";
const USER_AGENT: &str = "Codenotch-speedtest";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const IO_TIMEOUT: Duration = Duration::from_secs(10);
const RUN_CAP: Duration = Duration::from_secs(40);
/// A worker whose request outlives its phase is cut this long after the phase ends
const GRACE: Duration = Duration::from_secs(2);
/// Sampling and event cadence while a phase runs
const TICK: Duration = Duration::from_millis(250);
const LIVE_WINDOW_S: f64 = 1.0;
const LATENCY_PINGS: usize = 5;
const LATENCY_CAP: Duration = Duration::from_secs(8);
const LATENCY_REQ_TIMEOUT: Duration = Duration::from_secs(5);
const DOWN_REQ_BYTES: u64 = 25_000_000;
const DOWN_STREAMS: usize = 4;
const UP_REQ_BYTES: u64 = 4_000_000;
const UP_STREAMS: usize = 3;
const CHUNK: usize = 64 * 1024;
/// A phase that died sooner than this has measured nothing worth reporting
const MIN_MEASURE_S: f64 = 2.0;
/// A phase that moved nothing during its last stretch had its link die: the average over that tail is not a speed
const STALL_WINDOW_S: f64 = 2.0;

#[derive(Clone, Copy)]
struct Plan {
    warmup_s: f64,
    down_s: f64,
    down_cap: u64,
    up_s: f64,
    up_cap: u64,
}

const PLAN: Plan = Plan { warmup_s: 1.0, down_s: 10.0, down_cap: 400_000_000, up_s: 8.0, up_cap: 100_000_000 };

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SpeedResult {
    pub at: u64,
    pub down_bits_s: f64,
    pub up_bits_s: f64,
    pub latency_ms: f64,
    pub server: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SpeedState {
    /// "idle" | "running" | "done" | "error"
    pub state: String,
    /// "" | "latency" | "download" | "upload"
    pub phase: String,
    pub progress: f64,
    pub live_bits_s: Option<f64>,
    pub latency_ms: Option<f64>,
    pub down_bits_s: Option<f64>,
    pub up_bits_s: Option<f64>,
    pub last: Option<SpeedResult>,
    pub error: Option<String>,
}

// Manual: a derived Default would give state "" instead of "idle"
impl Default for SpeedState {
    fn default() -> Self {
        SpeedState {
            state: "idle".into(),
            phase: String::new(),
            progress: 0.0,
            live_bits_s: None,
            latency_ms: None,
            down_bits_s: None,
            up_bits_s: None,
            last: None,
            error: None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Phase {
    Latency,
    Download,
    Upload,
}

impl Phase {
    fn name(self) -> &'static str {
        match self {
            Phase::Latency => "latency",
            Phase::Download => "download",
            Phase::Upload => "upload",
        }
    }

    /// Maps how far through its own phase the run is (0..1) onto the whole run: latency 0..0.1, download 0.1..0.55, upload 0.55..1
    fn progress(self, frac: f64) -> f64 {
        let f = if frac.is_finite() { frac.clamp(0.0, 1.0) } else { 0.0 };
        match self {
            Phase::Latency => 0.1 * f,
            Phase::Download => 0.1 + 0.45 * f,
            Phase::Upload => 0.55 + 0.45 * f,
        }
    }
}

static STATE: Mutex<Option<SpeedState>> = Mutex::new(None);
static STORE: Mutex<Option<PathBuf>> = Mutex::new(None);
static RUNNING: AtomicBool = AtomicBool::new(false);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Current state (a copy). `last` is whatever `init` loaded from disk
pub fn state() -> SpeedState {
    lock(&STATE).clone().unwrap_or_default()
}

/// Sets where the last completed result lives (speedtest.json) and loads it, replacing any `last` held in memory
pub fn init(store: PathBuf) {
    let last = load_result(&store);
    *lock(&STORE) = Some(store);
    let mut st = lock(&STATE);
    let mut s = st.take().unwrap_or_default();
    s.last = last;
    *st = Some(s);
}

/// Starts a run on its own thread. False when one is already running
pub fn start(on_event: impl Fn(&SpeedState) + Send + Sync + 'static) -> bool {
    let Some(guard) = RunGuard::take() else { return false };
    let spawned = std::thread::Builder::new().name("speedtest".into()).spawn(move || {
        let _ = execute(guard, BASE, PLAN, &on_event);
    });
    // The closure (and with it the guard) is dropped when the spawn fails, which frees the slot again
    spawned.is_ok()
}

/// Same run as `start`, on the calling thread (the CLI)
pub fn run_blocking(on_event: &dyn Fn(&SpeedState)) -> Result<SpeedResult, String> {
    let guard = RunGuard::take().ok_or_else(|| "A speed test is already running".to_string())?;
    execute(guard, BASE, PLAN, on_event)
}

/// Owning it is the right to run; dropping it (also while unwinding) lets the next run start
struct RunGuard;

impl RunGuard {
    fn take() -> Option<RunGuard> {
        RUNNING.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).ok().map(|_| RunGuard)
    }
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        RUNNING.store(false, Ordering::Release);
    }
}

/// Mutates the shared state and reports the new copy. The callback runs outside the lock: it may re-enter `state()`
fn publish(on_event: &dyn Fn(&SpeedState), mutate: impl FnOnce(&mut SpeedState)) {
    let snap = {
        let mut st = lock(&STATE);
        let s = st.get_or_insert_with(SpeedState::default);
        mutate(s);
        s.clone()
    };
    on_event(&snap);
}

fn execute(_guard: RunGuard, base: &str, plan: Plan, on_event: &dyn Fn(&SpeedState)) -> Result<SpeedResult, String> {
    publish(on_event, |s| {
        // `last` stays: a run that fails or is still going must not erase the previous result
        let last = s.last.take();
        *s = SpeedState { state: "running".into(), phase: Phase::Latency.name().into(), last, ..Default::default() };
    });
    // A panic anywhere below would leave the UI on "running" forever
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| measure(base, plan, on_event)))
        .unwrap_or_else(|_| Err("Speed test failed unexpectedly".to_string()));
    match outcome {
        Ok(result) => {
            if let Some(path) = lock(&STORE).clone() {
                save_result(&path, &result);
            }
            publish(on_event, |s| {
                s.state = "done".into();
                s.phase.clear();
                s.progress = 1.0;
                s.live_bits_s = None;
                s.last = Some(result.clone());
            });
            Ok(result)
        }
        Err(msg) => {
            publish(on_event, |s| {
                s.state = "error".into();
                s.phase.clear();
                s.live_bits_s = None;
                s.error = Some(msg.clone());
            });
            Err(msg)
        }
    }
}

fn measure(base: &str, plan: Plan, on_event: &dyn Fn(&SpeedState)) -> Result<SpeedResult, String> {
    let run = Run {
        base,
        host: host_of(base),
        plan,
        started: Instant::now(),
        // One agent for every request: its pool is what keeps the warmed-up connections between requests
        // No redirects: the endpoints never redirect, and a captive portal or a hijacked answer must not send the test to another host
        agent: ureq::AgentBuilder::new()
            .redirects(0)
            .timeout_connect(CONNECT_TIMEOUT)
            .timeout_read(IO_TIMEOUT)
            .timeout_write(IO_TIMEOUT)
            .user_agent(USER_AGENT)
            .max_idle_connections(DOWN_STREAMS + UP_STREAMS)
            .max_idle_connections_per_host(DOWN_STREAMS + UP_STREAMS)
            .build(),
        on_event,
    };
    // Rounded so the value read back from speedtest.json is the very same f64 (serde_json parses long fractions 1 ulp off) and the UI never gets 15 digits
    let latency_ms = (run.latency()? * 100.0).round() / 100.0;
    publish(on_event, |s| s.latency_ms = Some(latency_ms));
    let down_bits_s = run.throughput(Phase::Download, DOWN_STREAMS)?.round();
    publish(on_event, |s| s.down_bits_s = Some(down_bits_s));
    let up_bits_s = run.throughput(Phase::Upload, UP_STREAMS)?.round();
    publish(on_event, |s| s.up_bits_s = Some(up_bits_s));
    Ok(SpeedResult { at: now_ms(), down_bits_s, up_bits_s, latency_ms, server: SERVER.into() })
}

struct Run<'a> {
    base: &'a str,
    host: &'a str,
    plan: Plan,
    started: Instant,
    agent: ureq::Agent,
    on_event: &'a dyn Fn(&SpeedState),
}

impl Run<'_> {
    fn begin(&self, phase: Phase) {
        publish(self.on_event, |s| {
            s.phase = phase.name().into();
            s.progress = phase.progress(0.0);
            s.live_bits_s = None;
        });
    }

    fn latency(&self) -> Result<f64, String> {
        self.begin(Phase::Latency);
        let url = format!("{}/__down?bytes=0", self.base);
        let mut times = Vec::with_capacity(LATENCY_PINGS);
        for i in 0..=LATENCY_PINGS {
            if i > 0 && self.started.elapsed() > LATENCY_CAP {
                break;
            }
            let t = Instant::now();
            let sent = self.agent.get(&url).timeout(LATENCY_REQ_TIMEOUT).call();
            let read = sent.map_err(|e| describe(self.host, &e)).and_then(|r| {
                check_status(self.host, &r)?;
                let server_ms = server_time_ms(&r);
                // The body must be drained or the connection is not handed back to the pool and the next ping pays a new handshake
                r.into_reader().read_to_end(&mut Vec::new()).map(|_| server_ms).map_err(|e| describe_io(self.host, &e))
            });
            match read {
                // Ping 0 is the warm-up: it carries DNS, TCP and TLS setup and is not timed
                Ok(server_ms) if i > 0 => times.push((t.elapsed().as_secs_f64() * 1000.0 - server_ms).max(0.0)),
                Ok(_) => {}
                Err(msg) if i == 0 => return Err(msg),
                Err(_) => {}
            }
            let frac = (i + 1) as f64 / (LATENCY_PINGS + 1) as f64;
            publish(self.on_event, |s| s.progress = Phase::Latency.progress(frac));
        }
        median(&mut times).ok_or_else(|| "Latency measurement failed".to_string())
    }

    /// Runs the download or upload streams and returns the steady-state rate in bits/s
    fn throughput(&self, phase: Phase, streams: usize) -> Result<f64, String> {
        let (planned, cap) = match phase {
            Phase::Download => (self.plan.down_s, self.plan.down_cap),
            _ => (self.plan.up_s, self.plan.up_cap),
        };
        // Whatever is left of the 40 s (minus the time workers get to wind down) is all this phase may take
        let room = RUN_CAP.saturating_sub(self.started.elapsed() + GRACE).as_secs_f64();
        let planned = planned.min(room);
        if planned < self.plan.warmup_s + MIN_MEASURE_S / 2.0 {
            return Err("Speed test took too long".to_string());
        }
        self.begin(phase);

        let t0 = Instant::now();
        let shared = Arc::new(Shared {
            bytes: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            alive: AtomicUsize::new(0),
            error: Mutex::new(None),
            deadline: t0 + Duration::from_secs_f64(planned),
        });
        for _ in 0..streams {
            shared.alive.fetch_add(1, Ordering::AcqRel);
            let (mine, agent) = (shared.clone(), self.agent.clone());
            let (base, host) = (self.base.to_string(), self.host.to_string());
            let spawned = std::thread::Builder::new().name("speedtest-worker".into()).spawn(move || {
                let _alive = Alive(mine.clone());
                match phase {
                    Phase::Download => download_worker(&agent, &base, &host, &mine),
                    _ => upload_worker(&agent, &base, &host, &mine),
                }
            });
            if spawned.is_err() {
                shared.alive.fetch_sub(1, Ordering::AcqRel);
            }
        }

        let mut samples = vec![(0.0, 0u64)];
        loop {
            std::thread::sleep(TICK);
            let t = t0.elapsed().as_secs_f64();
            let bytes = shared.bytes.load(Ordering::Relaxed);
            samples.push((t, bytes));
            let over = phase_done(t, planned, bytes, cap);
            let frac = (t / planned).max(bytes as f64 / cap as f64);
            let live = window_rate(&samples, LIVE_WINDOW_S);
            publish(self.on_event, |s| {
                s.progress = phase.progress(frac);
                s.live_bits_s = live;
            });
            if over || shared.alive.load(Ordering::Acquire) == 0 {
                break;
            }
        }
        shared.stop.store(true, Ordering::Release);
        // Bounded: a worker in a blocked DNS lookup cannot be interrupted, so it is left to finish on its own
        let wind_down = Instant::now();
        while shared.alive.load(Ordering::Acquire) > 0 && wind_down.elapsed() < GRACE {
            std::thread::sleep(Duration::from_millis(20));
        }

        let error = lock(&shared.error).clone();
        let elapsed = samples.last().map_or(0.0, |s| s.0);
        let bytes = samples.last().map_or(0, |s| s.1);
        if bytes == 0 || (error.is_some() && elapsed < MIN_MEASURE_S) {
            return Err(error.unwrap_or_else(|| format!("No {} data received", phase.name())));
        }
        // A link that died mid-phase must not be reported as a (slow, or zero) speed, nor overwrite the last good result
        if stalled(&samples, self.plan.warmup_s) {
            return Err(format!("The {} stalled", phase.name()));
        }
        steady_rate(&samples, self.plan.warmup_s).ok_or_else(|| format!("{} measurement failed", phase.name()))
    }
}

/// What the workers of one phase share with the sampling loop
struct Shared {
    bytes: AtomicU64,
    stop: AtomicBool,
    alive: AtomicUsize,
    /// First failure of a worker that was not asked to stop
    error: Mutex<Option<String>>,
    deadline: Instant,
}

impl Shared {
    fn fail(&self, msg: String) {
        if !self.stop.load(Ordering::Acquire) {
            lock(&self.error).get_or_insert(msg);
        }
    }

    /// Overall timeout for one request: it can last until the phase ends plus the wind-down allowance, and not a second more
    fn request_timeout(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now()) + GRACE
    }
}

struct Alive(Arc<Shared>);

impl Drop for Alive {
    fn drop(&mut self) {
        self.0.alive.fetch_sub(1, Ordering::AcqRel);
    }
}

fn download_worker(agent: &ureq::Agent, base: &str, host: &str, shared: &Shared) {
    let url = format!("{base}/__down?bytes={DOWN_REQ_BYTES}");
    let mut buf = vec![0u8; CHUNK];
    while !shared.stop.load(Ordering::Acquire) {
        let resp = match agent.get(&url).timeout(shared.request_timeout()).call() {
            Ok(r) => r,
            Err(e) => return shared.fail(describe(host, &e)),
        };
        if let Err(msg) = check_status(host, &resp) {
            return shared.fail(msg);
        }
        let mut body = resp.into_reader();
        let mut got = 0u64;
        loop {
            if shared.stop.load(Ordering::Acquire) {
                return;
            }
            match body.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    got += n as u64;
                    shared.bytes.fetch_add(n as u64, Ordering::Relaxed);
                }
                Err(e) => return shared.fail(describe_io(host, &e)),
            }
        }
        // An answer shorter than asked for (an empty 200, a portal's page) is not test data, and asking again at once would only hammer the host
        if got < DOWN_REQ_BYTES {
            return shared.fail(format!("{host} sent an incomplete reply"));
        }
    }
}

fn upload_worker(agent: &ureq::Agent, base: &str, host: &str, shared: &Shared) {
    let url = format!("{base}/__up");
    let length = UP_REQ_BYTES.to_string();
    while !shared.stop.load(Ordering::Acquire) {
        let body = ZeroBody { left: UP_REQ_BYTES, counter: &shared.bytes, stop: &shared.stop };
        // The header is explicit so the body goes out with a Content-Length instead of chunked encoding
        let sent = agent.post(&url).set("Content-Length", &length).timeout(shared.request_timeout()).send(body);
        match sent {
            Ok(r) => {
                if let Err(msg) = check_status(host, &r) {
                    return shared.fail(msg);
                }
                // The reply is tiny; draining it keeps the connection reusable
                let _ = r.into_reader().read_to_end(&mut Vec::new());
            }
            Err(_) if shared.stop.load(Ordering::Acquire) => return,
            Err(e) => return shared.fail(describe(host, &e)),
        }
    }
}

/// Request body of `left` zero bytes that counts what the HTTP client pulls out of it. It is the upload meter: the bytes handed
/// to the socket are the bytes on their way. Stops early, with an error the caller ignores, when the phase is over
struct ZeroBody<'a> {
    left: u64,
    counter: &'a AtomicU64,
    stop: &'a AtomicBool,
}

impl Read for ZeroBody<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.stop.load(Ordering::Acquire) {
            return Err(std::io::Error::other("speed test stopped"));
        }
        let n = buf.len().min(usize::try_from(self.left).unwrap_or(usize::MAX));
        buf[..n].fill(0);
        self.left -= n as u64;
        self.counter.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

fn host_of(base: &str) -> &str {
    let rest = base.split_once("://").map_or(base, |(_, r)| r);
    rest.split('/').next().unwrap_or(rest)
}

/// Short, human-readable reason. No URL and no OS error text: it ends up in the notch card
fn describe(host: &str, e: &ureq::Error) -> String {
    match e {
        ureq::Error::Status(code, _) => format!("{host} answered HTTP {code}"),
        ureq::Error::Transport(t) if matches!(t.kind(), ureq::ErrorKind::Dns | ureq::ErrorKind::ConnectionFailed) => {
            format!("No connection to {host}")
        }
        ureq::Error::Transport(t) if is_timeout(t) => format!("Connection to {host} timed out"),
        ureq::Error::Transport(t) => format!("Network error talking to {host} ({})", t.kind()),
    }
}

/// With redirects off, a 3xx comes back as an ordinary response instead of an error, so success has to be checked
fn check_status(host: &str, r: &ureq::Response) -> Result<(), String> {
    match r.status() {
        200..=299 => Ok(()),
        code => Err(format!("{host} answered HTTP {code}")),
    }
}

/// Milliseconds Cloudflare spent producing the answer (`Server-Timing: cfSpeedEdge;dur=8, cfSpeedWorker;dur=44`).
/// It is in every timed request, tens to hundreds of ms against a ping of 5-10 ms, and is no part of the network's latency
fn server_time_ms(r: &ureq::Response) -> f64 {
    parse_server_time(&r.all("server-timing"))
}

fn parse_server_time(headers: &[&str]) -> f64 {
    headers
        .iter()
        .flat_map(|h| h.split(','))
        .filter_map(|metric| {
            let mut parts = metric.split(';').map(str::trim);
            parts.next().filter(|name| name.starts_with("cfSpeed"))?;
            parts.find_map(|p| p.strip_prefix("dur=")?.parse::<f64>().ok())
        })
        .filter(|d| d.is_finite() && *d >= 0.0)
        .sum()
}

fn describe_io(host: &str, e: &std::io::Error) -> String {
    if is_timeout(e) {
        format!("Connection to {host} timed out")
    } else {
        format!("Connection to {host} was interrupted")
    }
}

fn is_timeout(e: &(dyn std::error::Error + 'static)) -> bool {
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(err) = cur {
        if let Some(io) = err.downcast_ref::<std::io::Error>() {
            if matches!(io.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock) {
                return true;
            }
        }
        cur = err.source();
    }
    false
}

fn median(values: &mut [f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| a.total_cmp(b));
    let mid = values.len() / 2;
    Some(if values.len() % 2 == 1 { values[mid] } else { (values[mid - 1] + values[mid]) / 2.0 })
}

/// Throughput in bits/s from (seconds, cumulative bytes) samples, ignoring the first `warmup_s` seconds (TCP slow start).
/// A run that ended before there was a meaningful stretch after the warm-up falls back to the whole series
fn steady_rate(samples: &[(f64, u64)], warmup_s: f64) -> Option<f64> {
    let last = *samples.last()?;
    let from = samples.iter().find(|s| s.0 >= warmup_s).filter(|s| last.0 - s.0 >= 0.5).unwrap_or(&samples[0]);
    rate_bits(*from, last)
}

/// Rate over roughly the last `window_s` seconds: what the live readout shows
fn window_rate(samples: &[(f64, u64)], window_s: f64) -> Option<f64> {
    let last = *samples.last()?;
    let from = samples.iter().find(|s| s.0 >= last.0 - window_s).unwrap_or(&samples[0]);
    rate_bits(*from, last)
}

/// True when nothing moved during the last stretch of the phase: up to `STALL_WINDOW_S`, and never reaching back into the warm-up
fn stalled(samples: &[(f64, u64)], warmup_s: f64) -> bool {
    let Some(&(end, _)) = samples.last() else { return false };
    window_rate(samples, STALL_WINDOW_S.min((end - warmup_s).max(0.0))) == Some(0.0)
}

fn rate_bits(from: (f64, u64), to: (f64, u64)) -> Option<f64> {
    let dt = to.0 - from.0;
    if dt <= 0.0 {
        return None;
    }
    Some(to.1.saturating_sub(from.1) as f64 * 8.0 / dt)
}

/// A phase ends at its deadline or once it has moved its byte budget, whichever comes first
fn phase_done(elapsed_s: f64, planned_s: f64, bytes: u64, cap: u64) -> bool {
    elapsed_s >= planned_s || bytes >= cap
}

fn load_result(path: &Path) -> Option<SpeedResult> {
    let r: SpeedResult = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    // A hand-edited file must not put NaN-ish or negative numbers on screen
    let sane = |v: f64| v.is_finite() && v >= 0.0;
    (sane(r.down_bits_s) && sane(r.up_bits_s) && sane(r.latency_ms)).then_some(r)
}

fn save_result(path: &Path, result: &SpeedResult) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(txt) = serde_json::to_string_pretty(result) {
        let _ = std::fs::write(path, txt);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    /// The run state is process-wide; tests that drive `execute` take turns
    static SERIAL: Mutex<()> = Mutex::new(());

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("codenotch-speedtest-{tag}-{}-{}", std::process::id(), now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample_result() -> SpeedResult {
        SpeedResult { at: 1_790_600_000_000, down_bits_s: 312e6, up_bits_s: 48e6, latency_ms: 12.3, server: "Cloudflare".into() }
    }

    #[test]
    fn median_odd_even_empty() {
        assert_eq!(median(&mut []), None);
        assert_eq!(median(&mut [5.0]), Some(5.0));
        assert_eq!(median(&mut [30.0, 10.0, 20.0]), Some(20.0));
        assert_eq!(median(&mut [40.0, 10.0, 20.0, 30.0]), Some(25.0));
        // one slow outlier does not move it
        assert_eq!(median(&mut [12.0, 11.0, 250.0, 13.0, 12.0]), Some(12.0));
    }

    #[test]
    fn steady_rate_skips_the_warmup() {
        // 1 MB/s during the first second (slow start), then a steady 10 MB/s
        let mut s = vec![(0.0, 0u64)];
        for i in 1..=20 {
            let t = i as f64 * 0.25;
            let bytes = if t <= 1.0 { (t * 1e6) as u64 } else { 1_000_000 + ((t - 1.0) * 10e6) as u64 };
            s.push((t, bytes));
        }
        let steady = steady_rate(&s, 1.0).unwrap();
        assert!((steady - 80e6).abs() < 1e3, "{steady}");
        let naive = rate_bits(s[0], *s.last().unwrap()).unwrap();
        assert!(naive < steady);
    }

    #[test]
    fn steady_rate_falls_back_when_the_run_was_too_short() {
        // the byte cap ended the phase 0.25 s after the warm-up: use the whole series instead of a 0.25 s sliver
        let s = [(0.0, 0u64), (0.5, 500_000), (1.0, 1_000_000), (1.25, 1_250_000)];
        assert_eq!(steady_rate(&s, 1.0), Some(8e6));
        assert_eq!(steady_rate(&[], 1.0), None);
        assert_eq!(steady_rate(&[(0.0, 0)], 1.0), None);
        // no progress at all is a legitimate zero, not a missing value
        assert_eq!(steady_rate(&[(0.0, 0), (3.0, 0)], 1.0), Some(0.0));
    }

    #[test]
    fn window_rate_looks_at_the_last_second_only() {
        let s = [(0.0, 0u64), (1.0, 1_000_000), (2.0, 2_000_000), (3.0, 12_000_000)];
        // last second: 10 MB in 1 s
        assert_eq!(window_rate(&s, 1.0), Some(80e6));
        // fewer samples than the window: everything so far
        assert_eq!(window_rate(&s[..2], 5.0), Some(8e6));
        assert_eq!(window_rate(&[(0.0, 0)], 1.0), None);
    }

    #[test]
    fn a_flat_tail_is_a_stall() {
        let series = |until: f64, end: f64| -> Vec<(f64, u64)> {
            (0..=(end * 4.0) as u32).map(|i| (i as f64 * 0.25, (i as f64 * 0.25).min(until) as u64 * 1_000_000)).collect()
        };
        // moving to the end, or stopped only in the last half second: fine
        assert!(!stalled(&series(10.0, 10.0), 1.0));
        assert!(!stalled(&series(9.5, 10.0), 1.0));
        // dead for the last 2 s, or since right after the warm-up: a stall, whatever the average says
        assert!(stalled(&series(4.0, 10.0), 1.0));
        assert!(stalled(&series(1.0, 3.0), 1.0));
        // a phase that ended inside its warm-up has nothing to judge
        assert!(!stalled(&series(0.5, 1.0), 1.0));
        assert!(!stalled(&[], 1.0));
        assert!(!stalled(&[(0.0, 0)], 1.0));
    }

    #[test]
    fn counter_going_backwards_never_yields_a_negative_rate() {
        assert_eq!(rate_bits((1.0, 500), (2.0, 100)), Some(0.0));
        assert_eq!(rate_bits((1.0, 500), (1.0, 900)), None);
    }

    #[test]
    fn progress_maps_each_phase_onto_its_slice() {
        assert_eq!(Phase::Latency.progress(0.0), 0.0);
        assert!((Phase::Latency.progress(1.0) - 0.1).abs() < 1e-12);
        assert!((Phase::Download.progress(0.0) - 0.1).abs() < 1e-12);
        assert!((Phase::Download.progress(1.0) - 0.55).abs() < 1e-12);
        assert!((Phase::Upload.progress(0.0) - 0.55).abs() < 1e-12);
        assert!((Phase::Upload.progress(1.0) - 1.0).abs() < 1e-12);
        assert!((Phase::Download.progress(0.5) - 0.325).abs() < 1e-12);
        // out of range and garbage input stay inside the slice
        assert!((Phase::Upload.progress(7.0) - 1.0).abs() < 1e-12);
        assert!((Phase::Download.progress(-1.0) - 0.1).abs() < 1e-12);
        assert!((Phase::Download.progress(f64::NAN) - 0.1).abs() < 1e-12);
    }

    #[test]
    fn a_phase_ends_at_its_deadline_or_its_byte_cap() {
        assert!(!phase_done(3.0, 10.0, 1_000, 400_000_000));
        assert!(phase_done(10.0, 10.0, 1_000, 400_000_000));
        assert!(phase_done(10.4, 10.0, 0, 400_000_000));
        assert!(phase_done(2.0, 10.0, 400_000_000, 400_000_000));
        assert!(phase_done(2.0, 10.0, 500_000_000, 400_000_000));
    }

    #[test]
    fn zero_body_counts_what_is_read_and_honours_its_limit() {
        let counter = AtomicU64::new(0);
        let stop = AtomicBool::new(false);
        let mut body = ZeroBody { left: 100_000, counter: &counter, stop: &stop };
        let mut buf = vec![0xAAu8; 30_000];
        let mut total = 0;
        loop {
            let n = body.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            assert!(buf[..n].iter().all(|b| *b == 0));
            total += n;
        }
        assert_eq!(total, 100_000);
        assert_eq!(counter.load(Ordering::Relaxed), 100_000);
        // exhausted stays exhausted
        assert_eq!(body.read(&mut buf).unwrap(), 0);
        assert_eq!(counter.load(Ordering::Relaxed), 100_000);
    }

    #[test]
    fn zero_body_stops_when_the_phase_is_over() {
        let counter = AtomicU64::new(0);
        let stop = AtomicBool::new(false);
        let mut body = ZeroBody { left: 1_000_000, counter: &counter, stop: &stop };
        let mut buf = [0u8; 1024];
        assert_eq!(body.read(&mut buf).unwrap(), 1024);
        stop.store(true, Ordering::Release);
        assert!(body.read(&mut buf).is_err());
        assert_eq!(counter.load(Ordering::Relaxed), 1024);
    }

    #[test]
    fn result_survives_a_round_trip_through_disk() {
        let dir = temp_dir("roundtrip");
        // parent directory that does not exist yet is created
        let path = dir.join("nested").join("speedtest.json");
        assert_eq!(load_result(&path), None);
        save_result(&path, &sample_result());
        assert_eq!(load_result(&path), Some(sample_result()));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn corrupt_or_odd_files_load_as_none() {
        let dir = temp_dir("corrupt");
        let path = dir.join("speedtest.json");
        for junk in ["", "not json", "{", "[]", "{\"at\":1}", "{\"at\":\"x\",\"down_bits_s\":1,\"up_bits_s\":1,\"latency_ms\":1,\"server\":\"c\"}"] {
            std::fs::write(&path, junk).unwrap();
            assert_eq!(load_result(&path), None, "{junk:?}");
        }
        std::fs::write(&path, [0xFF, 0xFE, 0x00, 0x9F]).unwrap();
        assert_eq!(load_result(&path), None);
        std::fs::write(&path, r#"{"at":1,"down_bits_s":-5.0,"up_bits_s":1.0,"latency_ms":1.0,"server":"c"}"#).unwrap();
        assert_eq!(load_result(&path), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn state_json_has_the_contract_keys() {
        let v = serde_json::to_value(SpeedState::default()).unwrap();
        let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["down_bits_s", "error", "last", "latency_ms", "live_bits_s", "phase", "progress", "state", "up_bits_s"]
        );
        assert_eq!(v["state"], "idle");
        assert_eq!(v["phase"], "");
        assert!(v["last"].is_null() && v["error"].is_null() && v["live_bits_s"].is_null());

        let s = SpeedState { last: Some(sample_result()), ..Default::default() };
        let v = serde_json::to_value(s).unwrap();
        let mut keys: Vec<&str> = v["last"].as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["at", "down_bits_s", "latency_ms", "server", "up_bits_s"]);
    }

    #[test]
    fn host_is_pulled_out_of_the_base_url() {
        assert_eq!(host_of("https://speed.cloudflare.com"), "speed.cloudflare.com");
        assert_eq!(host_of("http://127.0.0.1:8080/x"), "127.0.0.1:8080");
        assert_eq!(host_of("plain"), "plain");
    }

    #[test]
    fn only_one_run_at_a_time() {
        let _turn = lock(&SERIAL);
        let a = RunGuard::take();
        assert!(a.is_some());
        assert!(RunGuard::take().is_none());
        assert!(!start(|_| {}));
        assert_eq!(run_blocking(&|_| {}).unwrap_err(), "A speed test is already running");
        drop(a);
        assert!(RunGuard::take().is_some());
    }

    /// Minimal Cloudflare look-alike on loopback: GET /__down?bytes=N answers N zeros, POST /__up swallows its body.
    /// Returns the base URL and a counter of the bytes received by /__up
    fn fake_server() -> (String, Arc<AtomicU64>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let received = Arc::new(AtomicU64::new(0));
        let up = received.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                let up = up.clone();
                std::thread::spawn(move || {
                    let _ = serve(conn, &up);
                });
            }
        });
        (base, received)
    }

    fn serve(conn: std::net::TcpStream, up: &AtomicU64) -> std::io::Result<()> {
        let mut out = conn.try_clone()?;
        let mut input = BufReader::new(conn);
        loop {
            let mut line = String::new();
            if input.read_line(&mut line)? == 0 {
                return Ok(());
            }
            let request = line.trim().to_string();
            let mut length = 0u64;
            loop {
                line.clear();
                input.read_line(&mut line)?;
                if line.trim().is_empty() {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap_or(0);
                }
            }
            if request.starts_with("POST") {
                let mut left = length;
                let mut sink = vec![0u8; CHUNK];
                while left > 0 {
                    let n = input.read(&mut sink[..(left as usize).min(CHUNK)])?;
                    if n == 0 {
                        return Ok(());
                    }
                    up.fetch_add(n as u64, Ordering::Relaxed);
                    left -= n as u64;
                }
                out.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")?;
            } else {
                let n: u64 = request.split("bytes=").nth(1).and_then(|s| s.split(' ').next()).and_then(|s| s.parse().ok()).unwrap_or(0);
                out.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {n}\r\n\r\n").as_bytes())?;
                let zeros = vec![0u8; CHUNK];
                let mut left = n;
                while left > 0 {
                    let k = (left as usize).min(CHUNK);
                    out.write_all(&zeros[..k])?;
                    left -= k as u64;
                }
            }
        }
    }

    /// Short phases so the whole pipeline runs in a few seconds
    const FAST: Plan = Plan { warmup_s: 0.3, down_s: 1.5, down_cap: 300_000_000, up_s: 1.5, up_cap: 100_000_000 };

    #[test]
    fn full_run_against_a_local_server() {
        let _turn = lock(&SERIAL);
        let dir = temp_dir("local");
        let store = dir.join("speedtest.json");
        init(store.clone());
        assert_eq!(state().last, None);
        let (base, received) = fake_server();

        let events = Mutex::new(Vec::<SpeedState>::new());
        let result = execute(RunGuard::take().unwrap(), &base, FAST, &|s| lock(&events).push(s.clone())).expect("run");

        assert!(result.down_bits_s > 1e6 && result.up_bits_s > 1e6, "{result:?}");
        assert!(result.latency_ms > 0.0 && result.latency_ms < 500.0, "{result:?}");
        assert_eq!(result.server, "Cloudflare");
        assert!(received.load(Ordering::Relaxed) > 0);

        let events = lock(&events);
        assert_eq!(events.first().unwrap().state, "running");
        let end = events.last().unwrap();
        assert_eq!((end.state.as_str(), end.phase.as_str(), end.progress), ("done", "", 1.0));
        assert_eq!(end.last.as_ref(), Some(&result));
        assert!(end.live_bits_s.is_none() && end.error.is_none());
        // progress never goes backwards and every phase shows up in order
        assert!(events.windows(2).all(|w| w[1].progress >= w[0].progress));
        let phases: Vec<&str> = events.iter().map(|s| s.phase.as_str()).filter(|p| !p.is_empty()).collect();
        let (l, d, u) = (
            phases.iter().position(|p| *p == "latency").unwrap(),
            phases.iter().position(|p| *p == "download").unwrap(),
            phases.iter().position(|p| *p == "upload").unwrap(),
        );
        assert!(l < d && d < u);
        assert!(events.iter().any(|s| s.phase == "download" && s.live_bits_s.is_some()));
        // results fill in progressively
        assert!(events.iter().any(|s| s.phase == "download" && s.latency_ms.is_some() && s.down_bits_s.is_none()));

        // persisted, and a fresh init (a restart) sees it
        assert_eq!(load_result(&store), Some(result.clone()));
        init(store);
        assert_eq!(state().last, Some(result));
        let _ = std::fs::remove_dir_all(dir);
    }

    fn failing_run(base: &str) -> (Result<SpeedResult, String>, Vec<SpeedState>, Duration) {
        let _turn = lock(&SERIAL);
        let dir = temp_dir("fail");
        let store = dir.join("speedtest.json");
        save_result(&store, &sample_result());
        init(store);
        let events = Mutex::new(Vec::<SpeedState>::new());
        let t = Instant::now();
        let out = execute(RunGuard::take().unwrap(), base, FAST, &|s| lock(&events).push(s.clone()));
        let took = t.elapsed();
        // `last` must be exactly what was on disk before, in memory and in the file
        assert_eq!(state().last, Some(sample_result()));
        assert_eq!(load_result(&lock(&STORE).clone().unwrap()), Some(sample_result()));
        let _ = std::fs::remove_dir_all(dir);
        (out, events.into_inner().unwrap(), took)
    }

    #[test]
    fn unreachable_host_gives_a_short_error_and_keeps_last() {
        let (out, events, took) = failing_run("http://127.0.0.1:1");
        let msg = out.unwrap_err();
        assert!(msg.starts_with("No connection to 127.0.0.1:1"), "{msg}");
        assert!(msg.len() < 60);
        assert!(took < Duration::from_secs(8), "{took:?}");
        let end = events.last().unwrap();
        assert_eq!((end.state.as_str(), end.phase.as_str()), ("error", ""));
        assert_eq!(end.error.as_deref(), Some(msg.as_str()));
        assert_eq!((end.down_bits_s, end.up_bits_s, end.live_bits_s), (None, None, None));
        assert_eq!(end.last, Some(sample_result()));
    }

    #[test]
    fn unresolvable_host_gives_a_short_error_and_keeps_last() {
        // .invalid is reserved (RFC 2606): it can never resolve
        let (out, _, took) = failing_run("https://speedtest.codenotch.invalid");
        assert_eq!(out.unwrap_err(), "No connection to speedtest.codenotch.invalid");
        assert!(took < Duration::from_secs(10), "{took:?}");
    }

    #[test]
    fn server_error_status_is_reported_not_measured() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for mut c in listener.incoming().flatten() {
                std::thread::spawn(move || {
                    let mut b = [0u8; 1024];
                    let _ = c.read(&mut b);
                    let _ = c.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                });
            }
        });
        let (out, ..) = failing_run(&base);
        assert!(out.unwrap_err().ends_with("answered HTTP 503"));
    }

    #[test]
    fn server_time_is_read_from_every_timing_header() {
        assert_eq!(parse_server_time(&["cfSpeedEdge;dur=8, cfSpeedWorker;dur=44"]), 52.0);
        assert_eq!(parse_server_time(&["cfSpeedEdge;dur=8", "cfSpeedWorker;dur=44.5"]), 52.5);
        // other metrics, descriptions without a duration, garbage and negative numbers are ignored
        let noise = [r#"cfL4;desc="?proto=TCP&rtt=6555&x=0""#, "cfSpeedWorker;desc=x", "cfSpeedEdge;dur=abc", "cfSpeedWorker;dur=-3"];
        assert_eq!(parse_server_time(&noise), 0.0);
        assert_eq!(parse_server_time(&[]), 0.0);
    }

    /// A tiny HTTP server for the misbehaviours below. Kinds: "empty" (every answer is an empty 200), "stall" (pings fine, a real
    /// download sends 100 kB and then goes silent), "redirect:<base>" (302 to <base> for everything). Returns the base URL and
    /// the number of requests it has seen
    fn odd_server(kind: String) -> (String, Arc<AtomicU64>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(AtomicU64::new(0));
        let counter = seen.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                let (kind, counter) = (kind.clone(), counter.clone());
                std::thread::spawn(move || {
                    let mut out = conn.try_clone().unwrap();
                    let mut input = BufReader::new(conn);
                    loop {
                        let mut request = String::new();
                        if input.read_line(&mut request).unwrap_or(0) == 0 {
                            return;
                        }
                        loop {
                            let mut line = String::new();
                            if input.read_line(&mut line).unwrap_or(0) == 0 {
                                return;
                            }
                            if line.trim().is_empty() {
                                break;
                            }
                        }
                        counter.fetch_add(1, Ordering::Relaxed);
                        let asked: u64 = request.split("bytes=").nth(1).and_then(|s| s.split(' ').next()).and_then(|s| s.parse().ok()).unwrap_or(0);
                        let reply = match kind.strip_prefix("redirect:") {
                            Some(to) => format!("HTTP/1.1 302 Found\r\nLocation: {to}/__down?bytes=0\r\nContent-Length: 0\r\n\r\n"),
                            None => format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", if kind == "empty" { 0 } else { asked }),
                        };
                        if out.write_all(reply.as_bytes()).is_err() {
                            return;
                        }
                        if kind == "stall" && asked > 0 {
                            let _ = out.write_all(&[0u8; 100_000]);
                            std::thread::sleep(Duration::from_secs(30));
                            return;
                        }
                    }
                });
            }
        });
        (base, seen)
    }

    #[test]
    fn latency_leaves_out_the_time_the_server_spent() {
        let _turn = lock(&SERIAL);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        // Every answer takes 200 ms and says so, the way Cloudflare's speed worker does
        std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                std::thread::spawn(move || {
                    let mut out = conn.try_clone().unwrap();
                    let mut input = BufReader::new(conn);
                    loop {
                        loop {
                            let mut line = String::new();
                            if input.read_line(&mut line).unwrap_or(0) == 0 {
                                return;
                            }
                            if line.trim().is_empty() {
                                break;
                            }
                        }
                        std::thread::sleep(Duration::from_millis(200));
                        let reply = "HTTP/1.1 200 OK\r\nServer-Timing: cfSpeedEdge;dur=1, cfSpeedWorker;dur=199\r\nContent-Length: 0\r\n\r\n";
                        if out.write_all(reply.as_bytes()).is_err() {
                            return;
                        }
                    }
                });
            }
        });
        let run = Run { base: &base, host: host_of(&base), plan: FAST, started: Instant::now(), agent: ureq::AgentBuilder::new().build(), on_event: &|_| {} };
        let ms = run.latency().unwrap();
        assert!(ms < 100.0, "the server's own 200 ms must not count as network latency: {ms}");
    }

    #[test]
    fn a_link_that_dies_mid_phase_is_an_error_not_a_zero_result() {
        let (base, _) = odd_server("stall".into());
        let (out, events, took) = failing_run(&base);
        assert_eq!(out.unwrap_err(), "The download stalled");
        assert!(took < Duration::from_secs(8), "{took:?}");
        let end = events.last().unwrap();
        assert_eq!((end.state.as_str(), end.down_bits_s, end.up_bits_s), ("error", None, None));
    }

    #[test]
    fn an_empty_answer_is_an_error_and_is_not_asked_for_again() {
        let (base, seen) = odd_server("empty".into());
        let (out, _, took) = failing_run(&base);
        assert_eq!(out.unwrap_err(), format!("{} sent an incomplete reply", host_of(&base)));
        assert!(took < Duration::from_secs(5), "{took:?}");
        // 6 pings plus one download request per stream, not a request loop
        assert!(seen.load(Ordering::Relaxed) <= 20, "{} requests", seen.load(Ordering::Relaxed));
    }

    #[test]
    fn a_redirect_is_an_error_and_is_not_followed() {
        let (elsewhere, elsewhere_seen) = odd_server("stall".into());
        let (base, first_seen) = odd_server(format!("redirect:{elsewhere}"));
        let (out, ..) = failing_run(&base);
        assert_eq!(out.unwrap_err(), format!("{} answered HTTP 302", host_of(&base)));
        assert_eq!(elsewhere_seen.load(Ordering::Relaxed), 0);
        assert_eq!(first_seen.load(Ordering::Relaxed), 1);
    }

    /// Real run against Cloudflare: moves up to ~500 MB, so it is opt-in (`cargo test -- --ignored --nocapture`)
    #[test]
    #[ignore]
    fn real_run_against_cloudflare() {
        let _turn = lock(&SERIAL);
        let dir = temp_dir("real");
        init(dir.join("speedtest.json"));
        let started = Instant::now();
        let result = run_blocking(&|s| {
            if s.state != "running" {
                return;
            }
            println!("  {:>8} {:>5.1}%  live {:>10}", s.phase, s.progress * 100.0, s.live_bits_s.map_or("-".into(), |v| format!("{:.1} Mb/s", v / 1e6)));
        })
        .expect("speed test");
        println!(
            "RESULT down {:.1} Mb/s  up {:.1} Mb/s  latency {:.1} ms  in {:.1} s",
            result.down_bits_s / 1e6,
            result.up_bits_s / 1e6,
            result.latency_ms,
            started.elapsed().as_secs_f64()
        );
        assert!(result.down_bits_s > 0.0 && result.up_bits_s > 0.0 && result.latency_ms > 0.0);
        assert!(started.elapsed() < RUN_CAP);
        assert_eq!(state().last, Some(result));
        let _ = std::fs::remove_dir_all(dir);
    }
}
