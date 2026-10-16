//! codenotch-hook: the minimal client Claude Code's hooks call.
//! Duties: 1) report the event plus stdin JSON to the main app; 2) launch the main app if it is not running.
//! Iron rule: never block Claude Code — ~2 s total budget, and every failure exits 0 silently.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

const DEFAULT_PORT: u16 = 48666;
const MAX_STDIN: u64 = 256 * 1024;

fn main() {
    let event = std::env::args().nth(1).unwrap_or_else(|| "ping".into());

    // The hook's stdin is the JSON Claude Code provides (session_id / cwd / prompt / message…)
    let mut body = String::new();
    let _ = std::io::stdin().take(MAX_STDIN).read_to_string(&mut body);

    let cfg_text = config_text().unwrap_or_default();
    let port = port_from(&cfg_text);
    let secret = secret_from(&cfg_text);
    let ppid = parent_pid();

    if send(port, &event, ppid, &body, secret.as_deref()).is_ok() {
        return;
    }
    // Main app not running: launch it detached, then retry briefly
    spawn_main();
    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(100));
        if send(port, &event, ppid, &body, secret.as_deref()).is_ok() {
            return;
        }
    }
    // Give up quietly — never affect Claude Code
}

/// %APPDATA%\codenotch\config.json, read once per run (hand-rolled scan below, no JSON dependency)
fn config_text() -> Option<String> {
    let path = format!("{}\\codenotch\\config.json", std::env::var("APPDATA").ok()?);
    std::fs::read_to_string(path).ok()
}

/// Pulls "port": N out of the config text
fn port_from(txt: &str) -> u16 {
    let Some(i) = txt.find("\"port\"") else {
        return DEFAULT_PORT;
    };
    let digits: String = txt[i + 6..]
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().unwrap_or(DEFAULT_PORT)
}

/// Pulls "secret": "…" out of the config text — the value the server checks on every /event
fn secret_from(txt: &str) -> Option<String> {
    let key = "\"secret\"";
    let after_key = &txt[txt.find(key)? + key.len()..];
    let after_colon = &after_key[after_key.find(':')? + 1..];
    let value = &after_colon[after_colon.find('"')? + 1..];
    Some(value[..value.find('"')?].to_string())
}

fn send(port: u16, event: &str, ppid: u32, body: &str, secret: Option<&str>) -> std::io::Result<()> {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_millis(300))?;
    s.set_write_timeout(Some(Duration::from_millis(700)))?;
    s.set_read_timeout(Some(Duration::from_millis(700)))?;
    let secret_header = secret.map(|v| format!("X-Codenotch-Secret: {v}\r\n")).unwrap_or_default();
    let req = format!(
        "POST /event?e={}&ppid={} HTTP/1.1\r\nHost: 127.0.0.1\r\n{}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        event,
        ppid,
        secret_header,
        body.len(),
        body
    );
    s.write_all(req.as_bytes())?;
    let mut buf = [0u8; 64];
    let _ = s.read(&mut buf); // wait for a response fragment to confirm delivery; failure does not matter
    Ok(())
}

/// Launches the main app detached: no inherited handles, no window, never waits
fn spawn_main() {
    let Ok(me) = std::env::current_exe() else { return };
    let Some(dir) = me.parent() else { return };
    let exe = dir.join("codenotch.exe");
    if !exe.exists() {
        return;
    }
    let mut cmd = std::process::Command::new(exe);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW);
    }
    let _ = cmd.spawn();
}

/// Parent process PID (≈ the Claude Code CLI process) via NtQueryInformationProcess, no dependency
#[cfg(windows)]
fn parent_pid() -> u32 {
    #[repr(C)]
    struct Pbi {
        exit_status: isize,
        peb: usize,
        affinity_mask: usize,
        base_priority: isize,
        unique_process_id: usize,
        inherited_from_unique_process_id: usize,
    }
    extern "system" {
        fn NtQueryInformationProcess(
            handle: isize,
            class: u32,
            info: *mut Pbi,
            len: u32,
            ret_len: *mut u32,
        ) -> i32;
    }
    unsafe {
        let mut pbi = std::mem::zeroed::<Pbi>();
        let mut ret = 0u32;
        // -1 = GetCurrentProcess()
        if NtQueryInformationProcess(
            -1,
            0,
            &mut pbi,
            std::mem::size_of::<Pbi>() as u32,
            &mut ret,
        ) == 0
        {
            return pbi.inherited_from_unique_process_id as u32;
        }
    }
    0
}

#[cfg(not(windows))]
fn parent_pid() -> u32 {
    std::os::unix::process::parent_id()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_from_finds_the_configured_value() {
        assert_eq!(port_from("{\"port\": 12345}"), 12345);
    }

    #[test]
    fn port_from_falls_back_to_default_without_a_port_key() {
        assert_eq!(port_from("{}"), DEFAULT_PORT);
    }

    #[test]
    fn secret_from_finds_the_configured_value() {
        assert_eq!(secret_from("{\"port\":1,\"secret\":\"abc123\"}").as_deref(), Some("abc123"));
    }

    #[test]
    fn secret_from_is_none_without_a_secret_key() {
        assert_eq!(secret_from("{\"port\":1}"), None);
    }
}
