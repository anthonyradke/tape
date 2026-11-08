//! The side panel on Alt+Z, docked to the right of the screen with the game still in view. It's an
//! ordinary always-on-top window, on purpose: anything that hooks into a game's renderer risks an
//! anti-cheat ban. It opens without taking focus, since a game in fullscreen drops out of it the
//! moment it loses focus, as with Alt+Enter. So the game keeps the keyboard, and Esc reaches the
//! panel as a global key while it's open. True exclusive fullscreen doesn't show it at all.
//!
//! The window is excluded from capture so it can't end up in a clip. Its webview only exists while
//! it's in use, so it costs nothing during a game.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use tauri::{AppHandle, Emitter, Manager, Monitor, PhysicalPosition, PhysicalSize, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

use crate::hotkeys;
use crate::state::Shared;

const LABEL: &str = "overlay";
/// Reopening within this is instant. After it, the webview's memory goes back.
const KEEP_WARM: Duration = Duration::from_secs(300);
/// In logical pixels, so the panel looks the same at any display scaling.
const PANEL_WIDTH: f64 = 380.0;
const PANEL_MAX_HEIGHT: f64 = 780.0;
const PANEL_MARGIN: f64 = 16.0;
/// Share of the monitor's width the player takes while a clip plays, beside the panel.
const PLAYER_SHARE: f64 = 0.72;

/// Counts openings, so a timer can tell the overlay has been used since it started.
static OPENED: AtomicU32 = AtomicU32::new(0);

pub fn is_open(app: &AppHandle) -> bool {
    app.get_webview_window(LABEL).is_some_and(|w| w.is_visible().unwrap_or(false))
}

pub fn toggle(app: &AppHandle) {
    if is_open(app) {
        hide(app);
    } else {
        show(app);
    }
}

fn build(app: &AppHandle) -> Option<WebviewWindow> {
    WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("overlay.html".into()))
        .title("tape overlay")
        .decorations(false)
        .shadow(true)
        .theme(Some(tauri::Theme::Dark))
        .background_color(tauri::window::Color(20, 21, 23, 255))
        .resizable(false)
        .skip_taskbar(true)
        .always_on_top(true)
        .focusable(false)
        // Otherwise WebView2 pulls the focus in once it has loaded, which activates the window.
        .focused(false)
        .content_protected(true)
        .visible(false)
        .build()
        .ok()
}

/// The monitor being captured, which is where the game is.
fn monitor(app: &AppHandle) -> Option<Monitor> {
    let wanted = app.state::<Shared>().settings.lock().unwrap().monitor.clone();
    let monitors = app.available_monitors().unwrap_or_default();
    monitors.into_iter().find(|m| m.name().is_some_and(|n| *n == wanted)).or_else(|| app.primary_monitor().ok().flatten())
}

fn place(window: &WebviewWindow, x: i32, y: i32, width: u32, height: u32) {
    let _ = window.set_position(PhysicalPosition::new(x, y));
    let _ = window.set_size(PhysicalSize::new(width, height));
}

/// Where the panel goes: against the right edge, centered top to bottom. The monitor can be narrow
/// or turned on its side.
fn panel_rect(monitor: &Monitor) -> (i32, i32, u32, u32) {
    let (at, size, scale) = (monitor.position(), monitor.size(), monitor.scale_factor());
    let margin = (PANEL_MARGIN * scale) as u32;
    let width = ((PANEL_WIDTH * scale) as u32).min(size.width.saturating_sub(margin * 2));
    let height = ((PANEL_MAX_HEIGHT * scale) as u32).min(size.height.saturating_sub(margin * 2));
    let x = at.x + (size.width - width - margin) as i32;
    let y = at.y + ((size.height - height) / 2) as i32;
    (x, y, width, height)
}

fn place_panel(app: &AppHandle, window: &WebviewWindow) {
    let Some(monitor) = monitor(app) else { return };
    let (x, y, width, height) = panel_rect(&monitor);
    place(window, x, y, width, height);
}

/// The panel stays where it is, dimmed, and the window grows to its left to hold the player, with
/// room for the player's bar. overlay.html lays it out by the panel's width.
fn place_player(app: &AppHandle, window: &WebviewWindow) {
    let Some(monitor) = monitor(app) else { return };
    let (panel_x, _, panel_width, panel_height) = panel_rect(&monitor);
    let (at, size, scale) = (monitor.position(), monitor.size(), monitor.scale_factor());
    let room = (panel_x - at.x) as u32;
    let player = ((size.width as f64 * PLAYER_SHARE) as u32).min(room.saturating_sub((PANEL_MARGIN * scale) as u32));
    let bar = 54.0 * scale;
    let height = ((player as f64 * 9.0 / 16.0 + bar) as u32).min((size.height as f64 * 0.9) as u32).max(panel_height);
    let y = at.y + ((size.height - height) / 2) as i32;
    place(window, panel_x - player as i32, y, player + panel_width, height);
}

/// The panel is too narrow to watch anything in, so it grows while a clip plays.
pub fn set_player(app: &AppHandle, open: bool) {
    let Some(window) = app.get_webview_window(LABEL) else { return };
    if open {
        place_player(app, &window);
    } else {
        place_panel(app, &window);
    }
}

fn show(app: &AppHandle) {
    let Some(window) = app.get_webview_window(LABEL).or_else(|| build(app)) else { return };
    OPENED.fetch_add(1, Ordering::Relaxed);
    remember_foreground();
    place_panel(app, &window);
    show_without_focus(&window);
    hotkeys::grab_escape(app, true);
    let _ = app.emit_to(LABEL, "overlay-shown", ());
    close_when_game_left(app, OPENED.load(Ordering::Relaxed));
}

/// The panel never has focus, so it can't tell from a blur that the player Alt+Tabbed away.
fn close_when_game_left(app: &AppHandle, opened: u32) {
    let app = app.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(250));
        let Some(window) = app.get_webview_window(LABEL) else { return };
        if OPENED.load(Ordering::Relaxed) != opened || !is_open(&app) {
            return;
        }
        if focus_moved_on(&window) {
            hide(&app);
            return;
        }
    });
}

pub fn hide(app: &AppHandle) {
    let Some(window) = app.get_webview_window(LABEL) else { return };
    let _ = app.emit_to(LABEL, "overlay-hidden", ());
    hotkeys::grab_escape(app, false);
    // Only if the panel took the focus, from a click say. If the player went to another window,
    // they stay there.
    let had_focus = is_foreground(&window);
    hide_window(&window);
    if had_focus {
        give_focus_back();
    }
    let opened = OPENED.load(Ordering::Relaxed);
    std::thread::spawn(move || {
        std::thread::sleep(KEEP_WARM);
        if OPENED.load(Ordering::Relaxed) == opened {
            let _ = window.close();
        }
    });
}

/// For the UI test: whether it's up, whether Windows will leave it out of captures, whether it's
/// the side panel and not the whole screen, and whether it left the focus where it was.
pub fn state(app: &AppHandle) -> (bool, bool, bool, bool) {
    let Some(window) = app.get_webview_window(LABEL) else { return (false, false, false, true) };
    let width = window.outer_size().map_or(u32::MAX, |s| s.width);
    let narrow = monitor(app).is_some_and(|m| width < m.size().width / 2);
    (window.is_visible().unwrap_or(false), excluded_from_capture(&window), narrow, !is_foreground(&window))
}

#[cfg(windows)]
mod platform {
    use std::sync::atomic::{AtomicIsize, Ordering};

    use tauri::WebviewWindow;
    use windows::core::PWSTR;
    use windows::Win32::Foundation::{CloseHandle, HWND};
    use windows::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetClassNameW, GetForegroundWindow, GetWindowDisplayAffinity, GetWindowThreadProcessId, SetForegroundWindow, SetWindowPos,
        ShowWindow, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_HIDE, SW_SHOWNOACTIVATE, WDA_EXCLUDEFROMCAPTURE,
    };

    /// Processes that host the shell's flyouts: quick settings and the sound output picker
    /// (Win+Ctrl+V), the volume and notification flyouts, Start and search.
    const SHELL_HOSTS: [&str; 4] = ["shellexperiencehost.exe", "shellhost.exe", "startmenuexperiencehost.exe", "searchhost.exe"];
    /// Explorer's own popups: the taskbar, the tray overflow, the Alt+Tab switcher and the input
    /// switcher. Not its folder windows, or the desktop.
    const SHELL_CLASSES: [&str; 7] = [
        "Shell_TrayWnd",
        "Shell_SecondaryTrayWnd",
        "TopLevelWindowForOverflowXamlIsland",
        "NotifyIconOverflowWindow",
        "XamlExplorerHostIslandWindow",
        "MultitaskingViewFrame",
        "Shell_InputSwitchTopLevelWindow",
    ];

    /// The window that had focus when the panel opened, normally the game.
    static PREVIOUS: AtomicIsize = AtomicIsize::new(0);

    fn hwnd(window: &WebviewWindow) -> Option<HWND> {
        window.hwnd().ok().map(|h| HWND(h.0))
    }

    fn previous() -> HWND {
        HWND(PREVIOUS.load(Ordering::Relaxed) as *mut _)
    }

    pub fn remember_foreground() {
        PREVIOUS.store(unsafe { GetForegroundWindow() }.0 as isize, Ordering::Relaxed);
    }

    /// Tauri's show() activates the window, which is the one thing this mustn't do. Tauri keeps
    /// thinking it's hidden, which is harmless as long as hide_window is what hides it again.
    pub fn show_without_focus(window: &WebviewWindow) {
        let Some(handle) = hwnd(window) else { return };
        unsafe {
            let _ = ShowWindow(handle, SW_SHOWNOACTIVATE);
            let _ = SetWindowPos(handle, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        }
    }

    pub fn hide_window(window: &WebviewWindow) {
        let Some(handle) = hwnd(window) else { return };
        let _ = unsafe { ShowWindow(handle, SW_HIDE) };
    }

    pub fn is_foreground(window: &WebviewWindow) -> bool {
        hwnd(window).is_some_and(|h| unsafe { GetForegroundWindow() } == h)
    }

    fn class_name(window: HWND) -> String {
        let mut buffer = [0u16; 256];
        let len = unsafe { GetClassNameW(window, &mut buffer) } as usize;
        String::from_utf16_lossy(&buffer[..len])
    }

    fn process_name(window: HWND) -> String {
        let mut pid = 0;
        unsafe { GetWindowThreadProcessId(window, Some(&mut pid)) };
        let Ok(process) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }) else { return String::new() };
        let mut buffer = [0u16; 1024];
        let mut len = buffer.len() as u32;
        let got = unsafe { QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(buffer.as_mut_ptr()), &mut len) };
        let _ = unsafe { CloseHandle(process) };
        if got.is_err() {
            return String::new();
        }
        let path = String::from_utf16_lossy(&buffer[..len as usize]);
        path.rsplit('\\').next().unwrap_or_default().to_lowercase()
    }

    /// A shell popup over the game, like the sound output picker, which goes away again by itself.
    fn is_shell_popup(window: HWND) -> bool {
        SHELL_CLASSES.contains(&class_name(window).as_str()) || SHELL_HOSTS.contains(&process_name(window).as_str())
    }

    /// Something other than the game and the panel came to the front, Alt+Tab say. Shell popups
    /// don't count, so changing the sound output with Win+Ctrl+V leaves a clip playing.
    pub fn focus_moved_on(window: &WebviewWindow) -> bool {
        let now = unsafe { GetForegroundWindow() };
        !now.is_invalid() && now != previous() && !is_foreground(window) && !is_shell_popup(now)
    }

    /// Windows usually does this by itself, but not always to the game.
    pub fn give_focus_back() {
        let game = previous();
        if !game.is_invalid() {
            let _ = unsafe { SetForegroundWindow(game) };
        }
    }

    pub fn excluded_from_capture(window: &WebviewWindow) -> bool {
        let Some(handle) = hwnd(window) else { return false };
        let mut affinity = 0;
        let got = unsafe { GetWindowDisplayAffinity(handle, &mut affinity) };
        got.is_ok() && affinity == WDA_EXCLUDEFROMCAPTURE.0
    }
}

#[cfg(not(windows))]
mod platform {
    use tauri::WebviewWindow;

    pub fn remember_foreground() {}
    pub fn show_without_focus(window: &WebviewWindow) {
        let _ = window.show();
    }
    pub fn hide_window(window: &WebviewWindow) {
        let _ = window.hide();
    }
    pub fn is_foreground(window: &WebviewWindow) -> bool {
        window.is_focused().unwrap_or(false)
    }
    pub fn focus_moved_on(_window: &WebviewWindow) -> bool {
        false
    }
    pub fn give_focus_back() {}
    pub fn excluded_from_capture(_window: &WebviewWindow) -> bool {
        false
    }
}

use platform::{excluded_from_capture, focus_moved_on, give_focus_back, hide_window, is_foreground, remember_foreground, show_without_focus};
