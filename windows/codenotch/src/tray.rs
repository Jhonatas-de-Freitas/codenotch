use crate::i18n::tr;
use crate::traymenu;
use tauri::menu::{CheckMenuItemBuilder, Menu, MenuBuilder, MenuItemBuilder, SubmenuBuilder};
use tauri::tray::{TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize, Rect, Wry};

pub fn setup(app: &AppHandle) -> tauri::Result<()> {
    let menu = build_menu(app)?;
    let mut builder = TrayIconBuilder::with_id("main");
    // Windows does not tint tray icons and the monochrome outline vanishes on a dark taskbar, so
    // the app's own mark is the icon. trayicon::app_mark explains why at length.
    builder = builder.icon(icon_image(app));
    builder
        .menu(&menu)
        // A click does nothing; the readings appear on hover, in a small popup above the icon
        .show_menu_on_left_click(false)
        .on_tray_icon_event(|tray, ev| match ev {
            TrayIconEvent::Enter { rect, .. } => hover_show(tray.app_handle(), rect),
            TrayIconEvent::Leave { .. } => hover_hide(tray.app_handle()),
            _ => {}
        })
        .on_menu_event(|app, ev| handle(app, ev.id().as_ref()))
        .build(app)?;
    Ok(())
}

/// The readings themselves, as the Mac's menu bar shows them: a line per provider with its headline
/// figure, and under it one greyed line per limit window. The macOS menu is rebuilt as it opens;
/// Tauri has no such hook, so `refresh_menu` is called whenever a reading changes and once a minute
/// besides, which keeps "Resets in 12 min" honest.
/// Every line the menu would show, in order, with the id each carries. Kept apart from building the
/// menu so a refresh can tell whether anything visible changed before it swaps the menu out.
fn menu_lines(app: &AppHandle, lang: &str) -> Vec<(String, String, bool)> {
    let now = crate::now_ms();
    let mut lines = Vec::new();
    for id in crate::TRAY_PROVIDER_IDS {
        let snap = crate::snapshot_of(app, id);
        if snap.status == "absent" {
            continue;
        }
        let head = traymenu::header(
            crate::provider_label(id),
            crate::ring_fraction(app, id),
            traymenu::stale_since(&snap, now),
            now,
            lang,
        );
        // Clicking a provider re-reads that one, as on the Mac.
        lines.push((format!("refresh:{id}"), head, true));
        for (n, line) in traymenu::provider_lines(&snap, now, lang).iter().enumerate() {
            // Windows does not indent submenu-less items, so the indent is in the text.
            lines.push((format!("line:{id}:{n}"), format!("    {line}"), false));
        }
    }
    lines
}

pub fn build_menu(app: &AppHandle) -> tauri::Result<Menu<Wry>> {
    let lang = language(app);
    build_menu_from(app, &lang, &menu_lines(app, &lang))
}

fn build_menu_from(app: &AppHandle, lang: &str, lines: &[(String, String, bool)]) -> tauri::Result<Menu<Wry>> {
    let lang = lang.to_string();
    let mut items: Vec<tauri::menu::MenuItem<Wry>> = Vec::new();
    for (id, text, enabled) in lines {
        items.push(MenuItemBuilder::with_id(id.clone(), text.clone()).enabled(*enabled).build(app)?);
    }
    if items.is_empty() {
        items.push(
            MenuItemBuilder::with_id("waiting", tr(&lang, "waiting"))
                .enabled(false)
                .build(app)?,
        );
    }
    let refresh = MenuItemBuilder::with_id("refresh", tr(&lang, "refresh_all")).build(app)?;
    let speedtest = MenuItemBuilder::with_id("speedtest", tr(&lang, "speedtest")).build(app)?;
    let settings = MenuItemBuilder::with_id("settings", tr(&lang, "settings")).build(app)?;
    let quit = MenuItemBuilder::with_id("quit", tr(&lang, "quit_app")).build(app)?;
    let current = icon_choice(app);
    let autostart = CheckMenuItemBuilder::with_id("autostart", "Abrir com o Windows")
        .checked(crate::autostart::is_enabled())
        .build(app)?;
    let mut icon_menu = SubmenuBuilder::new(app, "Número no ícone");
    for (key, text) in ICON_CHOICES {
        let item = CheckMenuItemBuilder::with_id(format!("icon:{key}"), *text).checked(current == *key).build(app)?;
        icon_menu = icon_menu.item(&item);
    }
    let icon_menu = icon_menu.build()?;
    let mut menu = MenuBuilder::new(app);
    for item in &items {
        menu = menu.item(item);
    }
    menu.separator()
        .item(&icon_menu)
        .item(&autostart)
        .item(&refresh)
        .item(&speedtest)
        .item(&settings)
        .separator()
        .item(&quit)
        .build()
}

/// The language the menu speaks, already resolved: `traymenu` picks its wording by code and has no
/// "auto" of its own.
fn language(app: &AppHandle) -> String {
    let st = app.state::<crate::AppState>();
    let raw = st.cfg.lock().unwrap().lang.clone();
    if raw == "auto" {
        crate::i18n::resolve_auto().to_string()
    } else {
        raw
    }
}

/// Rebuilds the tray menu, ALWAYS on the main thread.
///
/// A menu is a Windows UI object. Building one or swapping it in from another thread leaves the
/// tray holding a menu that never opens again — and since changing the language is what triggers a
/// rebuild, the user is then locked out of the only place they could change it back. The tray's own
/// click handlers already run on the main thread, but the readings poller and the settings window
/// do not, so the hop is done here once rather than being remembered at every call site.
/// What the menu last showed, so an unchanged refresh leaves it alone.
static SHOWN: std::sync::Mutex<Option<(String, Vec<(String, String, bool)>)>> = std::sync::Mutex::new(None);

/// Swaps the menu only when a line of it would read differently. `set_menu` replaces the menu the
/// user may have open this moment — the refresh runs on the main thread, which the open popup's
/// message loop still serves — so the minute tick used to close it under the pointer even when
/// "Resets in 12 min" still said 12 min.
pub fn refresh_menu(app: &AppHandle) {
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        if let Some(tray) = handle.tray_by_id("main") {
            let lang = language(&handle);
            let _ = tray.set_icon(Some(icon_image(&handle)));
            let lines = menu_lines(&handle, &lang);
            let key = (format!("{lang}|{}|{}", icon_choice(&handle), crate::autostart::is_enabled()), lines.clone());
            if SHOWN.lock().unwrap().as_ref() == Some(&key) {
                // A tooltip can change without a line changing, and setting it closes nothing.
                return;
            }
            match build_menu_from(&handle, &lang, &lines) {
                Ok(menu) => {
                    let _ = tray.set_menu(Some(menu));
                    *SHOWN.lock().unwrap() = Some(key);
                }
                Err(e) => crate::applog(&format!("tray menu: {e}")),
            }
        }
    });
}

/// Provider rows carry `refresh:<id>`; everything else the menu offers is one of the five fixed
/// items. Settings, language, hooks and the rest arrive as commands from the settings window.
fn handle(app: &AppHandle, id: &str) {
    if let Some(choice) = id.strip_prefix("icon:") {
        {
            let st = app.state::<crate::AppState>();
            let mut c = st.cfg.lock().unwrap();
            c.tray_icon = choice.to_string();
            crate::config::save(&c);
        }
        // The menu's check marks changed, which the unchanged-lines test cannot see
        *SHOWN.lock().unwrap() = None;
        refresh_menu(app);
        return;
    }
    if let Some(provider) = id.strip_prefix("refresh:") {
        refresh_provider(app, provider);
        return;
    }
    match id {
        "autostart" => {
            let result = if crate::autostart::is_enabled() { crate::autostart::disable() } else { crate::autostart::enable() };
            if let Err(e) = result {
                crate::applog(&format!("autostart: {e}"));
            }
            *SHOWN.lock().unwrap() = None;
            refresh_menu(app);
        }
        "refresh" => {
            for provider in crate::TRAY_PROVIDER_IDS {
                refresh_provider(app, provider);
            }
            let a = app.clone();
            std::thread::spawn(move || crate::reload_glyphs(&a));
        }
        "speedtest" => {
            // A test already under way is left to finish: a second click is not a second test
            crate::start_speedtest(app);
        }
        "settings" => crate::settings_window::open(app),
        "quit" => app.exit(0),
        _ => {}
    }
}

/// Asks one provider to read again. Claude's backoff is cleared first: asking for a reading is the
/// user saying they want it now, not in fifteen minutes.
fn refresh_provider(app: &AppHandle, provider: &str) {
    match provider {
        "codex" => crate::codex::request_refresh(),
        "cursor" => crate::cursor::request_refresh(),
        "gemini" => crate::antigravity::request_refresh(),
        _ => {
            {
                let st = app.state::<crate::AppState>();
                let mut u = st.usage.lock().unwrap();
                u.backoff_until = 0;
            }
            crate::usage::request_refresh();
        }
    }
}


/// What the tray icon's number can read, in the order the menu lists them.
const ICON_CHOICES: &[(&str, &str)] = &[
    ("all4", "Claude e GPT juntos (alterna sessão e semana)"),
    ("claude_week", "Claude · total da semana"),
    ("codex_week", "GPT (Codex) · total da semana"),
    ("claude_session", "Claude · sessão atual"),
    ("codex_5h", "GPT (Codex) · janela de 5 h"),
    ("max", "O maior entre Claude e GPT"),
];

fn icon_choice(app: &AppHandle) -> String {
    let st = app.state::<crate::AppState>();
    let c = st.cfg.lock().unwrap();
    c.tray_icon.clone()
}

/// Does this window feed the number on the icon under `choice`?
fn feeds_icon(choice: &str, provider: &str, w: &crate::usage::LimitWindow) -> bool {
    match (choice, provider) {
        ("all4", "claude") => matches!(w.id.as_str(), "weekly_all" | "seven_day" | "session"),
        ("all4", "codex") => w.group.is_none() && matches!(w.label.as_str(), "Weekly limit" | "5h limit"),
        ("claude_week", "claude") => matches!(w.id.as_str(), "weekly_all" | "seven_day"),
        ("claude_session", "claude") => w.id == "session",
        ("codex_week", "codex") => w.group.is_none() && w.label == "Weekly limit",
        ("codex_5h", "codex") => w.group.is_none() && w.label == "5h limit",
        _ => false,
    }
}

/// Which half the paired icon shows: false = session, true = week. Flipped by a timer.
static WEEK_PHASE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The icon for the current choice: the Claude/Codex pair, or one number.
fn icon_image(app: &AppHandle) -> tauri::image::Image<'static> {
    if icon_choice(app) == "all4" {
        let week = WEEK_PHASE.load(std::sync::atomic::Ordering::Relaxed);
        let find = |provider: &str, id: &str, label: &str| -> Option<u32> {
            let snap = crate::snapshot_of(app, provider);
            snap.windows
                .iter()
                .find(|w| w.count.is_none() && w.group.is_none() && (w.id == id || (!label.is_empty() && w.label == label)))
                .map(|w| (w.used * 100.0).round().clamp(0.0, 100.0) as u32)
        };
        let claude = if week { find("claude", "weekly_all", "").or_else(|| find("claude", "seven_day", "")) } else { find("claude", "session", "") };
        let codex = find("codex", "", if week { "Weekly limit" } else { "5h limit" });
        return crate::gauge::quad([claude, codex], week, BLINK.load(std::sync::atomic::Ordering::Relaxed));
    }
    crate::gauge::icon(icon_pct(app), BLINK.load(std::sync::atomic::Ordering::Relaxed))
}

/// The icon is in the "on" half of its blink: the tile goes red (paired) or white (single).
static BLINK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// A blink lasts this long from the moment a window passes 90 %, then the icon stays steady red:
/// flashing for hours would be a nuisance, and the colour still says it.
const BLINK_FOR_MS: u64 = 60_000;

/// Is any watched window at 90 % or more?
fn any_hot(app: &AppHandle) -> bool {
    ["claude", "codex"].iter().any(|id| {
        let snap = crate::snapshot_of(app, id);
        snap.status == "ok" && snap.windows.iter().any(|w| w.count.is_none() && w.id != "weekly_scoped" && w.used >= 0.90)
    })
}

/// Makes the icon flash for a minute when a limit passes 90 %.
pub fn start_icon_blink(app: AppHandle) {
    std::thread::spawn(move || {
        let mut hot_since: Option<u64> = None;
        loop {
            std::thread::sleep(std::time::Duration::from_millis(600));
            let now = crate::now_ms();
            let hot = any_hot(&app);
            let was = BLINK.load(std::sync::atomic::Ordering::Relaxed);
            let want = if hot {
                let since = *hot_since.get_or_insert(now);
                now.saturating_sub(since) < BLINK_FOR_MS && !was
            } else {
                hot_since = None;
                false
            };
            if want != was {
                BLINK.store(want, std::sync::atomic::Ordering::Relaxed);
                let handle = app.clone();
                let _ = app.run_on_main_thread(move || {
                    if let Some(tray) = handle.tray_by_id("main") {
                        let _ = tray.set_icon(Some(icon_image(&handle)));
                    }
                });
            }
        }
    });
}

/// Flips the paired icon between session and week every few seconds.
pub fn start_icon_rotation(app: AppHandle) {
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(4));
        if icon_choice(&app) != "all4" {
            continue;
        }
        WEEK_PHASE.fetch_xor(true, std::sync::atomic::Ordering::Relaxed);
        let handle = app.clone();
        let _ = app.run_on_main_thread(move || {
            if let Some(tray) = handle.tray_by_id("main") {
                let _ = tray.set_icon(Some(icon_image(&handle)));
            }
        });
    });
}

/// The number on the icon: the chosen window, or the worst of the headline readings.
fn icon_pct(app: &AppHandle) -> Option<u32> {
    let choice = icon_choice(app);
    let pct = |w: &crate::usage::LimitWindow| (w.used * 100.0).round().clamp(0.0, 100.0) as u32;
    if choice == "max" {
        return ["claude", "codex"].iter().filter_map(|id| crate::ring_pct(app, id)).max();
    }
    let provider = if choice.starts_with("claude") { "claude" } else { "codex" };
    let snap = crate::snapshot_of(app, provider);
    snap.windows.iter().find(|w| w.count.is_none() && feeds_icon(&choice, provider, w)).map(pct)
}

const HOVER: &str = "hover";
const HOVER_W: f64 = 380.0; // CSS px
// Heights in CSS px, kept in step with hover.html: the window is sized here, the page cannot resize it
const PAD: f64 = 20.0;
const HEAD_H: f64 = 32.0;
const WIN_H: f64 = 52.0;
const COUNT_H: f64 = 24.0;
const NOTE_H: f64 = 22.0;
const BLOCK_GAP: f64 = 12.0;

/// The popup is built once, hidden, so its page is already listening when the first hover arrives.
pub fn build_hover(app: &AppHandle) {
    let b = tauri::WebviewWindowBuilder::new(app, HOVER, tauri::WebviewUrl::App("hover.html".into()))
        .inner_size(HOVER_W, 120.0)
        .decorations(false)
        .shadow(true)
        .always_on_top(true)
        .skip_taskbar(true)
        .resizable(false)
        .focusable(false)
        .focused(false)
        .visible(false);
    match b.build() {
        Ok(w) => {
            let _ = w.set_ignore_cursor_events(true);
        }
        Err(e) => crate::applog(&format!("hover window: {e}")),
    }
}

#[derive(serde::Serialize)]
struct HoverWindow {
    label: String,
    used: f64,
    count: Option<i64>,
    resets_at: Option<u64>,
    derived: bool,
    /// This is the window whose number the tray icon shows
    on_icon: bool,
}

#[derive(serde::Serialize)]
struct HoverProvider {
    id: String,
    name: String,
    /// How long the reading has been stale, in ms, when it is old enough to say so
    stale_ms: Option<u64>,
    note: String,
    windows: Vec<HoverWindow>,
}

/// What the popup draws, as data: the page lays it out and words it. Also returns the height it needs.
fn hover_data(app: &AppHandle) -> (Vec<HoverProvider>, f64) {
    let now = crate::now_ms();
    let choice = icon_choice(app);
    let mut out = Vec::new();
    let mut height = PAD;
    for id in crate::TRAY_PROVIDER_IDS {
        let snap = crate::snapshot_of(app, id);
        if snap.status == "absent" {
            continue;
        }
        let windows: Vec<HoverWindow> = snap
            .windows
            .iter()
            .filter(|w| w.id != "weekly_scoped")
            .map(|w| HoverWindow {
                label: w.label.clone(),
                used: w.used.clamp(0.0, 1.0),
                count: w.count,
                resets_at: w.resets_at,
                derived: w.derived,
                on_icon: w.count.is_none() && feeds_icon(&choice, id, w),
            })
            .collect();
        height += HEAD_H + BLOCK_GAP;
        for w in &windows {
            height += if w.count.is_some() { COUNT_H } else { WIN_H };
        }
        if windows.is_empty() && !snap.note.is_empty() {
            height += NOTE_H;
        }
        out.push(HoverProvider {
            id: id.to_string(),
            name: crate::provider_label(id).to_string(),
            stale_ms: traymenu::stale_since(&snap, now).map(|t| now.saturating_sub(t)),
            note: snap.note.clone(),
            windows,
        });
    }
    (out, height)
}

fn hover_show(app: &AppHandle, rect: Rect) {
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        let Some(w) = handle.get_webview_window(HOVER) else { return };
        let (data, css_h) = hover_data(&handle);
        let css_h = if data.is_empty() { PAD + NOTE_H } else { css_h };
        let scale = w.scale_factor().unwrap_or(1.0);
        let (width, height) = (HOVER_W * scale, css_h * scale);
        let _ = w.set_size(PhysicalSize::new(width as u32, height as u32));
        let pos = rect.position.to_physical::<f64>(1.0);
        let size = rect.size.to_physical::<f64>(1.0);
        // Centred over the icon, kept on the screen, and clear of the taskbar by a few pixels
        let (mut x, mut y) = (pos.x + size.width / 2.0 - width / 2.0, pos.y - height - 8.0 * scale);
        if let Ok(Some(m)) = w.current_monitor().or_else(|_| handle.primary_monitor()) {
            let (mx, mw) = (m.position().x as f64, m.size().width as f64);
            x = x.clamp(mx, (mx + mw - width).max(mx));
        }
        y = y.max(0.0);
        let _ = w.set_position(PhysicalPosition::new(x as i32, y as i32));
        let _ = w.emit("hover_data", serde_json::json!({ "now": crate::now_ms(), "providers": data }));
        let _ = w.show();
    });
}

fn hover_hide(app: &AppHandle) {
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        if let Some(w) = handle.get_webview_window(HOVER) {
            let _ = w.hide();
        }
    });
}
