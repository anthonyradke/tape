//! Self-update from GitHub Releases: a signed installer and a latest.json, published by the release
//! workflow. tape checks quietly and only installs when asked.

use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_updater::{Update, UpdaterExt};

use crate::state;
use crate::{supervisor, tray};

/// Let startup, and sign-in when launched with Windows, settle before the first check.
const FIRST_CHECK: Duration = Duration::from_secs(30);
const CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);

#[derive(Default)]
pub struct Shared {
    status: Mutex<Status>,
    pending: Mutex<Option<Update>>,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub current: String,
    /// Newer version on offer, if any.
    pub available: Option<String>,
    pub notes: Option<String>,
    pub checking: bool,
    /// A check has finished at least once.
    pub checked: bool,
    /// Download progress in percent while installing.
    pub progress: Option<u8>,
    pub error: Option<String>,
}

pub fn init(app: &AppHandle) {
    let status = Status { current: app.package_info().version.to_string(), ..Status::default() };
    app.manage(Shared { status: Mutex::new(status), ..Shared::default() });
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(FIRST_CHECK);
        loop {
            tauri::async_runtime::block_on(check(&app));
            std::thread::sleep(CHECK_EVERY);
        }
    });
}

pub fn status(app: &AppHandle) -> Status {
    app.state::<Shared>().status.lock().unwrap().clone()
}

fn set(app: &AppHandle, f: impl FnOnce(&mut Status)) {
    let status = {
        let shared = app.state::<Shared>();
        let mut s = shared.status.lock().unwrap();
        f(&mut s);
        s.clone()
    };
    let _ = app.emit("update", status);
}

/// The installer has to replace an exe the engine is running from, and the updater ends this
/// process with `exit(0)`, which skips everything a normal quit does.
fn updater(app: &AppHandle) -> tauri_plugin_updater::Result<tauri_plugin_updater::Updater> {
    let handle = app.clone();
    app.updater_builder()
        .on_before_exit(move || {
            supervisor::stop_engine(&handle);
            handle.cleanup_before_exit();
        })
        .build()
}

pub async fn check(app: &AppHandle) {
    let busy = {
        let s = app.state::<Shared>();
        let s = s.status.lock().unwrap();
        s.checking || s.progress.is_some()
    };
    if busy {
        return;
    }
    set(app, |s| s.checking = true);
    let result = match updater(app) {
        Ok(updater) => updater.check().await,
        Err(e) => Err(e),
    };
    match result {
        Ok(update) => {
            let (available, notes) = match &update {
                Some(u) => (Some(u.version.clone()), u.body.clone().filter(|b| !b.trim().is_empty())),
                None => (None, None),
            };
            *app.state::<Shared>().pending.lock().unwrap() = update;
            set(app, |s| {
                s.available = available;
                s.notes = notes;
                s.error = None;
            });
        }
        // Being offline is normal. Keep what's known, and only say why when asked.
        Err(e) => set(app, |s| s.error = Some(e.to_string())),
    }
    set(app, |s| {
        s.checking = false;
        s.checked = true;
    });
    tray::refresh(app);
}

/// Downloads, verifies and runs the installer, which restarts tape. Only returns on failure.
pub async fn install(app: &AppHandle) -> Result<(), String> {
    if app.state::<state::Shared>().status.lock().unwrap().recording {
        return Err("Stop the recording first.".into());
    }
    let update = app.state::<Shared>().pending.lock().unwrap().clone().ok_or("No update to install.")?;
    if status(app).progress.is_some() {
        return Ok(());
    }
    set(app, |s| {
        s.progress = Some(0);
        s.error = None;
    });
    let mut done = 0u64;
    let result = update
        .download_and_install(
            |chunk, total| {
                done += chunk as u64;
                if let Some(total) = total.filter(|&t| t > 0) {
                    let pct = (done * 100 / total).min(100) as u8;
                    if status(app).progress != Some(pct) {
                        set(app, |s| s.progress = Some(pct));
                    }
                }
            },
            || {},
        )
        .await;
    let err = match result {
        Ok(()) => "The installer didn't start.".to_string(),
        Err(e) => e.to_string(),
    };
    set(app, |s| {
        s.progress = None;
        s.error = Some(err.clone());
    });
    Err(err)
}

#[tauri::command]
pub fn update_state(app: AppHandle) -> Status {
    status(&app)
}

#[tauri::command]
pub async fn check_update(app: AppHandle) -> Status {
    check(&app).await;
    status(&app)
}

#[tauri::command]
pub async fn install_update(app: AppHandle) -> Result<(), String> {
    install(&app).await
}
