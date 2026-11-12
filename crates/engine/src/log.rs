//! A small file log. The engine has no console, and when something goes wrong on a machine
//! nobody is watching, this file is the record of what happened.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const MAX_BYTES: u64 = 2 << 20;
/// A message that keeps repeating (capture failing to reopen every 500 ms on a locked PC) is
/// counted rather than written, with the count written at least this often.
const REPEAT_REPORT: Duration = Duration::from_secs(600);

static LOG: Mutex<Option<Log>> = Mutex::new(None);

struct Log {
    file: File,
    path: PathBuf,
    max_bytes: u64,
    size: u64,
    last: String,
    repeats: u64,
    repeats_since: Instant,
}

impl Log {
    fn open(path: &Path, max_bytes: u64) -> Option<Log> {
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let mut log = Log {
            file: Self::append(path)?,
            path: path.to_path_buf(),
            max_bytes,
            size: 0,
            last: String::new(),
            repeats: 0,
            repeats_since: Instant::now(),
        };
        log.size = log.file.metadata().map(|m| m.len()).unwrap_or(0);
        log.rotate_if_full();
        Some(log)
    }

    fn append(path: &Path) -> Option<File> {
        OpenOptions::new().create(true).append(true).open(path).ok()
    }

    /// One generation of history: enough to see what led up to a crash.
    fn rotate_if_full(&mut self) {
        if self.size <= self.max_bytes {
            return;
        }
        let _ = fs::rename(&self.path, self.path.with_extension("old.log"));
        if let Some(file) = Self::append(&self.path) {
            self.file = file;
            self.size = 0;
        }
    }

    fn write(&mut self, stamp: &str, text: &str) {
        let line = format!("{stamp} {text}\n");
        if self.file.write_all(line.as_bytes()).is_ok() {
            self.size += line.len() as u64;
        }
        self.rotate_if_full();
    }

    fn flush_repeats(&mut self, stamp: &str) {
        if self.repeats > 0 {
            let text = format!("{} (repeated {} times)", self.last, self.repeats);
            self.repeats = 0;
            self.write(stamp, &text);
        }
    }

    fn line(&mut self, stamp: &str, message: &str, now: Instant) {
        if message == self.last {
            if self.repeats == 0 {
                self.repeats_since = now;
            }
            self.repeats += 1;
            if now.duration_since(self.repeats_since) >= REPEAT_REPORT {
                self.flush_repeats(stamp);
            }
            return;
        }
        self.flush_repeats(stamp);
        self.last = message.to_string();
        self.write(stamp, message);
    }
}

pub fn init(path: &Path) {
    *LOG.lock().unwrap() = Log::open(path, MAX_BYTES);
}

pub fn line(message: &str) {
    let stamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f").to_string();
    if let Some(log) = LOG.lock().unwrap().as_mut() {
        log.line(&stamp, message, Instant::now());
    }
}

#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => { $crate::log::line(&format!($($arg)*)) };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tape-log-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir.join("engine.log")
    }

    #[test]
    fn repeated_messages_are_counted() {
        let path = temp("repeat");
        let mut log = Log::open(&path, MAX_BYTES).unwrap();
        let t = Instant::now();
        log.line("1", "capture: paused", t);
        for _ in 0..5 {
            log.line("2", "capture: paused", t);
        }
        log.line("3", "capture: Desktop Duplication", t);
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text, "1 capture: paused\n3 capture: paused (repeated 5 times)\n3 capture: Desktop Duplication\n");
    }

    #[test]
    fn a_long_run_of_repeats_is_still_reported() {
        let path = temp("long-repeat");
        let mut log = Log::open(&path, MAX_BYTES).unwrap();
        let t = Instant::now();
        log.line("1", "x", t);
        log.line("2", "x", t);
        log.line("3", "x", t + REPEAT_REPORT);
        assert_eq!(fs::read_to_string(&path).unwrap(), "1 x\n3 x (repeated 2 times)\n");
    }

    #[test]
    fn the_file_rotates_while_running() {
        let path = temp("rotate");
        let mut log = Log::open(&path, 100).unwrap();
        let t = Instant::now();
        for i in 0..20 {
            log.line("t", &format!("message number {i}"), t);
        }
        let current = fs::metadata(&path).unwrap().len();
        let old = fs::metadata(path.with_extension("old.log")).unwrap().len();
        assert!(current <= 100 + 30, "{current}");
        assert!(old > 100, "{old}");
        assert!(fs::read_to_string(&path).unwrap().ends_with("message number 19\n"));
    }
}
