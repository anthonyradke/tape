//! The panel on Alt+Z. It's an ordinary always-on-top window, on purpose: anything that hooks into
//! a game's renderer risks an anti-cheat ban. Games in borderless or Windows' default fullscreen
//! show it fine; true exclusive fullscreen drops to the desktop, as with Alt+Tab.
//!
//! The window is excluded from capture so it can't end up in a clip. Its webview only exists while
//! it's in use, so it costs nothing during a game.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

use crate::state::Shared;

const LABEL: &str = "overlay";
/// Reopening within this is instant. After it, the webview's memory goes back.
const KEEP_WARM: Duration = Duration::from_secs(300);

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
        .transparent(true)
        .shadow(false)
        .resizable(false)
        .skip_taskbar(true)
        .always_on_top(true)
        .content_protected(true)
        .visible(false)
        .build()
        .ok()
}

/// The monitor being captured, which is where the game is.
fn place(app: &AppHandle, window: &WebviewWindow) {
    let wanted = app.state::<Shared>().settings.lock().unwrap().monitor.clone();
    let monitors = app.available_monitors().unwrap_or_default();
    let chosen = monitors.into_iter().find(|m| m.name().is_some_and(|n| *n == wanted)).or_else(|| app.primary_monitor().ok().flatten());
    if let Some(monitor) = chosen {
        let (at, size) = (monitor.position(), monitor.size());
        let _ = window.set_position(PhysicalPosition::new(at.x, at.y));
        let _ = window.set_size(PhysicalSize::new(size.width, size.height));
    }
}

fn show(app: &AppHandle) {
    let Some(window) = app.get_webview_window(LABEL).or_else(|| build(app)) else { return };
    OPENED.fetch_add(1, Ordering::Relaxed);
    remember_foreground();
    place(app, &window);
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

/// For the UI test: whether it's up, and whether Windows will leave it out of captures.
pub fn state(app: &AppHandle) -> (bool, bool) {
    let Some(window) = app.get_webview_window(LABEL) else { return (false, false) };
    (window.is_visible().unwrap_or(false), excluded_from_capture(&window))
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
