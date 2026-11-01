use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::ChildStdin;
use std::sync::atomic::{AtomicBool, AtomicU32};
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
    pub engine_generation: AtomicU32,
    pub self_test: Mutex<Option<String>>,
    pub log_path: PathBuf,
    settings_path: PathBuf,
}

impl Shared {
    /// Reads the settings file; a missing or damaged one means defaults. True if this is the
    /// first run.
    pub fn load(app: &AppHandle) -> (Shared, bool) {
        let dir = app.path().app_config_dir().unwrap_or_else(|_| PathBuf::from("."));
        let _ = fs::create_dir_all(&dir);
        let settings_path = dir.join("settings.json");
        let text = fs::read_to_string(&settings_path).ok();
        let first_run = text.is_none();
        let settings: Settings = text.and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
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
            engine_generation: AtomicU32::new(0),
            self_test: Mutex::new(None),
            log_path,
            settings_path,
        };
        shared.persist();
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

/// Sends a command to the engine. While it's restarting there is nobody to tell, and the
/// supervisor brings a new engine up to date with the current settings anyway.
pub fn send(app: &AppHandle, command: &Command) {
    let Some(shared) = app.try_state::<Shared>() else { return };
    let mut stdin = shared.engine_stdin.lock().unwrap();
    if let Some(pipe) = stdin.as_mut() {
        let Ok(mut line) = serde_json::to_vec(command) else { return };
        line.push(b'\n');
        if pipe.write_all(&line).and_then(|_| pipe.flush()).is_err() {
            *stdin = None;
        }
    }
}
