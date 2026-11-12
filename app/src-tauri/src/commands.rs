//! What the window and the tray can do.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
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
    pub(crate) path: String,
    name: String,
    game: String,
    /// "clip" for instant replay, "recording" otherwise.
    kind: &'static str,
    bytes: u64,
    modified: u64,
    /// Names its thumbnail, which changes if the file does.
    pub(crate) key: String,
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

/// 64-bit FNV-1a. Spelled out rather than std's hasher, whose output may change with any Rust
/// release: these keys name files on disk and links on the share server.
fn fnv1a(parts: &[&[u8]]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in parts.iter().flat_map(|p| p.iter()) {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn thumbnail_key(path: &Path, bytes: u64, modified: u64) -> String {
    let key = fnv1a(&[path.as_os_str().as_encoded_bytes(), &[0], &bytes.to_le_bytes(), &modified.to_le_bytes()]);
    format!("{key:016x}")
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
pub(crate) fn all_clips(app: &AppHandle) -> Vec<ClipFile> {
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
    hotkeys::retry(&app);
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
    state::send(&app, &state::for_engine(&settings));
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
    tray::refresh(app);
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

/// Only videos in the clips folder or a game folder directly under it. The path comes from the
/// webview, so every part after the folder has to be a plain name: no "..", no root, no drive.
/// A ':' would name an alternate stream of some other file, so it's out too.
pub(crate) fn within(folder: &Path, path: &Path) -> bool {
    let Ok(rest) = path.strip_prefix(folder) else { return false };
    let parts: Vec<Component> = rest.components().collect();
    let plain = |c: &Component| matches!(c, Component::Normal(name) if !name.to_string_lossy().contains(':'));
    (1..=2).contains(&parts.len()) && parts.iter().all(plain) && is_mp4(path)
}

/// A video file in the clips folder (see `within`), as a path to use.
pub(crate) fn clip_file(app: &AppHandle, path: &str) -> Option<PathBuf> {
    let path = PathBuf::from(path);
    (within(&app.state::<Shared>().folder(), &path) && path.is_file()).then_some(path)
}

#[tauri::command]
pub fn open_file(app: AppHandle, path: String) {
    if let Some(path) = clip_file(&app, &path) {
        open_path(&path);
    }
}

#[tauri::command]
pub fn reveal_file(app: AppHandle, path: String) {
    let Some(file) = clip_file(&app, &path) else { return };
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // raw_arg: explorer wants /select,"path" as one argument with the quotes inside it. Windows
        // names can't hold a quote, so one here could only be closing ours early.
        if path.contains('"') {
            return;
        }
        let _ = std::process::Command::new("explorer").raw_arg(format!("/select,\"{}\"", file.display())).spawn();
    }
    #[cfg(not(windows))]
    open_path(file.parent().unwrap_or(Path::new(".")));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn within_takes_videos_in_the_folder_and_one_level_down() {
        let folder = std::env::temp_dir().join("Tape");
        assert!(within(&folder, &folder.join("a.mp4")));
        assert!(within(&folder, &folder.join("Fortnite").join("a.DVR.MP4")));
        assert!(!within(&folder, &folder.join("a.exe")));
        assert!(!within(&folder, &folder.join("one").join("two").join("a.mp4")));
        assert!(!within(&folder, &folder));
        assert!(!within(&folder, &folder.with_file_name("Other").join("a.mp4")));
    }

    #[test]
    fn keys_are_fnv1a() {
        // The published test vectors.
        assert_eq!(fnv1a(&[]), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(&[b"a"]), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a(&[b"fo", b"obar"]), 0x8594_4171_f739_67e8);
        let key = thumbnail_key(Path::new("clip.mp4"), 1, 2);
        assert_eq!(key.len(), 16);
        assert_eq!(key, thumbnail_key(Path::new("clip.mp4"), 1, 2));
        assert_ne!(key, thumbnail_key(Path::new("clip.mp4"), 1, 3));
        assert_ne!(key, thumbnail_key(Path::new("clip2.mp4"), 1, 2));
    }

    #[test]
    fn within_refuses_climbing_out() {
        let folder = std::env::temp_dir().join("Tape");
        // The grandparent of this is the folder, which the old parent check let through.
        assert!(!within(&folder, &folder.join("..").join("x.mp4")));
        assert!(!within(&folder, &folder.join("game").join("..").join("..").join("x.mp4")));
        assert!(!within(&folder, &folder.join("..").join("Tape").join("x.mp4")));
        assert!(!within(&folder, Path::new("x.mp4")));
        assert!(!within(&folder, &folder.join("clip.mp4:hidden.mp4")));
    }
}
