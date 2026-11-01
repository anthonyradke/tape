//! What the window and the tray can do.

use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, UNIX_EPOCH};

use serde::Serialize;
use tape_core::ipc::{Command, Probe, Status};
use tape_core::settings::Settings;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_dialog::DialogExt;

use crate::cue::{self, Cue};
use crate::hotkeys;
use crate::state::{self, Health, Shared};
use crate::{supervisor, tray};

const RECENT: usize = 40;
/// It normally takes about 20 seconds.
const SELF_TEST_LIMIT: Duration = Duration::from_secs(90);

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClipFile {
    path: String,
    name: String,
    bytes: u64,
    modified: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct View {
    settings: Settings,
    status: Status,
    probe: Probe,
    health: Health,
    hotkey_errors: Vec<String>,
    folder: String,
    clips: Vec<ClipFile>,
    autostart: bool,
    overlay_running: bool,
    replay_ram: u64,
    version: &'static str,
    log_path: String,
    self_test: Option<String>,
}

fn recent_clips(folder: &Path) -> Vec<ClipFile> {
    let Ok(entries) = std::fs::read_dir(folder) else { return Vec::new() };
    let mut clips: Vec<ClipFile> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x.eq_ignore_ascii_case("mp4")))
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            let modified = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_millis() as u64;
            Some(ClipFile {
                path: e.path().display().to_string(),
                name: e.file_name().to_string_lossy().into_owned(),
                bytes: meta.len(),
                modified,
            })
        })
        .collect();
    clips.sort_by_key(|c| std::cmp::Reverse(c.modified));
    clips.truncate(RECENT);
    clips
}

/// The NVIDIA overlay holds the same default keys, and its replay costs what tape is here to save.
#[cfg(windows)]
fn overlay_running() -> bool {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
    };
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return false };
        let mut entry = PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut found = false;
        let mut more = Process32FirstW(snapshot, &mut entry).is_ok();
        while more {
            let end = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(0);
            let name = String::from_utf16_lossy(&entry.szExeFile[..end]).to_lowercase();
            if name == "nvidia overlay.exe" || name == "nvsphelper64.exe" {
                found = true;
                break;
            }
            more = Process32NextW(snapshot, &mut entry).is_ok();
        }
        let _ = CloseHandle(snapshot);
        found
    }
}

#[cfg(not(windows))]
fn overlay_running() -> bool {
    false
}

#[tauri::command]
pub fn view(app: AppHandle) -> View {
    let shared = app.state::<Shared>();
    let settings = shared.settings.lock().unwrap().clone();
    let folder = shared.folder();
    let view = View {
        replay_ram: settings.replay_ram_bytes(),
        status: shared.status.lock().unwrap().clone(),
        probe: shared.probe.lock().unwrap().clone(),
        health: shared.health.lock().unwrap().clone(),
        hotkey_errors: shared.hotkey_errors.lock().unwrap().clone(),
        clips: recent_clips(&folder),
        folder: folder.display().to_string(),
        autostart: app.autolaunch().is_enabled().unwrap_or(false),
        overlay_running: overlay_running(),
        version: env!("CARGO_PKG_VERSION"),
        log_path: shared.log_path.display().to_string(),
        self_test: shared.self_test.lock().unwrap().clone(),
        settings,
    };
    view
}

#[tauri::command]
pub fn save_settings(app: AppHandle, settings: Settings) -> View {
    let shared = app.state::<Shared>();
    let settings = settings.normalized();
    let keys_changed = shared.settings.lock().unwrap().hotkeys != settings.hotkeys;
    *shared.settings.lock().unwrap() = settings.clone();
    shared.persist();
    state::send(&app, &Command::Configure { settings: Box::new(settings) });
    if keys_changed {
        hotkeys::register(&app);
    }
    tray::refresh(&app);
    view(app)
}

pub fn do_save_clip(app: &AppHandle) {
    state::send(app, &Command::SaveClip);
}

pub fn do_toggle_recording(app: &AppHandle) {
    let shared = app.state::<Shared>();
    let sounds = shared.settings.lock().unwrap().sounds;
    let was_on = shared.recording_wanted.fetch_xor(true, Ordering::Relaxed);
    if was_on {
        state::send(app, &Command::StopRecording);
    } else {
        state::send(app, &Command::StartRecording);
        if sounds {
            cue::play(Cue::RecordingStarted);
        }
    }
    let _ = app.emit("changed", ());
}

pub fn do_toggle_replay(app: &AppHandle) {
    let shared = app.state::<Shared>();
    let on = {
        let mut settings = shared.settings.lock().unwrap();
        settings.replay = !settings.replay;
        settings.replay
    };
    shared.persist();
    state::send(app, &Command::SetReplay { on });
    tray::refresh(app);
    let _ = app.emit("changed", ());
}

pub fn reveal_folder(app: &AppHandle) {
    let folder = app.state::<Shared>().folder();
    let _ = std::fs::create_dir_all(&folder);
    open_path(&folder);
}

#[cfg(windows)]
fn open_path(path: &Path) {
    let _ = std::process::Command::new("explorer").arg(path).spawn();
}

#[cfg(not(windows))]
fn open_path(path: &Path) {
    let _ = std::process::Command::new("xdg-open").arg(path).spawn();
}

#[tauri::command]
pub fn save_clip(app: AppHandle) {
    do_save_clip(&app);
}

#[tauri::command]
pub fn toggle_recording(app: AppHandle) {
    do_toggle_recording(&app);
}

#[tauri::command]
pub fn toggle_replay(app: AppHandle) {
    do_toggle_replay(&app);
}

#[tauri::command]
pub fn open_folder(app: AppHandle) {
    reveal_folder(&app);
}

/// Only files in the clips folder: this takes a path from the webview.
fn inside_folder(app: &AppHandle, path: &str) -> bool {
    Path::new(path).parent() == Some(app.state::<Shared>().folder().as_path())
}

#[tauri::command]
pub fn open_file(app: AppHandle, path: String) {
    if inside_folder(&app, &path) {
        open_path(Path::new(&path));
    }
}

#[tauri::command]
pub fn reveal_file(app: AppHandle, path: String) {
    if !inside_folder(&app, &path) {
        return;
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // raw_arg: explorer wants /select,"path" as one argument with the quotes inside it.
        let _ = std::process::Command::new("explorer").raw_arg(format!("/select,\"{path}\"")).spawn();
    }
    #[cfg(not(windows))]
    open_path(Path::new(&path).parent().unwrap_or(Path::new(".")));
}

#[tauri::command]
pub fn open_log(app: AppHandle) {
    open_path(&app.state::<Shared>().log_path);
}

/// Async so the dialog runs off the main thread, which it would otherwise deadlock.
#[tauri::command]
pub async fn pick_folder(app: AppHandle) -> Option<String> {
    app.dialog().file().blocking_pick_folder().map(|f| f.to_string())
}

#[tauri::command]
pub fn self_test(app: AppHandle) {
    *app.state::<Shared>().self_test.lock().unwrap() = None;
    std::thread::spawn(move || {
        let shared = app.state::<Shared>();
        let report = run_self_test(&shared);
        *shared.self_test.lock().unwrap() = Some(report.clone());
        let _ = app.emit("selftest", report);
    });
}

/// In a process of its own, so a crash in a driver can't take replay down with it. Replay keeps
/// running throughout, so the report ends with how it's doing, which the test itself can't see.
fn run_self_test(shared: &Shared) -> String {
    let file = std::env::temp_dir().join("tape-self-test.txt");
    let _ = std::fs::remove_file(&file);
    let started = supervisor::own_process().and_then(|mut process| {
        process.arg("--self-test").arg(&file).arg(&shared.settings_path);
        process.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()
    });
    let mut child = match started {
        Ok(child) => child,
        Err(e) => return format!("Couldn't start the self-test: {e}"),
    };
    let began = Instant::now();
    let finished = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if began.elapsed() < SELF_TEST_LIMIT => std::thread::sleep(Duration::from_millis(100)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return format!("The self-test hung and was stopped after {} s.", SELF_TEST_LIMIT.as_secs());
            }
        }
    };
    let report = match std::fs::read_to_string(&file) {
        Ok(report) if finished.success() => report,
        _ => return format!("The self-test crashed before finishing ({finished})."),
    };
    let video = shared.status.lock().unwrap().video.clone();
    let live = if video.running {
        format!("{}, {:.0} fps, {} dropped, last frame {:.1} ms", video.method, video.fps, video.dropped, video.encode_ms)
    } else {
        "not running".to_string()
    };
    format!("{report}\nLive replay\n  {live}\n")
}

#[tauri::command]
pub fn set_autostart(app: AppHandle, enabled: bool) -> bool {
    let launcher = app.autolaunch();
    let _ = if enabled { launcher.enable() } else { launcher.disable() };
    launcher.is_enabled().unwrap_or(false)
}

#[tauri::command]
pub fn pause_hotkeys(app: AppHandle) {
    hotkeys::pause(&app);
}

#[tauri::command]
pub fn resume_hotkeys(app: AppHandle) {
    hotkeys::register(&app);
}
