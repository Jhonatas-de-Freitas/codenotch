//! Start at sign-in: an HKCU\...\Run registry value (per user, no administrator needed).
//! The command carries --silent: wait in the background, show no bar without sessions, appear when one starts.
//! Implemented with reg.exe, so no new dependency.

use std::path::{Path, PathBuf};
use std::process::Command;

const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const NAME: &str = "Codenotch";

/// Where autostart's copy of the app lives: a per-user, admin-free install directory, the same kind
/// of location the signed installer uses. The Run key always points here, never at wherever cargo
/// happened to place today's build, so moving or deleting the build folder cannot break sign-in.
fn install_dir() -> Option<PathBuf> {
    dirs::data_local_dir().map(|d| d.join("Programs").join("Codenotch"))
}

/// Copies `src` to `dst`, creating `dst`'s directory first and overwriting whatever was there.
fn copy_into(src: &Path, dst: &Path) -> std::io::Result<()> {
    if let Some(dir) = dst.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::copy(src, dst)?;
    Ok(())
}

/// Copies the running exe and its hook helper into `dir`, and returns the copy's own path. The hook
/// is optional: hooks_install.rs already tolerates its absence, so a build without it still installs.
fn ensure_installed_to(running_exe: &Path, dir: &Path) -> std::io::Result<PathBuf> {
    let dest_exe = dir.join("codenotch.exe");
    copy_into(running_exe, &dest_exe)?;
    if let Some(src_dir) = running_exe.parent() {
        let hook_src = src_dir.join("codenotch-hook.exe");
        if hook_src.is_file() {
            copy_into(&hook_src, &dir.join("codenotch-hook.exe"))?;
        }
    }
    Ok(dest_exe)
}

fn reg(args: &[&str]) -> Option<(bool, String)> {
    let mut c = Command::new("reg");
    c.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    c.output().ok().map(|o| {
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        );
        (o.status.success(), text)
    })
}

pub fn is_enabled() -> bool {
    reg(&["query", RUN_KEY, "/v", NAME])
        .map(|(ok, out)| ok && out.contains(NAME))
        .unwrap_or(false)
}

pub fn enable() -> Result<String, String> {
    let running = std::env::current_exe().map_err(|e| e.to_string())?;
    let dir = install_dir().ok_or("cannot find the local app data directory")?;
    let exe = ensure_installed_to(&running, &dir).map_err(|e| e.to_string())?;
    let val = format!("\"{}\" --silent", exe.display());
    match reg(&["add", RUN_KEY, "/v", NAME, "/t", "REG_SZ", "/d", &val, "/f"]) {
        Some((true, _)) => Ok(format!("start at sign-in enabled, installed to {}", exe.display())),
        Some((false, out)) => Err(out),
        None => Err("reg.exe failed to run".into()),
    }
}

/// When start-at-sign-in is on and this is a newer build running from somewhere else, brings the
/// installed copy up to date, so a rebuild reaches the next sign-in without toggling the setting.
/// The copy cannot be overwritten while it is itself running; that failure is simply ignored.
pub fn refresh_copy() {
    if !is_enabled() {
        return;
    }
    let (Ok(running), Some(dir)) = (std::env::current_exe(), install_dir()) else { return };
    if running.parent() == Some(dir.as_path()) {
        return;
    }
    let _ = ensure_installed_to(&running, &dir);
}

pub fn disable() -> Result<String, String> {
    match reg(&["delete", RUN_KEY, "/v", NAME, "/f"]) {
        Some((true, _)) => Ok("start at sign-in disabled".into()),
        Some((false, out)) => {
            if out.to_lowercase().contains("unable to find") || out.contains("找不到") { // reg.exe answers in the OS language; "找不到" is the Chinese "unable to find"
                Ok("start at sign-in was not enabled".into())
            } else {
                Err(out)
            }
        }
        None => Err("reg.exe failed to run".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_dir(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("codenotch-autostart-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn copy_into_copies_a_missing_destination() {
        let dir = temp_dir("missing");
        let src = dir.join("src.bin");
        let dst = dir.join("dst.bin");
        std::fs::write(&src, b"hello").unwrap();
        copy_into(&src, &dst).unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), b"hello");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn copy_into_creates_the_destination_directory() {
        let dir = temp_dir("mkdir");
        let src = dir.join("src.bin");
        let dst = dir.join("nested").join("dst.bin");
        std::fs::write(&src, b"hello").unwrap();
        copy_into(&src, &dst).unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), b"hello");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn copy_into_overwrites_an_existing_destination() {
        let dir = temp_dir("overwrite");
        let src = dir.join("src.bin");
        let dst = dir.join("dst.bin");
        std::fs::write(&src, b"new content").unwrap();
        std::fs::write(&dst, b"old").unwrap();
        copy_into(&src, &dst).unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), b"new content");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_installed_to_copies_exe_and_hook_beside_it() {
        let root = temp_dir("install");
        let source = root.join("source");
        std::fs::create_dir_all(&source).unwrap();
        let running = source.join("codenotch.exe");
        std::fs::write(&running, b"main exe").unwrap();
        std::fs::write(source.join("codenotch-hook.exe"), b"hook exe").unwrap();
        let install_dir = root.join("installed");

        let result = ensure_installed_to(&running, &install_dir).unwrap();

        assert_eq!(result, install_dir.join("codenotch.exe"));
        assert_eq!(std::fs::read(&result).unwrap(), b"main exe");
        assert_eq!(std::fs::read(install_dir.join("codenotch-hook.exe")).unwrap(), b"hook exe");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn ensure_installed_to_tolerates_a_missing_hook() {
        let root = temp_dir("install-no-hook");
        let source = root.join("source");
        std::fs::create_dir_all(&source).unwrap();
        let running = source.join("codenotch.exe");
        std::fs::write(&running, b"main exe").unwrap();
        let install_dir = root.join("installed");

        let result = ensure_installed_to(&running, &install_dir).unwrap();

        assert_eq!(std::fs::read(&result).unwrap(), b"main exe");
        assert!(!install_dir.join("codenotch-hook.exe").exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
