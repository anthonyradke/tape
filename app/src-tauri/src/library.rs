//! Changing what's in the clips folder from the gallery: trimming, deleting, renaming and starring.
//! Every path comes from the webview, so each is checked to be a video in the clips folder first.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tape_core::mp4::trim::{self, Info};
use tape_core::naming;
use tauri::{AppHandle, Emitter, Manager};

use crate::commands::{all_clips, clip_file, clip_key, thumbnails};
use crate::state::Shared;

const THUMBNAIL_GRACE: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// Starred clips, by path. Kept in a file of their own beside the settings.
pub struct Stars {
    path: PathBuf,
    set: Mutex<BTreeSet<String>>,
}

impl Stars {
    pub fn load(app: &AppHandle) -> Stars {
        let path = app.state::<Shared>().settings_path.with_file_name("stars.json");
        let set = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        Stars { path, set: Mutex::new(set) }
    }

    pub fn has(&self, path: &str) -> bool {
        self.set.lock().unwrap().contains(path)
    }

    fn change(&self, f: impl FnOnce(&mut BTreeSet<String>)) {
        let mut set = self.set.lock().unwrap();
        f(&mut set);
        let Ok(text) = serde_json::to_string_pretty(&*set) else { return };
        let temporary = self.path.with_extension("tmp");
        if std::fs::write(&temporary, text).is_ok() {
            let _ = std::fs::rename(&temporary, &self.path);
        }
    }
}

/// A finished video in the clips folder, not the recording still being written.
fn check(app: &AppHandle, path: &str) -> Result<PathBuf, String> {
    let writing = app.state::<Shared>().status.lock().unwrap().recording_path.clone();
    let file = clip_file(app, path).ok_or("That file isn't in the clips folder any more.")?;
    if writing.as_deref() == Some(path) {
        return Err("That recording is still going.".into());
    }
    Ok(file)
}

fn thumbnail_of(app: &AppHandle, path: &Path) -> Option<(PathBuf, f64)> {
    let key = clip_key(path)?;
    thumbnails(&app.state::<Shared>().thumbnails).remove(&key)
}

#[tauri::command]
pub fn clip_info(app: AppHandle, path: String) -> Result<Info, String> {
    let path = check(&app, &path)?;
    trim::info(&path).map_err(|e| format!("Can't trim this file: {e}."))
}

/// Saves the part between `start` and `end` seconds as a new file beside the original, which is
/// left as it was. Returns the new file's path.
#[tauri::command]
pub async fn trim_clip(app: AppHandle, path: String, start: f64, end: f64) -> Result<String, String> {
    let path = check(&app, &path)?;
    let to = naming::trimmed_path(&path);
    let made = to.clone();
    let result = tauri::async_runtime::spawn_blocking(move || trim::trim(&path, &made, start, end))
        .await
        .map_err(|e| e.to_string())?;
    match result {
        Ok(_) => {
            let _ = app.emit("changed", ());
            Ok(to.display().to_string())
        }
        Err(e) => Err(format!("Couldn't trim: {e}.")),
    }
}

/// To the Recycle Bin, so a slip can be undone from there.
#[tauri::command]
pub fn delete_clip(app: AppHandle, path: String) -> Result<(), String> {
    let file = check(&app, &path)?;
    let thumb = thumbnail_of(&app, &file);
    recycle(&file).map_err(|e| format!("Couldn't delete it: {e}"))?;
    if let Some((thumb, _)) = thumb {
        let _ = std::fs::remove_file(thumb);
    }
    app.state::<Stars>().change(|s| {
        s.remove(&path);
    });
    let _ = app.emit("changed", ());
    Ok(())
}

/// Renames in place, keeping the ending that says whether it's a clip or a recording. Returns the
/// new path.
#[tauri::command]
pub fn rename_clip(app: AppHandle, path: String, name: String) -> Result<String, String> {
    let file = check(&app, &path)?;
    let to = naming::renamed_path(&file, &name).ok_or("That name has nothing a file name can use.")?;
    if to == file {
        return Ok(path);
    }
    // Windows names ignore case: "clip" to "Clip" is the same file, not a clash.
    let same_file = to.to_string_lossy().eq_ignore_ascii_case(&file.to_string_lossy());
    if to.exists() && !same_file {
        return Err("There's already a clip with that name.".into());
    }
    let old_key = clip_key(&file);
    let thumb = thumbnail_of(&app, &file);
    std::fs::rename(&file, &to).map_err(|e| format!("Couldn't rename it: {e}"))?;
    // The key comes from the path, so the thumbnail moves with the file instead of being made
    // again, and a shared link is told where its clip went.
    let new_key = clip_key(&to);
    if let (Some((old, seconds)), Some(key)) = (thumb, &new_key) {
        let shared = app.state::<Shared>();
        let _ = std::fs::rename(old, shared.thumbnails.join(format!("{key}_{}.jpg", (seconds * 1000.0) as u64)));
    }
    if let (Some(old), Some(new)) = (old_key, new_key) {
        crate::share::moved(&app, &old, &new);
    }
    let new = to.display().to_string();
    let stars = app.state::<Stars>();
    if stars.has(&path) {
        stars.change(|s| {
            s.remove(&path);
            s.insert(new.clone());
        });
    }
    let _ = app.emit("changed", ());
    Ok(new)
}

/// Thumbnails nobody's file has any more (deleted outside tape, or keyed the way older versions
/// did) and stars on files that are gone. Cheap, so it runs at every start. Skipped while the clips
/// folder can't be reached (a drive that isn't plugged in), where everything would look gone.
pub fn prune(app: &AppHandle) {
    let shared = app.state::<Shared>();
    let folder = shared.folder();
    if !folder.is_dir() {
        return;
    }
    let keys: BTreeSet<String> = all_clips(app).into_iter().map(|c| c.key).collect();
    // Not one the gallery may be writing for a clip saved since the list above was made.
    let fresh = |path: &Path| {
        std::fs::metadata(path).and_then(|m| m.modified()).is_ok_and(|t| t.elapsed().map_or(true, |age| age < THUMBNAIL_GRACE))
    };
    for (key, (path, _)) in thumbnails(&shared.thumbnails) {
        if !keys.contains(&key) && !fresh(&path) {
            let _ = std::fs::remove_file(path);
        }
    }
    let stars = app.state::<Stars>();
    let gone = |star: &String| {
        let path = Path::new(star);
        !path.exists() && (path.starts_with(&folder) || path.parent().is_some_and(Path::is_dir))
    };
    if stars.set.lock().unwrap().iter().any(gone) {
        stars.change(|s| s.retain(|star| !gone(star)));
    }
}

#[tauri::command]
pub fn star_clip(app: AppHandle, path: String, on: bool) -> Result<(), String> {
    check(&app, &path)?;
    app.state::<Stars>().change(|s| {
        if on {
            s.insert(path);
        } else {
            s.remove(&path);
        }
    });
    Ok(())
}

#[cfg(windows)]
fn recycle(path: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::UI::Shell::{
        SHFileOperationW, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, FO_DELETE, SHFILEOPSTRUCTW,
    };
    // A list of paths, each ending in a null, with one more null after the last.
    let from: Vec<u16> = path.as_os_str().encode_wide().chain([0, 0]).collect();
    let mut op = SHFILEOPSTRUCTW {
        wFunc: FO_DELETE,
        pFrom: PCWSTR(from.as_ptr()),
        fFlags: (FOF_ALLOWUNDO.0 | FOF_NOCONFIRMATION.0 | FOF_NOERRORUI.0 | FOF_SILENT.0) as u16,
        ..Default::default()
    };
    let code = unsafe { SHFileOperationW(&mut op) };
    if code != 0 || op.fAnyOperationsAborted.as_bool() {
        return Err(format!("Windows error {code:#x}. The file may be open in another program."));
    }
    if path.exists() {
        return Err("Windows didn't remove it.".into());
    }
    Ok(())
}

/// Only the test runs on Linux get here; there's no recycle bin worth finding.
#[cfg(not(windows))]
fn recycle(path: &Path) -> Result<(), String> {
    std::fs::remove_file(path).map_err(|e| e.to_string())
}
