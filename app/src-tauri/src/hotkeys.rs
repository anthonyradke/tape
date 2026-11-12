//! Global keys. These work through RegisterHotKey, which fires while a fullscreen game has focus.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use tape_core::settings::Hotkeys;
use tauri::plugin::TauriPlugin;
use tauri::{AppHandle, Emitter, Manager, Wry};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

use crate::{commands, overlay};
use crate::state::Shared;

#[derive(Clone, Copy)]
enum Action {
    SaveClip,
    ToggleRecording,
    ToggleReplay,
    Overlay,
    Escape,
}

static BINDINGS: Mutex<Vec<(u32, Action)>> = Mutex::new(Vec::new());
/// Which pause is running, 0 for none. Counted so a timer left from an earlier pause can't end a later one.
static PAUSED: AtomicU64 = AtomicU64::new(0);
static PAUSES: AtomicU64 = AtomicU64::new(0);
/// A key capture nobody finished (the window closed or a game took focus) gives the keys back after this.
const PAUSE_LIMIT: Duration = Duration::from_secs(30);

pub fn plugin() -> TauriPlugin<Wry> {
    tauri_plugin_global_shortcut::Builder::new()
        .with_handler(|app, shortcut, event| {
            if event.state() != ShortcutState::Pressed {
                return;
            }
            let action = BINDINGS.lock().unwrap().iter().find(|(id, _)| *id == shortcut.id()).map(|(_, a)| *a);
            match action {
                Some(Action::SaveClip) => commands::do_save_clip(app),
                Some(Action::ToggleRecording) => commands::do_toggle_recording(app),
                Some(Action::ToggleReplay) => commands::do_toggle_replay(app),
                Some(Action::Overlay) => overlay::toggle(app),
                Some(Action::Escape) => {
                    let _ = app.emit_to("overlay", "overlay-escape", ());
                }
                None => {}
            }
        })
        .build()
}

/// Re-registers everything from the settings. A key that's taken (the NVIDIA overlay still
/// holding Alt+F10, say) goes in the errors list for the UI rather than failing silently.
pub fn register(app: &AppHandle) {
    PAUSED.store(0, Ordering::Relaxed);
    let shared = app.state::<Shared>();
    let keys: Hotkeys = shared.settings.lock().unwrap().hotkeys.clone();
    let manager = app.global_shortcut();
    let _ = manager.unregister_all();
    let mut bindings = Vec::new();
    let mut errors = Vec::new();
    let wanted = [
        (&keys.save_clip, Action::SaveClip),
        (&keys.toggle_recording, Action::ToggleRecording),
        (&keys.toggle_replay, Action::ToggleReplay),
        (&keys.overlay, Action::Overlay),
    ];
    for (text, action) in wanted {
        if text.is_empty() {
            continue;
        }
        match text.parse::<Shortcut>() {
            Ok(shortcut) => match manager.register(shortcut) {
                Ok(()) => bindings.push((shortcut.id(), action)),
                Err(_) => errors.push(text.clone()),
            },
            Err(_) => errors.push(text.clone()),
        }
    }
    *BINDINGS.lock().unwrap() = bindings;
    *shared.hotkey_errors.lock().unwrap() = errors;
    if overlay::is_open(app) {
        grab_escape(app, true);
    }
}

/// The overlay doesn't take focus from the game, so Esc only reaches it like this. Held only
/// while it's open, so the game gets Esc back after.
pub fn grab_escape(app: &AppHandle, on: bool) {
    let Ok(escape) = "Escape".parse::<Shortcut>() else { return };
    let manager = app.global_shortcut();
    // Not holding the lock through these: they wait on the main thread, which may be waiting on it.
    BINDINGS.lock().unwrap().retain(|(_, a)| !matches!(a, Action::Escape));
    if on {
        if manager.register(escape).is_ok() {
            BINDINGS.lock().unwrap().push((escape.id(), Action::Escape));
        }
    } else if manager.is_registered(escape) {
        let _ = manager.unregister(escape);
    }
}

/// While the UI listens for a new key, none of them fire. The UI ends the pause, and in case it
/// never does, so do the main window losing focus or closing (`resume_if_paused`) and a timer.
pub fn pause(app: &AppHandle) {
    let pause = PAUSES.fetch_add(1, Ordering::Relaxed) + 1;
    PAUSED.store(pause, Ordering::Relaxed);
    let _ = app.global_shortcut().unregister_all();
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(PAUSE_LIMIT);
        if PAUSED.load(Ordering::Relaxed) == pause {
            register(&app);
        }
    });
}

pub fn resume_if_paused(app: &AppHandle) {
    if PAUSED.load(Ordering::Relaxed) != 0 {
        register(app);
    }
}
