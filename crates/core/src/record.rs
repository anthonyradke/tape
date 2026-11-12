//! Writes one recording on its own thread, so a slow disk can never stall capture.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crate::mp4::Mp4Writer;
use crate::packet::{Packet, Tracks};

/// About 30 s of packets at 60 fps with two audio tracks.
const QUEUE: usize = 4096;
const FILE_BUFFER: usize = 1 << 20;

/// Counts the times the writer falls behind: once each time the packets waiting pass `high`, and
/// not again until they're back down to `low`, so one slow stretch counts once.
struct Backlog {
    waiting: AtomicUsize,
    behind: AtomicBool,
    high: usize,
    low: usize,
    events: Arc<AtomicU64>,
}

impl Backlog {
    fn new(high: usize, low: usize, events: Arc<AtomicU64>) -> Self {
        Backlog { waiting: AtomicUsize::new(0), behind: AtomicBool::new(false), high, low, events }
    }

    fn queued(&self) {
        let waiting = self.waiting.fetch_add(1, Ordering::Relaxed) + 1;
        if waiting >= self.high && !self.behind.swap(true, Ordering::Relaxed) {
            self.events.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// One packet less waiting: written, or never queued after all.
    fn done(&self) {
        let waiting = self.waiting.fetch_sub(1, Ordering::Relaxed).saturating_sub(1);
        if waiting <= self.low {
            self.behind.store(false, Ordering::Relaxed);
        }
    }

    /// The queue overflowed, which always counts.
    fn overflowed(&self) {
        self.behind.store(true, Ordering::Relaxed);
        self.events.fetch_add(1, Ordering::Relaxed);
    }
}

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
    backlog: Arc<Backlog>,
    /// Engine times of markers, handed to the writer when it finishes.
    markers: Arc<Mutex<Vec<i64>>>,
}

impl Recorder {
    pub fn start(path: &Path, tracks: &Tracks) -> Result<Recorder, String> {
        Recorder::start_counting(path, tracks, Arc::default())
    }

    /// `start`, adding to `slow_disk` each time the disk falls well behind.
    pub fn start_counting(path: &Path, tracks: &Tracks, slow_disk: Arc<AtomicU64>) -> Result<Recorder, String> {
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
        let backlog = Arc::new(Backlog::new(QUEUE / 2, QUEUE / 4, slow_disk));
        let markers = Arc::new(Mutex::new(Vec::new()));
        let (b, f, p, waiting, m) = (bytes.clone(), failed.clone(), path.to_path_buf(), backlog.clone(), markers.clone());
        let thread = std::thread::Builder::new()
            .name("recording".into())
            .spawn(move || {
                let mut error = None;
                while let Ok(packet) = rx.recv() {
                    waiting.done();
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
                // After a write failure the file is left as it is: the fragments already on disk
                // play as a fragmented MP4, and finishing would hide them behind an index that
                // points at the wrong bytes.
                if error.is_some() {
                    return Finished { path: p, bytes: writer.bytes(), error };
                }
                // Read after finishing: the last fragment is only written then, and a recording
                // shorter than a fragment has nothing on disk before it.
                let written = writer.bytes();
                writer.set_markers(m.lock().unwrap().clone());
                match writer.finish_with(|out| {
                    out.flush()?;
                    out.get_ref().sync_data()
                }) {
                    Ok((_, size)) => Finished { path: p, bytes: size, error },
                    Err(e) => {
                        let error = Some(format!("couldn't finish {}: {e}", p.display()));
                        Finished { path: p, bytes: written, error }
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Recorder {
            tx: Some(tx),
            thread: Some(thread),
            path: path.to_path_buf(),
            bytes,
            failed,
            overflowed: AtomicBool::new(false),
            backlog,
            markers,
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
            // Counted before sending, so the writer can never take it off first.
            self.backlog.queued();
            let sent = tx.try_send(packet);
            if sent.is_err() {
                self.backlog.done();
            }
            if let Err(TrySendError::Full(_)) = sent {
                if !self.overflowed.swap(true, Ordering::Relaxed) {
                    self.backlog.overflowed();
                    *self.failed.lock().unwrap() = Some("recording stopped, the disk is too slow to keep up".into());
                }
            }
        }
    }

    /// A marker at this engine time in microseconds, which becomes a chapter.
    pub fn mark(&self, at_us: i64) {
        self.markers.lock().unwrap().push(at_us);
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
    fn a_slow_stretch_counts_once_until_the_queue_drains() {
        let events = Arc::new(AtomicU64::new(0));
        let backlog = Backlog::new(4, 1, events.clone());
        let count = || events.load(Ordering::Relaxed);
        for _ in 0..3 {
            backlog.queued();
        }
        assert_eq!(count(), 0);
        backlog.queued();
        backlog.queued();
        assert_eq!(count(), 1);
        // Drains to 2: still behind, so passing the mark again isn't a new event.
        for _ in 0..3 {
            backlog.done();
        }
        backlog.queued();
        backlog.queued();
        assert_eq!(count(), 1);
        for _ in 0..4 {
            backlog.done();
        }
        for _ in 0..4 {
            backlog.queued();
        }
        assert_eq!(count(), 2);
        backlog.overflowed();
        assert_eq!(count(), 3);
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
