//! Windows toasts for the Claude and Codex limit windows:
//!
//! - a window crossing 25, 50, 75, 90, 95 and 100 %, once per level;
//! - a window that renewed after the user had been close to its limit (75 % or more);
//! - a fast climb: 20 points or more within 30 minutes, at most once an hour per window.
//!
//! A window remembers the highest level already announced and stays quiet until it climbs past it;
//! a new reset time starts it over. The first reading after launch announces nothing — it only
//! records where each window already is, so restarting the app at 56 % does not replay 25 and 50.

use std::collections::{HashMap, VecDeque};
use tauri::AppHandle;

const LEVELS: [u32; 6] = [25, 50, 75, 90, 95, 100];
const PROVIDERS: [&str; 2] = ["claude", "codex"];
/// A renewal is only worth a toast when the window had got this far.
const RENEWAL_FROM: u32 = 75;
/// Fast climb: this many points of the limit...
const FAST_POINTS: f64 = 0.20;
/// ...within this long...
const FAST_WINDOW_MS: u64 = 30 * 60 * 1000;
/// ...and no second warning for the same window inside this.
const FAST_QUIET_MS: u64 = 60 * 60 * 1000;

/// Highest level announced for a window, and the reset time it was announced under.
struct Seen {
    level: u32,
    resets_at: Option<u64>,
}

/// What one reading of a window means.
#[derive(Debug, PartialEq)]
struct Outcome {
    /// A level just crossed
    announce: Option<u32>,
    /// A new cycle began after the window had been near its limit
    renewed: bool,
}

/// The highest level at or under `used` (a 0.0–1.0 fraction); 0 when below the first.
fn level_of(used: f64) -> u32 {
    let pct = (used * 100.0 + 1e-6).floor() as u32;
    LEVELS.iter().copied().filter(|l| pct >= *l).max().unwrap_or(0)
}

/// The vendor's reset time wobbles by a few seconds between reads; a real new cycle moves it by hours.
fn same_cycle(a: Option<u64>, b: Option<u64>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => x.abs_diff(y) < 10 * 60 * 1000,
        _ => true,
    }
}

/// What to do with a reading: the outcome, and the state to remember.
fn step(prev: Option<&Seen>, used: f64, resets_at: Option<u64>, first_pass: bool) -> (Outcome, Seen) {
    let now_level = level_of(used);
    let same = prev.is_some_and(|p| same_cycle(p.resets_at, resets_at));
    if prev.is_none() && first_pass {
        return (Outcome { announce: None, renewed: false }, Seen { level: now_level, resets_at });
    }
    let renewed = prev.is_some_and(|p| !same && p.level >= RENEWAL_FROM && now_level < p.level);
    let carried = if same { prev.map_or(0, |p| p.level) } else { 0 };
    let announce = (now_level > carried).then_some(now_level);
    // Never lowered inside a cycle, so a reading wobbling around a line does not announce it twice
    (Outcome { announce, renewed }, Seen { level: now_level.max(carried), resets_at })
}

/// Recent readings of one window, to spot a fast climb.
#[derive(Default)]
struct Trail {
    samples: VecDeque<(u64, f64)>,
    last_warned: u64,
}

/// Records a reading; true when usage has climbed FAST_POINTS within FAST_WINDOW_MS and the window
/// has not been warned about lately. Returns the minutes the climb took.
fn climb(trail: &mut Trail, now: u64, used: f64) -> Option<u64> {
    // Usage falling is a renewal or a corrected reading: the history before it means nothing
    if trail.samples.back().is_some_and(|(_, u)| used + 0.05 < *u) {
        trail.samples.clear();
    }
    if trail.samples.back().map(|(_, u)| *u) != Some(used) {
        trail.samples.push_back((now, used));
    }
    while trail.samples.front().is_some_and(|(t, _)| now.saturating_sub(*t) > FAST_WINDOW_MS) {
        trail.samples.pop_front();
    }
    let (t0, u0) = *trail.samples.iter().min_by(|a, b| a.1.total_cmp(&b.1))?;
    let quiet = trail.last_warned != 0 && now.saturating_sub(trail.last_warned) < FAST_QUIET_MS;
    if used - u0 >= FAST_POINTS && used < 1.0 && !quiet {
        trail.last_warned = now;
        return Some((now.saturating_sub(t0) / 60_000).max(1));
    }
    None
}

pub fn start(app: AppHandle) {
    std::thread::spawn(move || {
        let mut seen: HashMap<String, Seen> = HashMap::new();
        let mut trails: HashMap<String, Trail> = HashMap::new();
        let mut first_pass: HashMap<&str, bool> = PROVIDERS.iter().map(|p| (*p, true)).collect();
        loop {
            std::thread::sleep(std::time::Duration::from_secs(5));
            for provider in PROVIDERS {
                let snap = crate::snapshot_of(&app, provider);
                // Nothing real to compare against: not read yet, signed out, or backing off with the old numbers
                if snap.status != "ok" || snap.windows.is_empty() {
                    continue;
                }
                let first = first_pass.get(provider).copied().unwrap_or(false);
                let now = crate::now_ms();
                for w in snap.windows.iter().filter(|w| w.count.is_none() && w.id != "weekly_scoped") {
                    let key = format!("{provider}:{}:{}", w.group.clone().unwrap_or_default(), w.id);
                    let (out, next) = step(seen.get(&key), w.used, w.resets_at, first);
                    seen.insert(key.clone(), next);
                    if let Some(level) = out.announce {
                        let title = if level >= 100 {
                            format!("{} — limite atingido", provider_name(provider))
                        } else {
                            format!("{} — {level}% usado", provider_name(provider))
                        };
                        toast(&title, &pt_label(&w.label));
                    }
                    if out.renewed {
                        toast(
                            &format!("{} — limite renovado", provider_name(provider)),
                            &format!("{} voltou a zero, pode usar.", pt_label(&w.label)),
                        );
                    }
                    if !first {
                        let trail = trails.entry(key).or_default();
                        if let Some(minutes) = climb(trail, now, w.used) {
                            toast(
                                &format!("{} — gasto rápido", provider_name(provider)),
                                &format!("{}: subiu {}% em {minutes} min.", pt_label(&w.label), (FAST_POINTS * 100.0) as u32),
                            );
                        }
                    }
                }
                first_pass.insert(provider, false);
            }
        }
    });
}

fn provider_name(provider: &str) -> &'static str {
    if provider == "codex" { "Codex (GPT)" } else { "Claude" }
}

/// The window names as the hover popup words them.
fn pt_label(label: &str) -> String {
    match label {
        "Current session" => "Sessão atual",
        "Weekly (all models)" | "Weekly limit" | "Weekly Limit" => "Semana",
        "Weekly (Opus)" => "Semana · Opus",
        "5h limit" | "5-hour Limit" | "5-Hour Limit" => "Janela de 5 horas",
        "Monthly limit" | "Monthly Limit" => "Mês",
        other => other,
    }
    .to_string()
}

fn toast(title: &str, body: &str) {
    crate::applog(&format!("alert: {title} / {body}"));
    // Text goes in through the environment, never spliced into the script, so a label cannot break out of it
    let script = r#"
[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] | Out-Null
[Windows.Data.Xml.Dom.XmlDocument, Windows.Data.Xml.Dom.XmlDocument, ContentType = WindowsRuntime] | Out-Null
$x = New-Object Windows.Data.Xml.Dom.XmlDocument
$x.LoadXml('<toast><visual><binding template="ToastGeneric"><text id="1"></text><text id="2"></text></binding></visual></toast>')
$t = $x.GetElementsByTagName('text')
$t.Item(0).AppendChild($x.CreateTextNode($env:CN_TITLE)) | Out-Null
$t.Item(1).AppendChild($x.CreateTextNode($env:CN_BODY)) | Out-Null
$id = '{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\WindowsPowerShell\v1.0\powershell.exe'
[Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier($id).Show([Windows.UI.Notifications.ToastNotification]::new($x))
"#;
    let mut cmd = std::process::Command::new("powershell");
    cmd.args(["-NoProfile", "-NonInteractive", "-WindowStyle", "Hidden", "-Command", script])
        .env("CN_TITLE", title)
        .env("CN_BODY", body);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let _ = cmd.spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: u64 = 3_600_000;

    #[test]
    fn levels_follow_the_thresholds() {
        assert_eq!(level_of(0.0), 0);
        assert_eq!(level_of(0.249), 0);
        assert_eq!(level_of(0.25), 25);
        assert_eq!(level_of(0.56), 50);
        assert_eq!(level_of(0.95), 95);
        assert_eq!(level_of(1.0), 100);
    }

    #[test]
    fn first_reading_is_silent() {
        let (o, s) = step(None, 0.56, Some(1), true);
        assert_eq!(o, Outcome { announce: None, renewed: false });
        assert_eq!(s.level, 50);
    }

    #[test]
    fn crossing_announces_once() {
        let s = Seen { level: 50, resets_at: Some(1) };
        let (o, s) = step(Some(&s), 0.76, Some(1), false);
        assert_eq!(o.announce, Some(75));
        let (o, _) = step(Some(&s), 0.80, Some(1), false);
        assert_eq!(o.announce, None);
    }

    #[test]
    fn jumping_over_levels_announces_the_highest() {
        let s = Seen { level: 0, resets_at: Some(1) };
        let (o, _) = step(Some(&s), 0.93, Some(1), false);
        assert_eq!(o.announce, Some(90));
    }

    #[test]
    fn a_new_cycle_starts_over() {
        let s = Seen { level: 100, resets_at: Some(1_000_000) };
        let (o, s) = step(Some(&s), 0.02, Some(100_000_000), false);
        assert_eq!(o.announce, None);
        assert_eq!(s.level, 0);
        let (o, _) = step(Some(&s), 0.27, Some(100_000_000), false);
        assert_eq!(o.announce, Some(25));
    }

    #[test]
    fn a_renewal_after_a_near_limit_is_announced() {
        let s = Seen { level: 90, resets_at: Some(1_000_000) };
        let (o, _) = step(Some(&s), 0.0, Some(100_000_000), false);
        assert!(o.renewed);
    }

    #[test]
    fn a_renewal_from_a_low_level_stays_quiet() {
        let s = Seen { level: 50, resets_at: Some(1_000_000) };
        let (o, _) = step(Some(&s), 0.0, Some(100_000_000), false);
        assert!(!o.renewed);
    }

    #[test]
    fn a_wobbling_reset_time_is_not_a_renewal() {
        let s = Seen { level: 90, resets_at: Some(1_000_000) };
        let (o, _) = step(Some(&s), 0.91, Some(1_000_500), false);
        assert!(!o.renewed);
    }

    #[test]
    fn a_fast_climb_warns_once_an_hour() {
        let mut t = Trail::default();
        let m = 60_000;
        assert_eq!(climb(&mut t, 0, 0.10), None);
        assert_eq!(climb(&mut t, 10 * m, 0.20), None);
        assert_eq!(climb(&mut t, 20 * m, 0.31), Some(20));
        // Still climbing, but warned a moment ago
        assert_eq!(climb(&mut t, 25 * m, 0.45), None);
        // Over an hour later, another sharp climb counts again
        assert_eq!(climb(&mut t, 2 * HOUR, 0.50), None);
        assert_eq!(climb(&mut t, 2 * HOUR + 10 * m, 0.72), Some(10));
    }

    #[test]
    fn a_slow_climb_is_not_fast() {
        let mut t = Trail::default();
        let m = 60_000;
        for i in 0..10u64 {
            assert_eq!(climb(&mut t, i * 20 * m, 0.1 + 0.05 * i as f64), None);
        }
    }

    #[test]
    fn a_drop_forgets_the_history() {
        let mut t = Trail::default();
        let m = 60_000;
        climb(&mut t, 0, 0.90);
        climb(&mut t, 5 * m, 0.01);
        assert_eq!(climb(&mut t, 10 * m, 0.10), None);
    }
}
