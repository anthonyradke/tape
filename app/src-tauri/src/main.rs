#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod cue;
mod hotkeys;
mod state;
mod supervisor;
mod tray;

use std::path::Path;

use tauri::{AppHandle, Manager, RunEvent, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};

const HIDDEN_ARG: &str = "--minimized";

/// Show the window, creating it if it was closed. Closing destroys the webview, so only the tray
/// and the engine stay resident during a game.
pub fn show_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
        return;
    }
    state::send(app, &tape_core::ipc::Command::Probe);
    let _ = WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
        .title("tape")
        .inner_size(1040.0, 720.0)
        .min_inner_size(900.0, 560.0)
        // Frameless: the UI draws its own title bar.
        .decorations(false)
        .shadow(true)
        .theme(Some(tauri::Theme::Dark))
        .background_color(tauri::window::Color(20, 21, 23, 255))
        .build();
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        // The same executable runs the engine, with no UI loaded, under the supervisor.
        Some("--engine") => {
            if let Some(path) = args.get(2) {
                tape_engine::log::init(Path::new(path));
            }
            std::process::exit(tape_engine::main());
        }
        Some("--self-test") => {
            let path = args.get(2).map_or("tape-self-test.txt", String::as_str);
            let settings = args.get(3).map(Path::new);
            std::process::exit(tape_engine::self_test(Path::new(path), settings));
        }
        _ => {}
    }
    let start_hidden = args.iter().any(|a| a == HIDDEN_ARG);
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| show_window(app)))
        .plugin(tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, Some(vec![HIDDEN_ARG])))
        .plugin(tauri_plugin_dialog::init())
        .plugin(hotkeys::plugin())
        .setup(move |app| {
            let handle = app.handle();
            let (shared, first_run) = state::Shared::load(handle);
            app.manage(shared);
            if first_run {
                // Replay is only useful if it's already running when something happens.
                let _ = app.autolaunch().enable();
            }
            hotkeys::register(handle);
            tray::create(handle)?;
            supervisor::start(handle.clone());
            if !start_hidden {
                show_window(handle);
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::view,
            commands::save_settings,
            commands::save_clip,
            commands::toggle_recording,
            commands::toggle_replay,
            commands::open_folder,
            commands::open_file,
            commands::reveal_file,
            commands::open_log,
            commands::pick_folder,
            commands::self_test,
            commands::set_autostart,
            commands::pause_hotkeys,
            commands::resume_hotkeys,
        ])
        .build(tauri::generate_context!())
        .expect("failed to start")
        .run(|_app, event| {
            // Closing the window isn't quitting: replay keeps running from the tray.
            if let RunEvent::ExitRequested { api, code, .. } = event {
                if code.is_none() {
                    api.prevent_exit();
                }
            }
        });
}
