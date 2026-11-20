use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::ChildStdin;
use std::sync::atomic::AtomicBool;
use std::sync::Mutex;

use serde::Serialize;
use tape_core::ipc::{Command, Probe, Status};
use tape_core::settings::Settings;
use tauri::{AppHandle, Manager};

/// What the tray and the UI show at a glance.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Light {
    /// Starting up, or everything switched off by choice.
    #[default]
    Idle,
    Replay,
    Recording,
    Trouble,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Health {
    pub light: Light,
    /// Why it's in trouble, in plain words.
    pub problem: Option<String>,
    pub engine_restarts: u32,
    pub last_restart: Option<String>,
}

pub struct Shared {
    pub settings: Mutex<Settings>,
    pub status: Mutex<Status>,
    pub probe: Mutex<Probe>,
    pub health: Mutex<Health>,
    pub hotkey_errors: Mutex<Vec<String>>,
    pub engine_stdin: Mutex<Option<ChildStdin>>,
    /// Recording the user asked for, carried across an engine restart.
    pub recording_wanted: AtomicBool,
    pub quitting: AtomicBool,
    /// The supervisor has finished and no engine is running.
    pub engine_stopped: AtomicBool,
    pub self_test: Mutex<Option<String>>,
    pub log_path: PathBuf,
    pub settings_path: PathBuf,
    /// Gallery thumbnails, a cache that's safe to delete.
    pub thumbnails: PathBuf,
}

impl Shared {
    /// Reads the settings file; a missing one means defaults, a damaged one keeps what still
    /// reads and is copied to settings.json.bad before being written over. True if this is the
    /// first run.
    pub fn load(app: &AppHandle) -> (Shared, bool) {
        let dir = app.path().app_config_dir().unwrap_or_else(|_| PathBuf::from("."));
        let _ = fs::create_dir_all(&dir);
        let settings_path = dir.join("settings.json");
        // Antivirus or a sync tool can hold the file for a moment at sign-in.
        let read = read_retrying(|| fs::read(&settings_path), 5, std::time::Duration::from_millis(200));
        let first_run = matches!(&read, Err(e) if e.kind() == std::io::ErrorKind::NotFound);
        let (settings, damaged) = match read.map(String::from_utf8) {
            Ok(Ok(text)) => Settings::from_json_lenient(&text),
            Ok(Err(_)) => (Settings::default(), true),
            Err(_) => (Settings::default(), !first_run),
        };
        // Only write over a file that couldn't be read once a copy of it is safe. Otherwise it's
        // left alone until the user changes a setting.
        let keep_file = damaged && fs::copy(&settings_path, settings_path.with_extension("json.bad")).is_err();
        #[cfg(windows)]
        if let Some(videos) = known_videos_folder() {
            tape_core::naming::set_videos_folder(videos);
        }
        let mut settings = settings;
        if settings.folder.is_empty() {
            match tape_core::naming::earlier_folder() {
                Some(earlier) => settings.folder = earlier.display().to_string(),
                None => tape_core::naming::rename_old_default_folder(),
            }
        }
        let thumbnails = app.path().app_cache_dir().unwrap_or_else(|_| dir.clone()).join("thumbnails");
        let log_path = app.path().app_log_dir().unwrap_or(dir).join("engine.log");
        let shared = Shared {
            settings: Mutex::new(settings.normalized()),
            status: Mutex::default(),
            probe: Mutex::default(),
            health: Mutex::default(),
            hotkey_errors: Mutex::default(),
            engine_stdin: Mutex::new(None),
            recording_wanted: AtomicBool::new(false),
            quitting: AtomicBool::new(false),
            engine_stopped: AtomicBool::new(false),
            self_test: Mutex::new(None),
            log_path,
            settings_path,
            thumbnails,
        };
        if !keep_file {
            shared.persist();
        }
        (shared, first_run)
    }

    /// Written to a temporary file first, so a crash mid-write can't lose the settings.
    pub fn persist(&self) {
        let Ok(text) = serde_json::to_string_pretty(&*self.settings.lock().unwrap()) else { return };
        let temporary = self.settings_path.with_extension("tmp");
        if fs::write(&temporary, text).is_ok() {
            let _ = fs::rename(&temporary, &self.settings_path);
        }
    }

    pub fn folder(&self) -> PathBuf {
        let folder = self.settings.lock().unwrap().folder.clone();
        if folder.is_empty() {
            tape_core::naming::default_folder()
        } else {
            PathBuf::from(folder)
        }
    }
}

/// The Videos folder from Windows, wherever the user has moved it.
#[cfg(windows)]
fn known_videos_folder() -> Option<PathBuf> {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{SHGetKnownFolderPath, FOLDERID_Videos, KF_FLAG_DEFAULT};
    unsafe {
        let raw = SHGetKnownFolderPath(&FOLDERID_Videos, KF_FLAG_DEFAULT, None).ok()?;
        let path = raw.to_string().ok();
        CoTaskMemFree(Some(raw.0 as *const _));
        path.filter(|p| !p.is_empty()).map(PathBuf::from)
    }
}

/// The settings as the engine gets them: without the share server and its key, which it has no
/// use for and which then can't end up anywhere it writes. An empty folder is filled in, since
/// only this process knows where Windows keeps Videos.
pub fn for_engine(settings: &Settings) -> Command {
    let mut settings = settings.clone();
    if settings.folder.is_empty() {
        settings.folder = tape_core::naming::default_folder().display().to_string();
    }
    settings.share_server.clear();
    settings.share_key.clear();
    Command::Configure { settings: Box::new(settings) }
}

/// Sends a command to the engine, and says whether it got there. While it's restarting there is
/// nobody to tell, and the supervisor brings a new engine up to date with the current settings
/// anyway, but a save has to say it didn't happen (`supervisor::save_clip`).
pub fn send(app: &AppHandle, command: &Command) -> bool {
    let Some(shared) = app.try_state::<Shared>() else { return false };
    let mut stdin = shared.engine_stdin.lock().unwrap();
    let Some(pipe) = stdin.as_mut() else { return false };
    let Ok(mut line) = serde_json::to_vec(command) else { return false };
    line.push(b'\n');
    if pipe.write_all(&line).and_then(|_| pipe.flush()).is_err() {
        *stdin = None;
        return false;
    }
    true
}

/// `read`, tried again after `wait` while it fails with anything but "not found".
fn read_retrying(mut read: impl FnMut() -> std::io::Result<Vec<u8>>, tries: u32, wait: std::time::Duration) -> std::io::Result<Vec<u8>> {
    let mut result = read();
    for _ in 1..tries {
        match &result {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => std::thread::sleep(wait),
            _ => break,
        }
        result = read();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Error, ErrorKind};
    use std::time::Duration;

    #[test]
    fn a_locked_file_is_read_again() {
        let mut calls = 0;
        let read = read_retrying(
            || {
                calls += 1;
                if calls < 3 { Err(Error::from(ErrorKind::PermissionDenied)) } else { Ok(b"{}".to_vec()) }
            },
            5,
            Duration::ZERO,
        );
        assert_eq!(read.unwrap(), b"{}");
        assert_eq!(calls, 3);
    }

    #[test]
    fn a_missing_file_is_not_waited_for() {
        let mut calls = 0;
        let read = read_retrying(
            || {
                calls += 1;
                Err(Error::from(ErrorKind::NotFound))
            },
            5,
            Duration::ZERO,
        );
        assert_eq!(read.unwrap_err().kind(), ErrorKind::NotFound);
        assert_eq!(calls, 1);
    }

    #[test]
    fn gives_up_after_the_last_try() {
        let mut calls = 0;
        let read = read_retrying(
            || {
                calls += 1;
                Err(Error::from(ErrorKind::PermissionDenied))
            },
            5,
            Duration::ZERO,
        );
        assert!(read.is_err());
        assert_eq!(calls, 5);
    }
}
