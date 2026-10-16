use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The Mac's notch sizes, as multiples of the designed size: Small, Medium, Large.
pub const SIZES: [f64; 3] = [0.8, 1.0, 1.25];

/// The nearest of `SIZES`, so a scale saved by the old 40–100 % slider still lands on a size that
/// exists. 0.9, halfway between Small and Medium, counts as Medium.
pub fn snap_scale(scale: f64) -> f64 {
    if scale < 0.9 {
        SIZES[0]
    } else if scale < 1.125 || !scale.is_finite() {
        SIZES[1]
    } else {
        SIZES[2]
    }
}

/// The rows of the notch's System block, in the order the notch draws them.
pub const SYS_METRICS: [&str; 4] = ["cpu", "ram", "disk", "net"];

/// One ring on the notch: which provider. (A `window` key from older builds is ignored.)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TraySlot {
    pub provider: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_port")]
    pub port: u16,
    /// Per-install random value codenotch-hook sends back on every event, so the local server can
    /// tell a real hook from another program that happens to be poking the same loopback port.
    #[serde(default = "random_secret")]
    pub secret: String,
    /// "auto" | "zh" | "en" | "pt" | "ja" | "ko" | "ru" | "uk"
    #[serde(default = "default_lang")]
    pub lang: String,
    #[serde(default)]
    pub bar_x: Option<i32>,
    #[serde(default)]
    pub bar_y: Option<i32>,
    /// Logical width of the bar (wheel-adjustable, 220-520); None = default 360
    #[serde(default)]
    pub bar_w: Option<u32>,
    /// Allow dragging + wheel resizing (tray toggle, off by default to prevent accidental drags)
    #[serde(default)]
    pub drag_enabled: bool,
    /// Position of the notch along its edge: the window centre as a fraction of the monitor's height
    /// (left/right edges) or width (top/bottom edges), 0 = top/left, 1 = bottom/right, default 0.5;
    /// saved after a drag. Named `notch_y` from when the right edge was the only one, so an existing
    /// config keeps its place.
    #[serde(default = "default_notch_y")]
    pub notch_y: f64,
    /// Which screen edge the notch is pinned to: "right" (the default), "left", "top" or "bottom".
    #[serde(default = "default_notch_edge")]
    pub notch_edge: String,
    /// Which monitor the notch lives on, by the system's device name (`\\.\DISPLAY2`). None, or a
    /// name no longer attached, means the primary monitor — so unplugging a screen cannot strand it.
    #[serde(default)]
    pub notch_monitor: Option<String>,
    /// Notch size as a multiple of the designed size, one of `SIZES`. The whole notch scales: the
    /// window grows and its WebView zooms, so the rings, text and hover card keep their proportions.
    #[serde(default = "default_scale")]
    pub scale: f64,
    /// Where the weekly limit gets a ring of its own: "off", "inside" or "outside".
    #[serde(default = "default_weekly_ring")]
    pub weekly_ring: String,
    /// Which providers the notch itself shows, in order. Empty means every provider that has
    /// something to report — the original behaviour, and the default. Superseded by `notch_slots`,
    /// kept so an existing config migrates cleanly.
    #[serde(default)]
    pub notch_providers: Vec<String>,
    /// Which providers get a ring on the notch, in order. An empty list means every provider.
    #[serde(default)]
    pub notch_slots: Vec<TraySlot>,
    /// Antigravity's lane on the ring, as the Mac app's "Notch reads": "automatic", "5h" or "weekly"
    #[serde(default = "default_antigravity_limit")]
    pub antigravity_limit: String,
    /// The model family that choice looks at, as the Mac app's "Model data": "gemini" or "3p"
    #[serde(default = "default_antigravity_model")]
    pub antigravity_model: String,
    /// false = the pill is kept off the screen edge entirely; the tray icon is then the only way in
    #[serde(default = "yes")]
    pub notch_visible: bool,
    /// What the tray icon's number reads: claude_week, claude_session, codex_week, codex_5h or max
    #[serde(default = "default_tray_icon")]
    pub tray_icon: String,
    /// false = the tray icon is hidden. Refused while the notch is also hidden, because that would
    /// leave the app running with no way to reach it.
    #[serde(default = "yes")]
    pub tray_visible: bool,
    /// Which rows of the System block the notch shows: any of `SYS_METRICS`. All of them unless the
    /// file says otherwise, so a config written before the block existed gets it the first time it
    /// runs a build that has it. An empty list hides the block, and the sampler stops with it.
    #[serde(default = "default_sys_metrics")]
    pub sys_metrics: Vec<String>,
}

fn default_notch_y() -> f64 {
    0.5
}
fn default_notch_edge() -> String {
    "right".into()
}

/// The four edges, in the order Settings lists them.
pub const EDGES: [&str; 4] = ["left", "right", "top", "bottom"];

/// An unreadable edge means the right-hand one, the layout every earlier build used.
pub fn edge_or_right(value: &str) -> String {
    if EDGES.contains(&value) {
        value.to_string()
    } else {
        "right".into()
    }
}

/// True for the edges the notch stands upright on (the pill is a column); false for top and bottom,
/// where it lies flat (the pill is a row) and the window's width and height swap.
pub fn edge_is_vertical(edge: &str) -> bool {
    matches!(edge, "left" | "right")
}
fn default_scale() -> f64 {
    1.0
}
fn default_weekly_ring() -> String {
    "off".into()
}

/// A second arc changes how every reading looks, so an unreadable value means off rather than a
/// guess at what was meant.
pub fn weekly_ring_or_off(value: &str) -> String {
    match value {
        "inside" | "outside" => value.to_string(),
        _ => default_weekly_ring(),
    }
}
fn yes() -> bool {
    true
}
fn default_sys_metrics() -> Vec<String> {
    SYS_METRICS.iter().map(|m| m.to_string()).collect()
}

/// Unknown ids are dropped and the rest come back once each, in the notch's own order: a hand-edited
/// file, or a page sending garbage, cannot put a row on screen that the notch has no drawing for.
pub fn clean_sys_metrics(list: &[String]) -> Vec<String> {
    SYS_METRICS
        .iter()
        .filter(|m| list.iter().any(|l| l == *m))
        .map(|m| m.to_string())
        .collect()
}
fn default_antigravity_limit() -> String {
    "automatic".into()
}
fn default_antigravity_model() -> String {
    "gemini".into()
}

fn default_port() -> u16 {
    48666
}
fn default_lang() -> String {
    "auto".into()
}

#[cfg(windows)]
fn random_secret() -> String {
    use windows::Win32::Security::Cryptography::{BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG};
    let mut bytes = [0u8; 24];
    let ok = unsafe { BCryptGenRandom(None, &mut bytes, BCRYPT_USE_SYSTEM_PREFERRED_RNG) }.is_ok();
    if !ok {
        // BCryptGenRandom failing is effectively unheard-of on a real Windows install; this only
        // keeps a first run from crashing rather than claiming the same strength as the OS RNG.
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let pid = std::process::id() as u128;
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = (t.wrapping_add(pid).rotate_left(i as u32 * 7) >> ((i % 8) * 8)) as u8;
        }
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
#[cfg(not(windows))]
fn random_secret() -> String {
    "0".repeat(48)
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: default_port(),
            secret: random_secret(),
            lang: default_lang(),
            bar_x: None,
            bar_y: None,
            bar_w: None,
            drag_enabled: false,
            notch_y: default_notch_y(),
            notch_edge: default_notch_edge(),
            notch_monitor: None,
            scale: default_scale(),
            weekly_ring: default_weekly_ring(),
            notch_providers: Vec::new(), // empty = show them all
            notch_slots: Vec::new(),     // filled in by load(), from notch_providers
            antigravity_limit: default_antigravity_limit(),
            antigravity_model: default_antigravity_model(),
            notch_visible: true,
            tray_icon: default_tray_icon(),
            tray_visible: true,
            sys_metrics: default_sys_metrics(),
        }
    }
}

fn default_tray_icon() -> String {
    "all4".into()
}

pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("codenotch")
        .join("config.json")
}

pub fn load() -> Config {
    let path = config_path();
    let raw = std::fs::read_to_string(&path).ok();
    let mut cfg: Config = raw
        .as_deref()
        .and_then(|t| serde_json::from_str(t).ok())
        .unwrap_or_default();

    // Migration: before slots existed the notch was a plain provider list, one ring each. That is
    // exactly a list of slots, so nobody's choice is lost and nobody has to reconfigure anything.
    if cfg.notch_slots.is_empty() {
        cfg.notch_slots = cfg
            .notch_providers
            .iter()
            .map(|p| TraySlot { provider: p.clone() })
            .collect();
    }

    // Both hidden would leave the app unreachable: no pill, no tray icon, no way to open settings.
    if !cfg.notch_visible && !cfg.tray_visible {
        cfg.tray_visible = true;
    }

    // The old slider's 40–100 %, or a hand-edited file, lands on one of the three sizes
    cfg.scale = snap_scale(cfg.scale);
    cfg.weekly_ring = weekly_ring_or_off(&cfg.weekly_ring);
    cfg.sys_metrics = clean_sys_metrics(&cfg.sys_metrics);
    cfg
}

pub fn save(cfg: &Config) {
    let path = config_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(txt) = serde_json::to_string_pretty(cfg) {
        let _ = std::fs::write(path, txt);
    }
}

#[cfg(test)]
mod tests {
    use super::{clean_sys_metrics, random_secret, snap_scale, weekly_ring_or_off, Config};

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_saved_scale_snaps_to_the_nearest_size() {
        assert_eq!(snap_scale(0.4), 0.8);
        assert_eq!(snap_scale(0.85), 0.8);
        assert_eq!(snap_scale(0.9), 1.0);
        assert_eq!(snap_scale(1.0), 1.0);
        assert_eq!(snap_scale(1.2), 1.25);
        assert_eq!(snap_scale(3.0), 1.25);
    }

    #[test]
    fn only_the_two_placements_are_kept() {
        assert_eq!(weekly_ring_or_off("inside"), "inside");
        assert_eq!(weekly_ring_or_off("outside"), "outside");
        assert_eq!(weekly_ring_or_off("Inside"), "off");
        assert_eq!(weekly_ring_or_off(""), "off");
    }

    #[test]
    fn random_secret_is_a_48_char_hex_string() {
        let s = random_secret();
        assert_eq!(s.len(), 48);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn random_secret_differs_between_calls() {
        assert_ne!(random_secret(), random_secret());
    }

    #[test]
    fn a_default_config_has_a_nonempty_secret() {
        assert!(!Config::default().secret.is_empty());
    }

    #[test]
    fn a_config_from_before_the_system_block_shows_every_row() {
        let cfg: Config = serde_json::from_str(r#"{"port":48666}"#).unwrap();
        assert_eq!(cfg.sys_metrics, ids(&["cpu", "ram", "disk", "net"]));
        assert_eq!(Config::default().sys_metrics, cfg.sys_metrics);
    }

    #[test]
    fn an_empty_list_is_a_choice_and_stays_empty() {
        let cfg: Config = serde_json::from_str(r#"{"sys_metrics":[]}"#).unwrap();
        assert!(cfg.sys_metrics.is_empty());
    }

    #[test]
    fn system_rows_are_deduplicated_ordered_and_stripped_of_strangers() {
        assert_eq!(clean_sys_metrics(&ids(&["net", "cpu", "net", "gpu", ""])), ids(&["cpu", "net"]));
        assert_eq!(clean_sys_metrics(&ids(&["disk", "ram", "cpu", "net"])), ids(&["cpu", "ram", "disk", "net"]));
        assert!(clean_sys_metrics(&ids(&["CPU", "gpu"])).is_empty());
        assert!(clean_sys_metrics(&[]).is_empty());
    }
}
