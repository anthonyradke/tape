use std::sync::Mutex;

use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, Wry};

use crate::commands;
use crate::state::{Light, Shared};
use crate::supervisor;

const TRAY_ID: &str = "tape";

/// What the tray currently shows, to avoid touching it when nothing changed.
static SHOWN: Mutex<Option<(Light, bool, bool, Option<String>)>> = Mutex::new(None);

fn icon(light: Light) -> tauri::Result<Image<'static>> {
    let bytes: &[u8] = match light {
        Light::Idle => include_bytes!("../icons/tray-idle.png"),
        Light::Replay => include_bytes!("../icons/tray-replay.png"),
        Light::Recording => include_bytes!("../icons/tray-recording.png"),
        Light::Trouble => include_bytes!("../icons/tray-trouble.png"),
    };
    Image::from_bytes(bytes)
}

fn build_menu(app: &AppHandle) -> tauri::Result<Menu<Wry>> {
    let shared = app.state::<Shared>();
    let settings = shared.settings.lock().unwrap().clone();
    let recording = shared.status.lock().unwrap().recording;
    let menu = Menu::new(app)?;
    let label = |text: &str, key: &str| if key.is_empty() { text.to_string() } else { format!("{text}\t{key}") };
    let keys = &settings.hotkeys;
    menu.append(&MenuItem::with_id(app, "clip", label("Save clip", &keys.save_clip), settings.replay, None::<&str>)?)?;
    let record = if recording { "Stop recording" } else { "Start recording" };
    menu.append(&MenuItem::with_id(app, "record", label(record, &keys.toggle_recording), true, None::<&str>)?)?;
    menu.append(&CheckMenuItem::with_id(app, "replay", label("Instant replay", &keys.toggle_replay), true, settings.replay, None::<&str>)?)?;
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&MenuItem::with_id(app, "folder", "Open clips folder", true, None::<&str>)?)?;
    menu.append(&MenuItem::with_id(app, "open", "Open tape", true, None::<&str>)?)?;
    menu.append(&MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?)?;
    Ok(menu)
}

pub fn create(app: &AppHandle) -> tauri::Result<()> {
    TrayIconBuilder::with_id(TRAY_ID)
        .icon(icon(Light::Idle)?)
        .tooltip("tape")
        .menu(&build_menu(app)?)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "clip" => commands::do_save_clip(app),
            "record" => commands::do_toggle_recording(app),
            "replay" => commands::do_toggle_replay(app),
            "folder" => commands::reveal_folder(app),
            "open" => crate::show_window(app),
            "quit" => {
                supervisor::shutdown(app);
                app.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                crate::show_window(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

/// Brings the icon, tooltip and menu up to date. Cheap when nothing changed.
pub fn refresh(app: &AppHandle) {
    let shared = app.state::<Shared>();
    let health = shared.health.lock().unwrap().clone();
    let recording = shared.status.lock().unwrap().recording;
    let replay = shared.settings.lock().unwrap().replay;
    let now = (health.light, recording, replay, health.problem.clone());
    {
        let mut shown = SHOWN.lock().unwrap();
        if shown.as_ref() == Some(&now) {
            return;
        }
        *shown = Some(now);
    }
    let Some(tray) = app.tray_by_id(TRAY_ID) else { return };
    let _ = tray.set_icon(icon(health.light).ok());
    let tooltip = match (&health.problem, health.light) {
        (Some(problem), _) => format!("tape: {problem}"),
        (None, Light::Recording) => "tape: recording".to_string(),
        (None, Light::Replay) => "tape: instant replay on".to_string(),
        _ => "tape: off".to_string(),
    };
    let _ = tray.set_tooltip(Some(tooltip));
    if let Ok(menu) = build_menu(app) {
        let _ = tray.set_menu(Some(menu));
    }
}
