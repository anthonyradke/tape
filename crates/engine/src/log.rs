//! A small file log. The engine has no console, and when something goes wrong on a machine
//! nobody is watching, this file is the record of what happened.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const MAX_BYTES: u64 = 2 << 20;

static LOG: Mutex<Option<(File, PathBuf)>> = Mutex::new(None);

pub fn init(path: &Path) {
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    // One generation of history: enough to see what led up to a crash.
    if fs::metadata(path).is_ok_and(|m| m.len() > MAX_BYTES) {
        let _ = fs::rename(path, path.with_extension("old.log"));
    }
    if let Ok(file) = OpenOptions::new().create(true).append(true).open(path) {
        *LOG.lock().unwrap() = Some((file, path.to_path_buf()));
    }
}

pub fn line(message: &str) {
    let stamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
    if let Some((file, _)) = LOG.lock().unwrap().as_mut() {
        let _ = writeln!(file, "{stamp} {message}");
    }
}

#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => { $crate::log::line(&format!($($arg)*)) };
}
