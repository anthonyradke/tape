//! Global keys. These work through RegisterHotKey, which fires while a fullscreen game has focus.

use std::sync::Mutex;

use tape_core::settings::Hotkeys;
use tauri::plugin::TauriPlugin;
use tauri::{AppHandle, Manager, Wry};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

use crate::{commands, overlay};
use crate::state::Shared;

#[derive(Clone, Copy)]
enum Action {
    SaveClip,
    ToggleRecording,
    ToggleReplay,
    Overlay,
}

static BINDINGS: Mutex<Vec<(u32, Action)>> = Mutex::new(Vec::new());

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
                None => {}
            }
        })
        .build()
}

/// Re-registers everything from the settings. A key that's taken (the NVIDIA overlay still
/// holding Alt+F10, say) goes in the errors list for the UI rather than failing silently.
pub fn register(app: &AppHandle) {
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
}

/// While the UI listens for a new key, none of them fire.
pub fn pause(app: &AppHandle) {
    let _ = app.global_shortcut().unregister_all();
}
