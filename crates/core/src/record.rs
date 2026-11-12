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
                // Read after finishing: the last fragment is only written then, and a recording
                // shorter than a fragment has nothing on disk before it.
                let mut total = writer.bytes();
                // After a write failure the index may not fit on the disk either, but the
                // fragments already on disk stay playable, so nothing is lost by trying.
                match writer.finish() {
                    Ok((_, size)) => total = size,
                    Err(e) => {
                        error.get_or_insert(format!("couldn't finish {}: {e}", p.display()));
                    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::{AudioFormat, Codec, VideoFormat};

    fn tracks() -> Tracks {
        Tracks {
            video: VideoFormat { codec: Codec::H264, width: 16, height: 16, fps: 60, config: vec![1, 0x64, 0, 0x1f, 0xff, 0xe0, 0] },
            audio: vec![AudioFormat { name: "a".into(), asc: vec![0x11, 0x90] }],
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tape-record-{}", std::process::id()));
        dir.join(name)
    }

    #[test]
    fn a_recording_shorter_than_a_fragment_reports_its_size() {
        let path = scratch("short.mp4");
        let t = tracks();
        let recorder = Recorder::start(&path, &t).unwrap();
        // 1.5 s: less than one fragment, so nothing but the header is on disk until finish.
        for frame in 0..90 {
            recorder.push(Packet { track: 0, pts: frame, key: frame == 0, data: Arc::new(vec![7; 1000]) });
            if frame % 2 == 0 {
                let pts = frame * 800;
                recorder.push(Packet { track: 1, pts, key: true, data: Arc::new(vec![9; 100]) });
            }
        }
        let finished = recorder.finish();
        assert!(finished.error.is_none(), "{:?}", finished.error);
        let on_disk = std::fs::metadata(&path).unwrap().len();
        assert_eq!(finished.bytes, on_disk);
        assert!(on_disk > 90 * 1000);
        let _ = std::fs::remove_file(path);
    }
}
