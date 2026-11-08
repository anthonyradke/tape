//! What the window and the tray can do.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
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
use crate::{overlay, supervisor, tray};

const RECENT: usize = 40;
/// Set by CI to run app/ui/uitest.js in a real WebView2. See the workflow.
const UI_TEST: &str = "TAPE_UI_TEST";
/// It normally takes about 20 seconds.
const SELF_TEST_LIMIT: Duration = Duration::from_secs(90);

/// Label for files directly in the clips folder, from before they were sorted by game.
const UNSORTED: &str = "Other";
const MAX_THUMBNAIL: usize = 2 << 20;

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClipFile {
    path: String,
    name: String,
    game: String,
    /// "clip" for instant replay, "recording" otherwise.
    kind: &'static str,
    bytes: u64,
    modified: u64,
    /// Names its thumbnail, which changes if the file does.
    key: String,
    thumb: Option<String>,
    seconds: Option<f64>,
    starred: bool,
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
    ui_test: bool,
}

fn is_mp4(path: &Path) -> bool {
    path.extension().is_some_and(|x| x.eq_ignore_ascii_case("mp4"))
}

fn thumbnail_key(path: &Path, bytes: u64, modified: u64) -> String {
    let mut hasher = DefaultHasher::new();
    (path, bytes, modified).hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Thumbnails by key. Each file is called <key>_<duration in ms>.jpg, so finding them also gives
/// every clip's length without opening it.
pub(crate) fn thumbnails(dir: &Path) -> HashMap<String, (PathBuf, f64)> {
    let Ok(entries) = std::fs::read_dir(dir) else { return HashMap::new() };
    entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let stem = path.file_stem()?.to_str()?;
            let (key, millis) = stem.split_once('_')?;
            Some((key.to_string(), (path.clone(), millis.parse::<f64>().ok()? / 1000.0)))
        })
        .collect()
}

/// The key `add_clips` gives a file, which the share server keeps to match links to clips.
pub(crate) fn clip_key(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_millis() as u64;
    Some(thumbnail_key(path, meta.len(), modified))
}

fn add_clips(dir: &Path, game: &str, known: &HashMap<String, (PathBuf, f64)>, clips: &mut Vec<ClipFile>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        let modified = meta.modified().ok().and_then(|m| m.duration_since(UNIX_EPOCH).ok());
        let (true, Some(modified)) = (is_mp4(&path), modified) else { continue };
        let modified = modified.as_millis() as u64;
        let name = entry.file_name().to_string_lossy().into_owned();
        let key = thumbnail_key(&path, meta.len(), modified);
        let thumb = known.get(&key);
        clips.push(ClipFile {
            kind: if name.to_lowercase().ends_with(".dvr.mp4") { "clip" } else { "recording" },
            thumb: thumb.map(|(p, _)| p.display().to_string()),
            seconds: thumb.map(|(_, s)| *s),
            path: path.display().to_string(),
            starred: false,
            game: game.to_string(),
            bytes: meta.len(),
            modified,
            name,
            key,
        });
    }
}

/// Everything saved, newest first: the clips folder and one level of game folders. Skips the
/// recording in progress, which isn't a finished video yet.
fn all_clips(app: &AppHandle) -> Vec<ClipFile> {
    let shared = app.state::<Shared>();
    let stars = app.state::<crate::library::Stars>();
    let folder = shared.folder();
    let known = thumbnails(&shared.thumbnails);
    let mut clips = Vec::new();
    add_clips(&folder, UNSORTED, &known, &mut clips);
    for entry in std::fs::read_dir(&folder).into_iter().flatten().flatten() {
        if entry.path().is_dir() {
            add_clips(&entry.path(), &entry.file_name().to_string_lossy(), &known, &mut clips);
        }
    }
    let writing = shared.status.lock().unwrap().recording_path.clone();
    clips.retain(|c| Some(&c.path) != writing.as_ref());
    for c in &mut clips {
        c.starred = stars.has(&c.path);
    }
    clips.sort_by_key(|c| std::cmp::Reverse(c.modified));
    clips
}

/// Lets the player and thumbnails read these, and nothing else on disk.
pub fn allow_media(app: &AppHandle) {
    let shared = app.state::<Shared>();
    let scope = app.asset_protocol_scope();
    for dir in [shared.folder(), shared.thumbnails.clone()] {
        let _ = std::fs::create_dir_all(&dir);
        let _ = scope.allow_directory(&dir, true);
    }
}

#[tauri::command]
pub fn clips(app: AppHandle) -> Vec<ClipFile> {
    all_clips(&app)
}

/// `key` comes from the webview, so it has to look like one before it goes in a path.
#[tauri::command]
pub fn save_thumb(app: AppHandle, key: String, seconds: f64, jpeg: Vec<u8>) -> Option<String> {
    if key.is_empty() || !key.chars().all(|c| c.is_ascii_hexdigit()) || jpeg.len() > MAX_THUMBNAIL {
        return None;
    }
    let shared = app.state::<Shared>();
    let path = shared.thumbnails.join(format!("{key}_{}.jpg", (seconds * 1000.0) as u64));
    std::fs::write(&path, jpeg).ok()?;
    Some(path.display().to_string())
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
    let mut clips = all_clips(&app);
    clips.truncate(RECENT);
    let view = View {
        replay_ram: settings.replay_ram_bytes(),
        status: shared.status.lock().unwrap().clone(),
        probe: shared.probe.lock().unwrap().clone(),
        health: shared.health.lock().unwrap().clone(),
        hotkey_errors: shared.hotkey_errors.lock().unwrap().clone(),
        clips,
        folder: folder.display().to_string(),
        autostart: app.autolaunch().is_enabled().unwrap_or(false),
        overlay_running: overlay_running(),
        version: env!("CARGO_PKG_VERSION"),
        log_path: shared.log_path.display().to_string(),
        self_test: shared.self_test.lock().unwrap().clone(),
        ui_test: std::env::var_os(UI_TEST).is_some(),
        settings,
    };
    view
}

#[tauri::command]
pub fn save_settings(app: AppHandle, settings: Settings) -> View {
    let shared = app.state::<Shared>();
    let settings = settings.normalized();
    let keys_changed = shared.settings.lock().unwrap().hotkeys != settings.hotkeys;
    let folder_changed = shared.settings.lock().unwrap().folder != settings.folder;
    let server_changed = {
        let old = shared.settings.lock().unwrap();
        old.share_server != settings.share_server || old.share_key != settings.share_key
    };
    *shared.settings.lock().unwrap() = settings.clone();
    shared.persist();
    state::send(&app, &Command::Configure { settings: Box::new(settings) });
    if keys_changed {
        hotkeys::register(&app);
    }
    if folder_changed {
        allow_media(&app);
    }
    if server_changed {
        crate::share::forget(&app);
    }
    let _ = app.emit("changed", ());
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

/// Only videos in the clips folder or a game folder directly under it: this takes a path from the
/// webview. Comparing exact parents leaves no way to climb out with "..".
pub(crate) fn inside_folder(app: &AppHandle, path: &str) -> bool {
    let folder = app.state::<Shared>().folder();
    let path = Path::new(path);
    let parent = path.parent();
    is_mp4(path) && (parent == Some(folder.as_path()) || parent.and_then(Path::parent) == Some(folder.as_path()))
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
pub fn hide_overlay(app: AppHandle) {
    overlay::hide(&app);
}

#[tauri::command]
pub fn overlay_player(app: AppHandle, open: bool) {
    overlay::set_player(&app, open);
}

/// Async because making a window from a synchronous command deadlocks on Windows.
#[tauri::command]
pub async fn open_main(app: AppHandle) {
    overlay::hide(&app);
    crate::show_window(&app);
}

/// Opens or closes the overlay as the hotkey would, and says what happened. Async for the same
/// reason as `open_main`.
#[tauri::command]
pub async fn ui_test_overlay(app: AppHandle) -> Option<(bool, bool, bool, bool)> {
    std::env::var_os(UI_TEST)?;
    overlay::toggle(&app);
    std::thread::sleep(Duration::from_millis(1500));
    Some(overlay::state(&app))
}

/// Ends a run started with the UI test variable set, which holds where to write the results.
#[tauri::command]
pub fn ui_test_done(app: AppHandle, report: String) {
    let Some(path) = std::env::var_os(UI_TEST) else { return };
    let _ = std::fs::write(path, report);
    supervisor::shutdown(&app);
    app.exit(0);
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
