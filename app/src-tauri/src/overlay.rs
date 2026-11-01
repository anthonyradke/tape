//! The side panel on Alt+Z, docked to the right of the screen with the game still in view. It's an
//! ordinary always-on-top window, on purpose: anything that hooks into a game's renderer risks an
//! anti-cheat ban. Games in borderless or Windows' default fullscreen show it fine; true exclusive
//! fullscreen drops to the desktop, as with Alt+Tab.
//!
//! The window is excluded from capture so it can't end up in a clip. Its webview only exists while
//! it's in use, so it costs nothing during a game.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use tauri::{AppHandle, Emitter, Manager, Monitor, PhysicalPosition, PhysicalSize, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

use crate::state::Shared;

const LABEL: &str = "overlay";
/// Reopening within this is instant. After it, the webview's memory goes back.
const KEEP_WARM: Duration = Duration::from_secs(300);
/// In logical pixels, so the panel looks the same at any display scaling.
const PANEL_WIDTH: f64 = 380.0;
const PANEL_MAX_HEIGHT: f64 = 780.0;
const PANEL_MARGIN: f64 = 16.0;
/// Share of the monitor's width the player takes while a clip plays.
const PLAYER_SHARE: f64 = 0.72;

/// Counts openings, so a timer can tell the overlay has been used since it started.
static OPENED: AtomicU32 = AtomicU32::new(0);

pub fn toggle(app: &AppHandle) {
    let open = app.get_webview_window(LABEL).is_some_and(|w| w.is_visible().unwrap_or(false));
    if open {
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

/// Against the right edge, centered top to bottom. The monitor can be narrow or turned on its side.
fn place_panel(app: &AppHandle, window: &WebviewWindow) {
    let Some(monitor) = monitor(app) else { return };
    let (at, size, scale) = (monitor.position(), monitor.size(), monitor.scale_factor());
    let margin = (PANEL_MARGIN * scale) as u32;
    let width = ((PANEL_WIDTH * scale) as u32).min(size.width.saturating_sub(margin * 2));
    let height = ((PANEL_MAX_HEIGHT * scale) as u32).min(size.height.saturating_sub(margin * 2));
    let x = at.x + (size.width - width - margin) as i32;
    let y = at.y + ((size.height - height) / 2) as i32;
    place(window, x, y, width, height);
}

/// Centered, wide enough to watch a clip, with room for the player's bar.
fn place_player(app: &AppHandle, window: &WebviewWindow) {
    let Some(monitor) = monitor(app) else { return };
    let (at, size, scale) = (monitor.position(), monitor.size(), monitor.scale_factor());
    let width = (size.width as f64 * PLAYER_SHARE) as u32;
    let bar = 54.0 * scale;
    let height = ((width as f64 * 9.0 / 16.0 + bar) as u32).min((size.height as f64 * 0.9) as u32);
    place(window, at.x + ((size.width - width) / 2) as i32, at.y + ((size.height - height) / 2) as i32, width, height);
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
    let _ = window.show();
    let _ = window.set_focus();
    let _ = app.emit_to(LABEL, "overlay-shown", ());
}

pub fn hide(app: &AppHandle) {
    let Some(window) = app.get_webview_window(LABEL) else { return };
    let _ = app.emit_to(LABEL, "overlay-hidden", ());
    let _ = window.hide();
    restore_foreground();
    let opened = OPENED.load(Ordering::Relaxed);
    std::thread::spawn(move || {
        std::thread::sleep(KEEP_WARM);
        if OPENED.load(Ordering::Relaxed) == opened {
            let _ = window.close();
        }
    });
}

/// For the UI test: whether it's up, whether Windows will leave it out of captures, and whether
/// it's the side panel and not the whole screen.
pub fn state(app: &AppHandle) -> (bool, bool, bool) {
    let Some(window) = app.get_webview_window(LABEL) else { return (false, false, false) };
    let width = window.outer_size().map_or(u32::MAX, |s| s.width);
    let narrow = monitor(app).is_some_and(|m| width < m.size().width / 2);
    (window.is_visible().unwrap_or(false), excluded_from_capture(&window), narrow)
}

#[cfg(windows)]
mod platform {
    use std::sync::atomic::{AtomicIsize, Ordering};

    use tauri::WebviewWindow;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowDisplayAffinity, SetForegroundWindow, WDA_EXCLUDEFROMCAPTURE};

    /// The window to give focus back to.
    static PREVIOUS: AtomicIsize = AtomicIsize::new(0);

    pub fn remember_foreground() {
        PREVIOUS.store(unsafe { GetForegroundWindow() }.0 as isize, Ordering::Relaxed);
    }

    /// Windows usually does this by itself, but not always to the game.
    pub fn restore_foreground() {
        let window = HWND(PREVIOUS.swap(0, Ordering::Relaxed) as *mut _);
        if !window.is_invalid() {
            let _ = unsafe { SetForegroundWindow(window) };
        }
    }

    pub fn excluded_from_capture(window: &WebviewWindow) -> bool {
        let Ok(handle) = window.hwnd() else { return false };
        let mut affinity = 0;
        let got = unsafe { GetWindowDisplayAffinity(HWND(handle.0), &mut affinity) };
        got.is_ok() && affinity == WDA_EXCLUDEFROMCAPTURE.0
    }
}

#[cfg(not(windows))]
mod platform {
    use tauri::WebviewWindow;

    pub fn remember_foreground() {}
    pub fn restore_foreground() {}
    pub fn excluded_from_capture(_window: &WebviewWindow) -> bool {
        false
    }
}

use platform::{excluded_from_capture, remember_foreground, restore_foreground};
