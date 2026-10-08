#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod indexer;
mod search;
mod launcher;
mod icons;

use tauri::{Emitter, Manager, WindowEvent};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, TrayIconBuilder, TrayIconEvent};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::process::Command;
use std::time::{Duration, Instant};

const DEFAULT_HOTKEY: &str = "Ctrl+Space";
/// How long after showing the main window a focus-loss is treated as part of
/// the show hand-off instead of a real "user clicked away".
const SHOW_FOCUS_GRACE: Duration = Duration::from_millis(350);

/// The shortcut that is currently registered with the OS (if any).
static REGISTERED_SHORTCUT: Mutex<Option<Shortcut>> = Mutex::new(None);
/// When the main window was last shown (used for the focus-loss grace period).
static SHOWN_AT: Mutex<Option<Instant>> = Mutex::new(None);
/// Set while the settings window is recording a new hotkey; the current
/// shortcut is unregistered so the recorder can actually see the key presses.
static HOTKEY_PAUSED: AtomicBool = AtomicBool::new(false);
/// Serializes pause/unpause/save registration work. Rapid record-stop-record
/// cycles send overlapping IPC commands; without this, an unpause that runs
/// after a second pause would re-register the live hotkey mid-recording and
/// the OS would swallow the very keys being recorded.
static HOTKEY_OP_LOCK: Mutex<()> = Mutex::new(());

const DEFAULT_ENGINE: &str = "google";

/// Search engines the palette can hand queries to. IDs are stored in
/// config.json; labels/URLs live here and in the frontend (`App.tsx` keeps
/// its own copy - update both when adding one).
fn sanitize_engine(raw: Option<String>) -> String {
    match raw.as_deref().unwrap_or(DEFAULT_ENGINE) {
        "google" | "bing" | "duckduckgo" | "brave" => raw.unwrap_or_else(|| DEFAULT_ENGINE.to_string()),
        _ => DEFAULT_ENGINE.to_string(),
    }
}

#[derive(serde::Deserialize, serde::Serialize, Default, Clone)]
struct AppConfig {
    hotkey: Option<String>,
    theme: Option<String>,
    search_engine: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize, Clone)]
pub struct FullConfig {
    pub hotkey: String,
    pub theme: String,
    pub startup: bool,
    pub search_engine: String,
}

fn get_config_from_disk(app: &tauri::AppHandle) -> AppConfig {
    if let Ok(config_dir) = app.path().app_config_dir() {
        let config_path = config_dir.join("config.json");
        if config_path.exists() {
            if let Ok(content) = std::fs::read_to_string(&config_path) {
                if let Ok(config) = serde_json::from_str::<AppConfig>(&content) {
                    return config;
                }
            }
        }
    }
    AppConfig::default()
}

/// True when the user has never saved settings (fresh install). Used to apply
/// first-run defaults such as auto-start without touching existing setups.
fn config_file_missing(app: &tauri::AppHandle) -> bool {
    match app.path().app_config_dir() {
        Ok(dir) => !dir.join("config.json").exists(),
        Err(_) => true,
    }
}

fn check_autostart() -> bool {
    if let Ok(key) = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
        .open_subkey_with_flags(
            r"Software\Microsoft\Windows\CurrentVersion\Run",
            winreg::enums::KEY_READ,
        )
    {
        let val: Result<String, _> = key.get_value("Glimpse");
        return val.is_ok();
    }
    false
}

fn create_vbs_launcher(exe_path: &std::path::Path, vbs_path: &std::path::Path) -> Result<(), String> {
    // VBS string literals only need doubled *quotes* - backslashes are literal,
    // so escaping them (as before) produced paths like C:\\Users\\..\\app.exe.
    let exe_path_str = exe_path.to_string_lossy().replace('"', "\"\"");
    let vbs_content = format!(
        r#"Set WshShell = CreateObject("WScript.Shell")
WshShell.Run "{}", 0, False
Set WshShell = Nothing"#,
        exe_path_str
    );
    std::fs::write(vbs_path, vbs_content)
        .map_err(|e| format!("Failed to create VBS launcher: {}", e))
}

fn get_system_uptime_seconds() -> u64 {
    use windows::Win32::System::SystemInformation::GetTickCount64;
    unsafe {
        GetTickCount64() / 1000
    }
}

fn is_webview2_available() -> bool {
    use winreg::enums::*;

    let check_keys = [
        r"SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}",
        r"SOFTWARE\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}",
    ];

    for key_path in &check_keys {
        if let Ok(key) = winreg::RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey_with_flags(key_path, KEY_READ) {
            if let Ok(_) = key.get_value::<String, _>("pv") {
                return true;
            }
        }
        if let Ok(key) = winreg::RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(key_path, KEY_READ) {
            if let Ok(_) = key.get_value::<String, _>("pv") {
                return true;
            }
        }
    }

    // Also check for Evergreen Standalone
    if let Ok(key) = winreg::RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey_with_flags(
        r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\Microsoft Edge WebView2 Runtime",
        KEY_READ
    ) {
        if let Ok(_) = key.get_value::<String, _>("DisplayVersion") {
            return true;
        }
    }

    false
}

fn set_autostart(enable: bool) {
    if let Ok(exe) = std::env::current_exe() {
        let config_dir = exe.parent().unwrap_or(std::path::Path::new("."));
        let vbs_path = config_dir.join("glimpse-launcher.vbs");

        if let Ok(key) = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
            .open_subkey_with_flags(
                r"Software\Microsoft\Windows\CurrentVersion\Run",
                winreg::enums::KEY_WRITE | winreg::enums::KEY_READ,
            )
        {
            if enable {
                // Create VBS wrapper to hide console window
                if let Err(e) = create_vbs_launcher(&exe, &vbs_path) {
                    eprintln!("Failed to create VBS launcher: {}", e);
                    // Fallback to direct exe path
                    let _ = key.set_value("Glimpse", &exe.to_string_lossy().as_ref());
                } else {
                    let _ = key.set_value("Glimpse", &vbs_path.to_string_lossy().as_ref());
                }
            } else {
                let _ = key.delete_value("Glimpse");
                // Clean up VBS file if it exists
                let _ = std::fs::remove_file(&vbs_path);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Hotkey handling
// ---------------------------------------------------------------------------

fn parse_shortcut(hotkey: &str) -> Result<Shortcut, String> {
    Shortcut::from_str(hotkey.trim())
        .map_err(|e| format!("\"{}\" is not a valid hotkey: {}", hotkey.trim(), e))
}

/// A global hotkey must include Ctrl, Alt or the Windows (Super) key.
/// Without this a single letter (e.g. "A") would be swallowed system-wide and
/// typing that letter anywhere in Windows would become impossible.
fn ensure_has_modifier(shortcut: &Shortcut) -> Result<(), String> {
    let required = Modifiers::CONTROL | Modifiers::ALT | Modifiers::SUPER;
    if !shortcut.mods.intersects(required) {
        return Err("Hotkey must include Ctrl, Alt, or the Windows (Super) key.".to_string());
    }
    Ok(())
}

fn validate_hotkey_string(hotkey: &str) -> Result<Shortcut, String> {
    let shortcut = parse_shortcut(hotkey)?;
    ensure_has_modifier(&shortcut)?;
    Ok(shortcut)
}

fn register_shortcut(app: &tauri::AppHandle, shortcut: Shortcut) -> Result<(), String> {
    let handle = app.clone();
    app.global_shortcut()
        .on_shortcut(shortcut, move |_app, _shortcut, event| {
            if event.state == ShortcutState::Pressed {
                show_main_window(&handle);
            }
        })
        .map_err(|e| {
            format!(
                "Could not register hotkey (it is probably used by another app): {}",
                e
            )
        })?;
    if let Ok(mut registered) = REGISTERED_SHORTCUT.lock() {
        *registered = Some(shortcut);
    }
    Ok(())
}

fn unregister_registered(app: &tauri::AppHandle) {
    if let Ok(mut registered) = REGISTERED_SHORTCUT.lock() {
        if let Some(shortcut) = registered.take() {
            let _ = app.global_shortcut().unregister(shortcut);
        }
    }
}

fn registered_shortcut() -> Option<Shortcut> {
    REGISTERED_SHORTCUT.lock().ok().and_then(|g| *g)
}

/// Bring the window to the foreground, even when Windows would normally
/// refuse (foreground lock). Without this the palette can open *without*
/// keyboard focus, which means the first keystrokes are swallowed by whatever
/// application was focused before - the classic "I have to click it first"
/// bug. Two strategies are used, cheapest first:
///   1. AttachThreadInput to the foreground thread + SetForegroundWindow
///   2. the classic Alt-key trick that grants foreground permission
fn force_foreground(window: &tauri::WebviewWindow) {
    use windows::Win32::Foundation::{HWND, FALSE, TRUE};
    use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        keybd_event, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, VK_MENU,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowThreadProcessId, IsIconic, SetForegroundWindow,
        ShowWindow, SW_RESTORE,
    };

    let tauri_hwnd = match window.hwnd() {
        Ok(h) => h,
        Err(_) => return,
    };
    // tauri 2.10 is built against `windows` 0.61 while this crate uses 0.58,
    // so go through the raw value instead of the foreign HWND type.
    let raw = tauri_hwnd.0 as isize;
    let hwnd = HWND(raw as *mut std::ffi::c_void);

    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }

        let foreground = GetForegroundWindow();
        if foreground == hwnd {
            return;
        }

        let foreground_thread = if foreground.is_invalid() {
            0
        } else {
            GetWindowThreadProcessId(foreground, None)
        };
        let our_thread = GetCurrentThreadId();

        if foreground_thread != 0 && foreground_thread != our_thread {
            let attached = AttachThreadInput(foreground_thread, our_thread, TRUE).as_bool();
            let _ = SetForegroundWindow(hwnd);
            if attached {
                let _ = AttachThreadInput(foreground_thread, our_thread, FALSE);
            }
        } else {
            let _ = SetForegroundWindow(hwnd);
        }

        // SetForegroundWindow flips foreground *asynchronously* - checking
        // immediately afterwards still reports the old window, which used to
        // fire the Alt-key trick (a visible activation flicker) every single
        // open even when it was never needed. Give the flip a moment to land
        // and only escalate if we genuinely are still in the background.
        std::thread::sleep(Duration::from_millis(50));
        if GetForegroundWindow() != hwnd {
            keybd_event(VK_MENU.0 as u8, 0, KEYEVENTF_EXTENDEDKEY, 0);
            let _ = SetForegroundWindow(hwnd);
            keybd_event(VK_MENU.0 as u8, 0, KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP, 0);
        }
    }
}

fn show_main_window(app: &tauri::AppHandle) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };

    // Already open (hotkey repeat, double tray click, ...): just make sure
    // the keyboard lands in the search box. Re-running the whole show
    // sequence here would steal/reassert foreground a second time - that
    // reads as a blink on open.
    if window.is_visible().unwrap_or(false) {
        let _ = window.set_focus();
        let _ = window.emit("window-focused", ());
        return;
    }

    let _ = window.unminimize();
    let _ = window.show();
    let _ = window.set_always_on_top(true);
    let _ = window.set_focus();
    force_foreground(&window);

    if let Ok(mut shown_at) = SHOWN_AT.lock() {
        *shown_at = Some(Instant::now());
    }

    // Tell the frontend so it can put the caret in the search box - DOM
    // focus is *not* restored automatically after a hidden window is shown.
    let _ = window.emit("window-shown", ());
}

fn hide_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.hide();
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

#[tauri::command]
fn get_full_config(app: tauri::AppHandle) -> FullConfig {
    let cfg = get_config_from_disk(&app);
    FullConfig {
        hotkey: cfg.hotkey.unwrap_or_else(|| DEFAULT_HOTKEY.to_string()),
        theme: cfg.theme.unwrap_or_else(|| "system".to_string()),
        startup: check_autostart(),
        search_engine: sanitize_engine(cfg.search_engine),
    }
}

#[tauri::command]
fn save_full_config(app: tauri::AppHandle, config: FullConfig) -> Result<(), String> {
    let old_cfg = get_full_config(app.clone());

    // 1. Registration is only touched when the hotkey actually changed (or
    //    when it is not registered at all, e.g. the previous one was taken by
    //    another app). Validation happens *before* the currently working
    //    binding is released, so a bad/conflicting combination can never leave
    //    the user without a shortcut, and config.json is only written once the
    //    new hotkey is live.
    let hotkey_changed = old_cfg.hotkey != config.hotkey;
    let needs_registration = hotkey_changed || registered_shortcut().is_none();
    if needs_registration && !HOTKEY_PAUSED.load(Ordering::SeqCst) {
        // Same lock as set_hotkey_paused: a save must never interleave with
        // a pause/unpause cycle, or the live binding can end up wrong.
        let _guard = HOTKEY_OP_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        // Re-check under the lock - a recording may have started while we
        // waited. In that case skip registration (as before): the unpause
        // that ends the recording restores the binding from disk, and the
        // rest of the config still saves below.
        if !HOTKEY_PAUSED.load(Ordering::SeqCst) {
            let new_shortcut = validate_hotkey_string(&config.hotkey)?;
            let previous = registered_shortcut();
            match register_shortcut(&app, new_shortcut) {
                Ok(()) => {
                    // Only drop the old binding once the new one is live.
                    if let Some(previous) = previous {
                        if previous != new_shortcut {
                            let _ = app.global_shortcut().unregister(previous);
                        }
                    }
                }
                Err(e) => return Err(e),
            }
        }
    }

    // 3. Update autostart
    if old_cfg.startup != config.startup {
        set_autostart(config.startup);
    }

    // 4. Save to disk (only after the hotkey succeeded, so config.json always
    //    describes a hotkey that is actually registered).
    if let Ok(config_dir) = app.path().app_config_dir() {
        let _ = std::fs::create_dir_all(&config_dir);
        let config_path = config_dir.join("config.json");
        let new_app_config = AppConfig {
            hotkey: Some(config.hotkey.clone()),
            theme: Some(config.theme.clone()),
            search_engine: Some(sanitize_engine(Some(config.search_engine.clone()))),
        };
        if let Ok(content) = serde_json::to_string_pretty(&new_app_config) {
            let _ = std::fs::write(&config_path, content);
        }
    }

    // 5. Emit event
    let _ = app.emit("config-changed", config);

    Ok(())
}

/// Checks a hotkey string without side effects (used by the recorder for
/// immediate feedback).
#[tauri::command]
fn validate_hotkey(hotkey: String) -> Result<(), String> {
    validate_hotkey_string(&hotkey).map(|_| ())
}

/// Temporarily releases the global hotkey so the settings recorder can observe
/// the very combination that is currently bound (RegisterHotKey would swallow
/// it otherwise and recording would hang forever).
#[tauri::command]
fn set_hotkey_paused(app: tauri::AppHandle, paused: bool) -> Result<(), String> {
    // Serialize against other pause/unpause/save work so back-to-back
    // recordings always converge to the requested end state. If the mutex
    // is poisoned we proceed anyway - a stuck recorder is worse than a
    // theoretical race.
    let _guard = HOTKEY_OP_LOCK.lock().unwrap_or_else(|p| p.into_inner());

    if paused {
        HOTKEY_PAUSED.store(true, Ordering::SeqCst);
        unregister_registered(&app);
        return Ok(());
    }

    HOTKEY_PAUSED.store(false, Ordering::SeqCst);
    // Restore deterministically: release anything still held, then bind
    // exactly what is on disk. Never trust REGISTERED_SHORTCUT here - a
    // previously raced cycle could have left it out of sync with the OS.
    unregister_registered(&app);
    let cfg = get_full_config(app.clone());
    let shortcut = parse_shortcut(&cfg.hotkey)?;
    register_shortcut(&app, shortcut)?;
    Ok(())
}

#[tauri::command]
fn get_hotkey_string(app: tauri::AppHandle) -> String {
    get_full_config(app).hotkey
}

#[tauri::command]
fn hide_window(app: tauri::AppHandle) {
    hide_main_window(&app);
}

#[tauri::command]
fn copy_to_clipboard(app: tauri::AppHandle, text: String) {
    use tauri_plugin_clipboard_manager::ClipboardExt;
    let _ = app.clipboard().write_text(text);
}

#[tauri::command]
async fn open_settings_window(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("settings") {
        let _ = window.show();
        let _ = window.set_focus();
        force_foreground(&window);
    } else {
        tauri::WebviewWindowBuilder::new(
            &app,
            "settings",
            tauri::WebviewUrl::App("/?settings=true".into())
        )
        .title("Glimpse Settings")
        .inner_size(520.0, 520.0)
        .center()
        .always_on_top(true)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .resizable(false)
        .build()
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
fn close_settings_window(app: tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("settings") {
        let _ = window.close();
    }
}

#[tauri::command]
fn start_settings_window_drag(app: tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("settings") {
        let _ = window.start_dragging();
    }
}

// ---------------------------------------------------------------------------
// Startup
// ---------------------------------------------------------------------------

fn check_and_wait_for_webview2() {
    // Check system uptime - if system just booted (< 60 seconds), wait for WebView2
    let uptime = get_system_uptime_seconds();
    if uptime < 60 {
        println!("System recently booted ({}s). Waiting for WebView2 initialization...", uptime);

        // Wait up to 10 seconds for WebView2 to be ready
        for i in 0..20 {
            if is_webview2_available() {
                println!("WebView2 is available after {}ms", i * 500);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }

        // Additional safety delay
        std::thread::sleep(std::time::Duration::from_secs(2));
    }

    if !is_webview2_available() {
        eprintln!("WARNING: WebView2 Runtime not detected!");
        // Try to install WebView2
        if let Err(e) = Command::new("cmd")
            .args(["/c", "start", "", "https://developer.microsoft.com/en-us/microsoft-edge/webview2/"])
            .spawn()
        {
            eprintln!("Failed to open WebView2 download page: {}", e);
        }
    }
}

fn main() {
    // Check WebView2 and add startup delay before initializing Tauri
    check_and_wait_for_webview2();

    tauri::Builder::default()
        // Must be registered first: a second launch hands the hotkey/focus over
        // to the running instance instead of fighting over it.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            show_main_window(app);
        }))
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_shell::init())
        .setup(|app| {
            let handle = app.handle().clone();

            let current_config = get_full_config(handle.clone());

            // First run (no config.json yet): opt the machine into auto-start
            // so Glimpse is there after every login. From then on the
            // settings checkbox is the source of truth - this never runs again
            // once any config has been saved.
            if config_file_missing(&handle) && !current_config.startup {
                set_autostart(true);
            }

            // Pre-build index on startup (background thread)
            std::thread::spawn(|| {
                indexer::build_index();
            });

            // System Tray - left click opens the palette, right click opens the menu
            let show_item = MenuItem::with_id(app, "show", "Show Glimpse", true, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let tray_menu = Menu::with_items(app, &[&show_item, &quit_item])?;

            let _tray = TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
                .menu(&tray_menu)
                .tooltip("Glimpse")
                .show_menu_on_left_click(false)
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        ..
                    } = event
                    {
                        show_main_window(tray.app_handle());
                    }
                })
                .on_menu_event(move |app, event| {
                    match event.id.as_ref() {
                        "quit" => {
                            app.exit(0);
                        }
                        "show" => {
                            show_main_window(app);
                        }
                        _ => {}
                    }
                })
                .build(app)?;

            // Register global hotkey. A failure here (hotkey already taken by
            // another app) must not kill the app - the tray still works.
            let shortcut = match parse_shortcut(&current_config.hotkey) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("Invalid stored hotkey, falling back to {DEFAULT_HOTKEY}: {e}");
                    Shortcut::new(Some(Modifiers::CONTROL), Code::Space)
                }
            };
            if let Err(e) = register_shortcut(&handle, shortcut) {
                eprintln!("Global hotkey registration failed: {e}");
            }

            Ok(())
        })
        .on_window_event(|window, event| match event {
            // Hide window on close instead of quitting
            WindowEvent::CloseRequested { api, .. } if window.label() == "main" => {
                let _ = window.hide();
                api.prevent_close();
            }
            // Hide when focus is lost (only for main)
            WindowEvent::Focused(false) if window.label() == "main" => {
                // Ignore the focus flicker that can happen while the window is
                // still being brought to the foreground after a hotkey press,
                // otherwise the palette vanishes before you can type in it.
                let just_shown = SHOWN_AT
                    .lock()
                    .ok()
                    .and_then(|shown_at| *shown_at)
                    .map(|shown_at| shown_at.elapsed() < SHOW_FOCUS_GRACE)
                    .unwrap_or(false);
                if !just_shown {
                    let _ = window.hide();
                }
            }
            // Re-focus the search box whenever the user comes back to the window
            WindowEvent::Focused(true) if window.label() == "main" => {
                let _ = window.emit("window-focused", ());
            }
            _ => {}
        })
        .invoke_handler(tauri::generate_handler![
            search::search_items,
            launcher::launch_item,
            launcher::open_path,
            launcher::kill_process,
            indexer::get_index_status,
            get_hotkey_string,
            hide_window,
            copy_to_clipboard,
            get_full_config,
            save_full_config,
            validate_hotkey,
            set_hotkey_paused,
            open_settings_window,
            close_settings_window,
            start_settings_window_drag,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
