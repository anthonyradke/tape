//! Writes one recording on its own thread, so a slow disk can never stall capture.

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crate::mp4::Mp4Writer;
use crate::packet::{Packet, Tracks};

/// About 30 s of packets at 60 fps with two audio tracks.
const QUEUE: usize = 4096;
const FILE_BUFFER: usize = 1 << 20;

#[derive(Debug)]
pub struct Finished {
    pub path: PathBuf,
    pub bytes: u64,
    pub error: Option<String>,
}

pub struct Recorder {
    tx: Option<SyncSender<Packet>>,
    thread: Option<JoinHandle<Finished>>,
    path: PathBuf,
    bytes: Arc<AtomicU64>,
    failed: Arc<Mutex<Option<String>>>,
    overflowed: AtomicBool,
}

impl Recorder {
    pub fn start(path: &Path, tracks: &Tracks) -> Result<Recorder, String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
        }
        let file = File::create(path).map_err(|e| format!("can't create {}: {e}", path.display()))?;
        let mut writer = Mp4Writer::new(BufWriter::with_capacity(FILE_BUFFER, file), tracks)
            .map_err(|e| format!("can't write {}: {e}", path.display()))?;

        let (tx, rx) = sync_channel::<Packet>(QUEUE);
        let bytes = Arc::new(AtomicU64::new(0));
        let failed = Arc::new(Mutex::new(None));
        let tracks = tracks.clone();
        let (b, f, p) = (bytes.clone(), failed.clone(), path.to_path_buf());
        let thread = std::thread::Builder::new()
            .name("recording".into())
            .spawn(move || {
                let mut error = None;
                while let Ok(packet) = rx.recv() {
                    if error.is_some() {
                        continue; // keep draining so the sender never blocks
                    }
                    let timescale = tracks.timescale(packet.track);
                    if let Err(e) = writer.push(&packet, timescale) {
                        let message = format!("recording stopped, write failed: {e}");
                        *f.lock().unwrap() = Some(message.clone());
                        error = Some(message);
                    }
                    b.store(writer.bytes(), Ordering::Relaxed);
                }
                let total = writer.bytes();
                // After a write failure the index may not fit on the disk either, but the
                // fragments already on disk stay playable, so nothing is lost by trying.
                if let Err(e) = writer.finish() {
                    error.get_or_insert(format!("couldn't finish {}: {e}", p.display()));
                }
                Finished { path: p, bytes: total, error }
            })
            .map_err(|e| e.to_string())?;
        Ok(Recorder {
            tx: Some(tx),
            thread: Some(thread),
            path: path.to_path_buf(),
            bytes,
            failed,
            overflowed: AtomicBool::new(false),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    /// Never blocks. If the disk has fallen 30 s behind, the recording is reported as failed
    /// rather than holding up the replay buffer.
    pub fn push(&self, packet: Packet) {
        if let Some(tx) = &self.tx {
            if let Err(TrySendError::Full(_)) = tx.try_send(packet) {
                if !self.overflowed.swap(true, Ordering::Relaxed) {
                    *self.failed.lock().unwrap() = Some("recording stopped, the disk is too slow to keep up".into());
                }
            }
        }
    }

    pub fn failure(&self) -> Option<String> {
        self.failed.lock().unwrap().clone()
    }

    pub fn finish(mut self) -> Finished {
        self.tx = None;
        self.thread.take().unwrap().join().unwrap_or_else(|_| Finished {
            path: self.path.clone(),
            bytes: self.bytes(),
            error: Some("recording thread panicked".into()),
        })
    }
}
