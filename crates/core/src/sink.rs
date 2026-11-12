//! Where encoded packets go. One encoder session feeds the replay ring and, when recording, the
//! file, so turning recording on costs nothing extra on the GPU.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crate::packet::{AudioFormat, Packet, Tracks, VideoFormat};
use crate::record::{Finished, Recorder};
use crate::ring::Ring;

pub struct Clip {
    pub tracks: Tracks,
    pub packets: Vec<Packet>,
    /// Where the clip before it ended, put back by `Sink::clip_failed` if this one isn't saved.
    pub previous_cut: Option<i64>,
    /// Where this clip ends.
    pub cut: i64,
}

#[derive(Default)]
pub struct RecordingStats {
    pub path: Option<PathBuf>,
    pub bytes: u64,
    pub micros: i64,
}

struct Active {
    recorder: Recorder,
    started_us: i64,
}

struct Inner {
    audio: Vec<AudioFormat>,
    tracks: Option<Tracks>,
    ring: Ring,
    replay: bool,
    window: u32,
    active: Option<Active>,
    /// Waiting for the next keyframe to begin writing.
    pending: Option<PathBuf>,
    newest_video_us: i64,
    /// Files ended by a parameter change, finishing on threads of their own.
    finishing: Vec<(PathBuf, JoinHandle<Finished>)>,
    finished: Vec<Finished>,
    failure: Option<String>,
}

pub struct Sink {
    inner: Mutex<Inner>,
    /// The video thread reads this and forces a keyframe, so a recording starts immediately.
    want_keyframe: AtomicBool,
    frames: AtomicU64,
    /// Times a recording's writer fell behind the disk, across every file.
    slow_disk: Arc<AtomicU64>,
}

impl Sink {
    pub fn new(replay: bool, seconds: u32) -> Arc<Sink> {
        Arc::new(Sink {
            inner: Mutex::new(Inner {
                audio: Vec::new(),
                tracks: None,
                ring: Ring::new(seconds),
                replay,
                window: seconds,
                active: None,
                pending: None,
                newest_video_us: 0,
                finishing: Vec::new(),
                finished: Vec::new(),
                failure: None,
            }),
            want_keyframe: AtomicBool::new(false),
            frames: AtomicU64::new(0),
            slow_disk: Arc::default(),
        })
    }

    pub fn take_keyframe_request(&self) -> bool {
        self.want_keyframe.swap(false, Ordering::Relaxed)
    }

    /// Times a recording's writer fell behind the disk so far.
    pub fn slow_disk_events(&self) -> u64 {
        self.slow_disk.load(Ordering::Relaxed)
    }

    /// Video frames accepted so far.
    pub fn frames(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }

    pub fn set_replay(&self, on: bool, seconds: u32) {
        let mut s = self.inner.lock().unwrap();
        s.replay = on;
        s.window = seconds;
        s.ring.set_length(seconds);
        if !on {
            s.ring.clear();
        }
    }

    /// Most RAM the replay buffer may hold, whatever its length. None for no limit.
    pub fn set_memory_limit(&self, bytes: Option<usize>) {
        self.inner.lock().unwrap().ring.set_max_bytes(bytes);
    }

    /// The audio tracks of everything from here on.
    pub fn set_audio(&self, audio: Vec<AudioFormat>) {
        let mut s = self.inner.lock().unwrap();
        if s.audio == audio {
            return;
        }
        s.audio = audio;
        // Video's next keyframe rebuilds the tracks with the new list.
        s.tracks = None;
        self.want_keyframe.store(true, Ordering::Relaxed);
    }

    pub fn video(&self, format: &VideoFormat, pts: i64, key: bool, data: Vec<u8>) {
        let mut s = self.inner.lock().unwrap();
        let s = &mut *s;
        self.frames.fetch_add(1, Ordering::Relaxed);
        let changed = s.tracks.as_ref().is_none_or(|t| t.video != *format || t.audio != s.audio);
        if changed {
            if !key {
                return; // nothing can start before a keyframe
            }
            let split = s.tracks.is_some() || s.active.is_some();
            s.tracks = Some(Tracks { video: format.clone(), audio: s.audio.clone() });
            s.ring.clear();
            if split {
                // Different stream parameters can't share a file: end it and go on in a new one.
                // Finishing drains the queue and writes the index, which takes as long as the disk
                // needs; the video thread can't wait for that, so it happens on a thread of its own.
                if let Some(active) = s.active.take() {
                    let path = active.recorder.path().to_path_buf();
                    s.pending.get_or_insert(continued(&path));
                    match finish_in_background(active.recorder) {
                        Ok(handle) => s.finishing.push(handle),
                        Err(finished) => s.finished.push(finished),
                    }
                }
            }
        }
        let tracks = s.tracks.as_ref().unwrap();
        let packet = Packet { track: 0, pts, key, data: Arc::new(data) };
        let us = packet.micros(format.fps);
        s.newest_video_us = us;

        if key {
            if let Some(path) = s.pending.take() {
                match Recorder::start_counting(&path, tracks, self.slow_disk.clone()) {
                    Ok(recorder) => s.active = Some(Active { recorder, started_us: us }),
                    Err(e) => s.failure = Some(e),
                }
            }
        }
        if let Some(active) = &s.active {
            active.recorder.push(packet.clone());
        }
        if s.replay {
            s.ring.push(packet, tracks);
        }
    }

    pub fn audio(&self, track: usize, pts: i64, data: Vec<u8>) {
        let mut s = self.inner.lock().unwrap();
        let s = &mut *s;
        let Some(tracks) = &s.tracks else { return };
        if track >= tracks.audio.len() {
            return;
        }
        let packet = Packet { track: track as u8 + 1, pts, key: true, data: Arc::new(data) };
        if let Some(active) = &s.active {
            active.recorder.push(packet.clone());
        }
        if s.replay {
            s.ring.push(packet, tracks);
        }
    }

    pub fn start_recording(&self, path: PathBuf) {
        let mut s = self.inner.lock().unwrap();
        if s.active.is_none() {
            s.pending = Some(path);
            self.want_keyframe.store(true, Ordering::Relaxed);
        }
    }

    /// Also waits for files a parameter change ended, so none is left unfinished when the engine
    /// stops; those are reported by `take_finished`.
    pub fn stop_recording(&self) -> Option<Finished> {
        let (active, finishing) = {
            let mut s = self.inner.lock().unwrap();
            s.pending = None;
            (s.active.take(), std::mem::take(&mut s.finishing))
        };
        // Outside the lock: finishing a long recording takes a moment, and capture runs throughout.
        let done: Vec<Finished> = finishing.into_iter().map(joined).collect();
        self.inner.lock().unwrap().finished.extend(done);
        active.map(|a| a.recorder.finish())
    }

    pub fn is_recording(&self) -> bool {
        let s = self.inner.lock().unwrap();
        s.active.is_some() || s.pending.is_some()
    }

    pub fn recording_stats(&self) -> RecordingStats {
        let s = self.inner.lock().unwrap();
        match &s.active {
            Some(a) => RecordingStats {
                path: Some(a.recorder.path().to_path_buf()),
                bytes: a.recorder.bytes(),
                micros: s.newest_video_us - a.started_us,
            },
            None => RecordingStats { path: s.pending.clone(), bytes: 0, micros: 0 },
        }
    }

    /// Files finished by a parameter change since the last call. One still finishing comes in a
    /// later call.
    pub fn take_finished(&self) -> Vec<Finished> {
        let (mut out, done) = {
            let mut s = self.inner.lock().unwrap();
            let (done, running) = std::mem::take(&mut s.finishing).into_iter().partition(|(_, h)| h.is_finished());
            s.finishing = running;
            (std::mem::take(&mut s.finished), done)
        };
        out.extend(done.into_iter().map(joined));
        out
    }

    /// A problem with the current recording, if any.
    pub fn take_failure(&self) -> Option<String> {
        let mut s = self.inner.lock().unwrap();
        s.failure.take().or_else(|| s.active.as_ref().and_then(|a| a.recorder.failure()))
    }

    /// Up to the newest `seconds`, starting where the last clip ended if that's more recent.
    pub fn clip(&self, seconds: u32) -> Option<Clip> {
        let end = self.clip_end();
        self.clip_until(seconds, end)
    }

    /// Fixes where a clip saved now ends: the newest frame. Clips start on keyframes, so this
    /// asks for one right after it, for the next clip to pick up from.
    pub fn clip_end(&self) -> i64 {
        let end = self.inner.lock().unwrap().ring.newest_video_us();
        self.want_keyframe.store(true, Ordering::Relaxed);
        end
    }

    /// Whether the sound for a clip ending at `end` is all in. Until then the clip would end in
    /// silence and the next would start without what was missing.
    pub fn audio_caught_up(&self, end: i64) -> bool {
        let s = self.inner.lock().unwrap();
        let tracks = s.tracks.as_ref().map_or(0, |t| t.audio.len());
        s.ring.audio_reached(end, tracks)
    }

    /// The clip of up to `seconds` ending at `end` (from `clip_end`), starting where the last
    /// clip ended if that's more recent.
    pub fn clip_until(&self, seconds: u32, end: i64) -> Option<Clip> {
        let mut s = self.inner.lock().unwrap();
        let packets = s.ring.snapshot_until(seconds, end);
        if packets.is_empty() {
            return None;
        }
        let tracks = s.tracks.clone()?;
        let previous_cut = s.ring.cut_to(end);
        let cut = s.ring.cut_us().unwrap_or(end);
        Some(Clip { tracks, packets, previous_cut, cut })
    }

    /// Puts the footage of a clip that wasn't saved back in reach of the next one, unless a later
    /// clip has been taken since.
    pub fn clip_failed(&self, clip: &Clip) {
        self.inner.lock().unwrap().ring.restore_cut(clip.previous_cut, clip.cut);
    }

    pub fn replay_seconds(&self) -> f64 {
        let s = self.inner.lock().unwrap();
        s.ring.seconds(s.window)
    }

    /// Seconds a clip saved right now would hold.
    pub fn clip_seconds(&self) -> f64 {
        let s = self.inner.lock().unwrap();
        s.ring.clip_seconds(s.window)
    }

    pub fn replay_bytes(&self) -> usize {
        self.inner.lock().unwrap().ring.bytes()
    }
}

/// The handle, or the result straight away if no thread could be started.
fn finish_in_background(recorder: Recorder) -> Result<(PathBuf, JoinHandle<Finished>), Finished> {
    let path = recorder.path().to_path_buf();
    let slot = Arc::new(Mutex::new(Some(recorder)));
    let theirs = slot.clone();
    let started = std::thread::Builder::new().name("finish recording".into()).spawn(move || {
        let recorder = theirs.lock().unwrap().take().unwrap();
        recorder.finish()
    });
    match started {
        Ok(handle) => Ok((path, handle)),
        Err(e) => match slot.lock().unwrap().take() {
            Some(recorder) => Err(recorder.finish()),
            None => Err(Finished { path, bytes: 0, error: Some(format!("couldn't finish the recording: {e}")) }),
        },
    }
}

fn joined((path, handle): (PathBuf, JoinHandle<Finished>)) -> Finished {
    handle.join().unwrap_or_else(|_| Finished { path, bytes: 0, error: Some("finishing the recording panicked".into()) })
}

/// "name.mp4" -> "name part 2.mp4", counting up from an existing "part N".
pub fn continued(path: &std::path::Path) -> PathBuf {
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let (base, part) = match stem.rsplit_once(" part ") {
        Some((base, n)) if n.parse::<u32>().is_ok() => (base.to_string(), n.parse::<u32>().unwrap() + 1),
        _ => (stem, 2),
    };
    path.with_file_name(format!("{base} part {part}.mp4"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::Codec;
    use std::path::Path;

    fn format(config: u8) -> VideoFormat {
        VideoFormat { codec: Codec::H264, width: 16, height: 16, fps: 10, config: vec![config] }
    }

    #[test]
    fn drops_video_until_a_keyframe() {
        let sink = Sink::new(true, 10);
        sink.video(&format(1), 0, false, vec![1]);
        assert!(sink.clip(10).is_none());
        sink.video(&format(1), 1, true, vec![1]);
        assert_eq!(sink.clip(10).unwrap().packets.len(), 1);
    }

    #[test]
    fn audio_before_video_is_ignored() {
        let sink = Sink::new(true, 10);
        sink.set_audio(vec![AudioFormat { name: "a".into(), asc: vec![] }]);
        sink.audio(0, 0, vec![1]);
        assert!(sink.clip(10).is_none());
    }

    #[test]
    fn format_change_clears_the_ring() {
        let sink = Sink::new(true, 10);
        sink.video(&format(1), 0, true, vec![1]);
        sink.video(&format(1), 1, false, vec![1]);
        sink.video(&format(2), 2, true, vec![1]);
        assert_eq!(sink.clip(10).unwrap().packets.len(), 1);
    }

    #[test]
    fn a_clip_asks_for_a_keyframe_and_the_next_starts_there() {
        let sink = Sink::new(true, 10);
        sink.video(&format(1), 0, true, vec![1]);
        for pts in 1..30 {
            sink.video(&format(1), pts, false, vec![1]);
        }
        let first = sink.clip(10).unwrap();
        assert_eq!(first.packets.len(), 30);
        assert!(sink.take_keyframe_request());
        assert!(sink.clip(10).is_none());
        sink.video(&format(1), 30, true, vec![1]);
        for pts in 31..50 {
            sink.video(&format(1), pts, false, vec![1]);
        }
        let second = sink.clip(10).unwrap();
        assert_eq!(second.packets[0].pts, 30);
        assert_eq!(second.packets.len(), 20);
    }

    #[test]
    fn a_failed_clip_can_be_saved_again() {
        let sink = Sink::new(true, 10);
        sink.video(&format(1), 0, true, vec![1]);
        let clip = sink.clip(10).unwrap();
        assert!(sink.clip(10).is_none());
        sink.clip_failed(&clip);
        assert_eq!(sink.clip(10).unwrap().packets.len(), 1);
    }

    #[test]
    fn replay_off_keeps_nothing_in_ram() {
        let sink = Sink::new(false, 10);
        sink.video(&format(1), 0, true, vec![0; 1000]);
        assert_eq!(sink.replay_bytes(), 0);
    }

    #[test]
    fn starting_a_recording_asks_for_a_keyframe() {
        let sink = Sink::new(false, 10);
        assert!(!sink.take_keyframe_request());
        sink.start_recording(std::env::temp_dir().join("tape-unused.mp4"));
        assert!(sink.take_keyframe_request());
        assert!(!sink.take_keyframe_request());
        assert!(sink.is_recording());
        assert!(sink.stop_recording().is_none());
        assert!(!sink.is_recording());
    }

    #[test]
    fn capture_keeps_running_while_a_recording_finishes() {
        let dir = std::env::temp_dir().join(format!("tape-sink-{}", std::process::id()));
        let sink = Sink::new(false, 10);
        sink.start_recording(dir.join("a.mp4"));
        sink.video(&format(1), 0, true, vec![0; 100]);
        assert!(sink.is_recording());
        let finished = sink.stop_recording().unwrap();
        assert!(finished.error.is_none(), "{:?}", finished.error);
        // The lock is free: another frame goes straight in.
        sink.video(&format(1), 1, false, vec![0; 100]);
        assert_eq!(sink.frames(), 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    fn audio_format() -> Vec<AudioFormat> {
        vec![AudioFormat { name: "a".into(), asc: vec![] }]
    }

    #[test]
    fn a_clip_ends_where_the_save_was_asked_for() {
        let sink = Sink::new(true, 10);
        sink.set_audio(audio_format());
        for pts in 0..30 {
            sink.video(&format(1), pts, pts == 0, vec![1]);
        }
        // Audio trails the video by 150 ms, as it comes out of the mixer.
        for n in 0..(2_750 * 48 / 1024) {
            sink.audio(0, n * 1024, vec![2]);
        }
        let end = sink.clip_end();
        assert!(sink.take_keyframe_request());
        assert!(!sink.audio_caught_up(end));
        // Capture goes on while the save waits for the sound.
        for pts in 30..35 {
            sink.video(&format(1), pts, pts == 30, vec![1]);
        }
        for n in (2_750 * 48 / 1024)..(3_200 * 48 / 1024) {
            sink.audio(0, n * 1024, vec![2]);
        }
        assert!(sink.audio_caught_up(end));
        let clip = sink.clip_until(10, end).unwrap();
        let video: Vec<i64> = clip.packets.iter().filter(|p| p.track == 0).map(|p| p.pts).collect();
        assert_eq!(video, (0..30).collect::<Vec<_>>());
        let last_audio = clip.packets.iter().rfind(|p| p.track == 1).unwrap();
        assert!(last_audio.micros(48_000) < 3_000_000 && last_audio.micros(48_000) + 21_334 >= 3_000_000);
    }

    #[test]
    fn with_no_audio_tracks_there_is_nothing_to_wait_for() {
        let sink = Sink::new(true, 10);
        sink.video(&format(1), 0, true, vec![1]);
        assert!(sink.audio_caught_up(sink.clip_end()));
    }

    #[test]
    fn an_older_save_failing_does_not_undo_a_newer_one() {
        let sink = Sink::new(true, 60);
        for pts in 0..30 {
            sink.video(&format(1), pts, pts % 10 == 0, vec![1]);
        }
        let a = sink.clip(60).unwrap();
        for pts in 30..60 {
            sink.video(&format(1), pts, pts % 10 == 0, vec![1]);
        }
        let b = sink.clip(60).unwrap();
        assert_eq!(b.packets[0].pts, 30);
        sink.clip_failed(&a);
        for pts in 60..90 {
            sink.video(&format(1), pts, pts % 10 == 0, vec![1]);
        }
        let c = sink.clip(60).unwrap();
        assert_eq!(c.packets[0].pts, 60, "the clip after B must not repeat it");
    }

    fn wait_finished(sink: &Sink) -> Vec<Finished> {
        for _ in 0..500 {
            let done = sink.take_finished();
            if !done.is_empty() {
                return done;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("the split file never finished");
    }

    #[test]
    fn a_split_recording_finishes_without_holding_up_capture() {
        let dir = std::env::temp_dir().join(format!("tape-sink-split-{}", std::process::id()));
        let sink = Sink::new(false, 10);
        sink.start_recording(dir.join("game.mp4"));
        sink.video(&format(1), 0, true, vec![0; 100]);
        sink.video(&format(1), 1, false, vec![0; 100]);
        sink.video(&format(2), 2, true, vec![0; 100]);
        sink.video(&format(2), 3, false, vec![0; 100]);
        let done = wait_finished(&sink);
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].path, dir.join("game.mp4"));
        assert!(done[0].error.is_none() && done[0].bytes > 0, "{:?}", done[0]);
        assert_eq!(sink.recording_stats().path, Some(dir.join("game part 2.mp4")));
        let last = sink.stop_recording().unwrap();
        assert!(last.error.is_none() && last.bytes > 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn stopping_waits_for_a_split_file_still_finishing() {
        let dir = std::env::temp_dir().join(format!("tape-sink-split-stop-{}", std::process::id()));
        let sink = Sink::new(false, 10);
        sink.start_recording(dir.join("game.mp4"));
        sink.video(&format(1), 0, true, vec![0; 100]);
        sink.video(&format(2), 1, true, vec![0; 100]);
        sink.stop_recording().unwrap();
        let done = sink.take_finished();
        assert_eq!(done.len(), 1, "stop_recording must have joined the split file");
        assert!(std::fs::metadata(&done[0].path).unwrap().len() >= done[0].bytes);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn names_continuation_files() {
        assert_eq!(continued(Path::new("a/game.mp4")), Path::new("a/game part 2.mp4"));
        assert_eq!(continued(Path::new("a/game part 2.mp4")), Path::new("a/game part 3.mp4"));
    }
}
