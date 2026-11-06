use std::io::{BufRead, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tape_core::game::Tracker;
use tape_core::ipc::{AudioSource, Command, Event, SavedKind, Status, VideoStatus};
use tape_core::mp4::write_clip;
use tape_core::naming::{self, ClipKind};
use tape_core::record::Finished;
use tape_core::settings::{Settings, NO_DEVICE};
use tape_core::sink::Sink;

use crate::backend::{AudioContext, AudioShared, Backend, Clock, VideoContext, VideoStats};
use crate::log;
use crate::mix;

const STATUS_EVERY: Duration = Duration::from_millis(500);
/// How long the video thread may go without a sign of life. Past this it is stuck inside a
/// driver call. The engine stops reporting, and the host replaces the process.
const HANG_LIMIT_MS: u64 = 8000;
/// Audio that is still in flight when the save key is pressed.
const CLIP_SETTLE: Duration = Duration::from_millis(250);
/// A recording belongs to the game that was in front this recently.
const RECENT_GAME: Duration = Duration::from_secs(15);

const BACKOFF: [Duration; 5] = [
    Duration::from_millis(250),
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(5),
];
/// A pipeline that ran at least this long was healthy, so its next failure starts the backoff over.
const HEALTHY: Duration = Duration::from_secs(30);

/// A pipeline thread that restarts its work on any failure or panic until told to stop.
struct Worker {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

impl Worker {
    fn spawn(
        name: &'static str,
        stop: Arc<AtomicBool>,
        restarts: Arc<AtomicU32>,
        mut on_failure: impl FnMut(String) + Send + 'static,
        mut heartbeat: impl FnMut() + Send + 'static,
        mut body: impl FnMut() -> Result<(), String> + Send + 'static,
    ) -> Worker {
        let flag = stop.clone();
        let thread = thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                let mut failures = 0usize;
                while !flag.load(Ordering::Relaxed) {
                    let started = Instant::now();
                    let result = catch_unwind(AssertUnwindSafe(&mut body));
                    if flag.load(Ordering::Relaxed) {
                        break;
                    }
                    let message = match result {
                        Ok(Ok(())) => "stopped unexpectedly".to_string(),
                        Ok(Err(e)) => e,
                        Err(panic) => panic_text(&panic),
                    };
                    log!("{name}: {message}");
                    on_failure(message);
                    restarts.fetch_add(1, Ordering::Relaxed);
                    if started.elapsed() >= HEALTHY {
                        failures = 0;
                    }
                    let wait = BACKOFF[failures.min(BACKOFF.len() - 1)];
                    failures += 1;
                    let until = Instant::now() + wait;
                    while Instant::now() < until && !flag.load(Ordering::Relaxed) {
                        heartbeat();
                        thread::sleep(Duration::from_millis(50));
                    }
                }
            })
            .expect("spawn pipeline thread");
        Worker { stop, thread }
    }

    fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.thread.join();
    }
}

fn panic_text(panic: &Box<dyn std::any::Any + Send>) -> String {
    let text = panic
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown".into());
    format!("panic: {text}")
}

struct Engine {
    backend: Arc<dyn Backend>,
    settings: Settings,
    /// Encoder settings that changed during a recording, applied when it stops.
    deferred: Option<Settings>,
    sink: Arc<Sink>,
    clock: Clock,
    events: Sender<Event>,
    video: Option<Worker>,
    audio: Option<Worker>,
    stats: Arc<VideoStats>,
    audio_shared: Arc<AudioShared>,
    restarts: Arc<AtomicU32>,
    configured: bool,
    games: Tracker,
    last_bytes: u64,
    last_frames: u64,
    last_sample: Instant,
    rate_fps: f32,
    rate_kbps: u32,
}

impl Engine {
    fn wants_pipeline(&self) -> bool {
        self.configured && (self.settings.replay || self.sink.is_recording())
    }

    fn folder(&self) -> PathBuf {
        if self.settings.folder.is_empty() {
            self.backend.default_folder()
        } else {
            PathBuf::from(&self.settings.folder)
        }
    }

    fn track_game(&mut self) {
        self.games.update(self.backend.foreground(), Instant::now());
    }

    /// `game_age`: how long ago a game can have been in front and still be the one this is of.
    fn output_path(&mut self, kind: ClipKind, game_age: Duration) -> PathBuf {
        self.track_game();
        let game = self.games.game(game_age, Instant::now());
        naming::output_path(&self.folder(), &game, kind, chrono::Local::now())
    }

    fn start_pipeline(&mut self) {
        if self.video.is_none() {
            self.sink.set_audio(mix::tracks(&self.settings));
            self.video = Some(self.spawn_video());
        }
        if self.audio.is_none() {
            self.audio = Some(self.spawn_audio());
        }
    }

    fn stop_video(&mut self) {
        if let Some(w) = self.video.take() {
            w.stop();
        }
        self.stats.running.store(false, Ordering::Relaxed);
    }

    fn stop_audio(&mut self) {
        if let Some(w) = self.audio.take() {
            w.stop();
        }
        *self.audio_shared.system.lock().unwrap() = None;
        *self.audio_shared.mic.lock().unwrap() = None;
    }

    fn spawn_video(&self) -> Worker {
        let stop = Arc::new(AtomicBool::new(false));
        let ctx = VideoContext { sink: self.sink.clone(), clock: self.clock, stop: stop.clone(), stats: self.stats.clone() };
        let (backend, settings) = (self.backend.clone(), self.settings.clone());
        let (fail_stats, beat_stats, clock) = (self.stats.clone(), self.stats.clone(), self.clock);
        self.stats.beat(&self.clock);
        Worker::spawn(
            "video",
            stop,
            self.restarts.clone(),
            move |message| {
                fail_stats.running.store(false, Ordering::Relaxed);
                *fail_stats.problem.lock().unwrap() = Some(message);
            },
            move || beat_stats.beat(&clock),
            move || backend.run_video(&settings, &ctx),
        )
    }

    fn spawn_audio(&self) -> Worker {
        let stop = Arc::new(AtomicBool::new(false));
        let ctx = AudioContext { sink: self.sink.clone(), clock: self.clock, stop: stop.clone(), shared: self.audio_shared.clone() };
        let (backend, settings) = (self.backend.clone(), self.settings.clone());
        let shared = self.audio_shared.clone();
        let (has_system, has_mic) = (self.settings.system_audio != NO_DEVICE, self.settings.microphone != NO_DEVICE);
        Worker::spawn(
            "audio",
            stop,
            self.restarts.clone(),
            // Video carries on without sound, but the user has to be told.
            move |message| {
                let report = |slot: &Mutex<Option<AudioSource>>| {
                    *slot.lock().unwrap() = Some(AudioSource { problem: Some(message.clone()), ..Default::default() });
                };
                if has_system {
                    report(&shared.system);
                }
                if has_mic {
                    report(&shared.mic);
                }
            },
            || {},
            move || mix::run(backend.as_ref(), &settings, &ctx),
        )
    }

    fn reconcile(&mut self) {
        if self.wants_pipeline() {
            self.start_pipeline();
        } else {
            self.stop_audio();
            self.stop_video();
        }
    }

    fn configure(&mut self, new: Settings) {
        let new = new.normalized();
        let restart_video = new.video_differs(&self.settings);
        let restart_audio = new.audio_differs(&self.settings);
        self.configured = true;
        if self.sink.is_recording() && (restart_video || restart_audio) {
            // A change like this would split the file. It waits for the recording to end;
            // everything else (folder, volumes, replay length) applies now.
            let mut now = new.clone();
            now.monitor = self.settings.monitor.clone();
            now.resolution = self.settings.resolution;
            now.fps = self.settings.fps;
            now.codec = self.settings.codec;
            now.bitrate_mbps = self.settings.bitrate_mbps;
            now.preset = self.settings.preset;
            now.capture = self.settings.capture;
            now.show_cursor = self.settings.show_cursor;
            now.system_audio = self.settings.system_audio.clone();
            now.microphone = self.settings.microphone.clone();
            now.separate_tracks = self.settings.separate_tracks;
            self.deferred = Some(new);
            self.apply(now, false, false);
            return;
        }
        self.deferred = None;
        self.apply(new, restart_video, restart_audio);
    }

    fn apply(&mut self, new: Settings, restart_video: bool, restart_audio: bool) {
        if restart_audio {
            self.stop_audio();
        }
        if restart_video {
            self.stop_video();
        }
        if restart_audio && !restart_video {
            self.sink.set_audio(mix::tracks(&new));
        }
        self.audio_shared.system_volume.store(new.system_volume, Ordering::Relaxed);
        self.audio_shared.mic_volume.store(new.mic_volume, Ordering::Relaxed);
        self.audio_shared.mic_delay_ms.store(new.mic_delay_ms, Ordering::Relaxed);
        self.sink.set_replay(new.replay, new.replay_seconds);
        self.settings = new;
        self.reconcile();
    }

    fn save_clip(&mut self) {
        if !self.settings.replay {
            let _ = self.events.send(Event::Error { message: "Instant replay is off".into(), clip: true });
            return;
        }
        let (sink, events, seconds) = (self.sink.clone(), self.events.clone(), self.settings.replay_seconds);
        // Anything played in the last `seconds` is in the clip.
        let path = self.output_path(ClipKind::Replay, Duration::from_secs(seconds as u64));
        thread::spawn(move || {
            thread::sleep(CLIP_SETTLE);
            let event = match sink.clip(seconds) {
                None if sink.replay_seconds() > 0.0 => Event::Error { message: "Nothing new since the last clip".into(), clip: true },
                None => Event::Error { message: "Nothing has been buffered yet".into(), clip: true },
                Some(clip) => match write_file(&path, &clip) {
                    Ok(bytes) => {
                        log!("saved clip {} ({bytes} bytes)", path.display());
                        Event::Saved { kind: SavedKind::Clip, path: path.display().to_string(), bytes }
                    }
                    Err(e) => {
                        log!("clip failed: {e}");
                        sink.clip_failed(clip.previous_cut);
                        Event::Error { message: format!("Couldn't save the clip: {e}"), clip: true }
                    }
                },
            };
            let _ = events.send(event);
        });
    }

    fn start_recording(&mut self) {
        if !self.sink.is_recording() {
            let path = self.output_path(ClipKind::Recording, RECENT_GAME);
            self.sink.start_recording(path);
            self.reconcile();
        }
    }

    fn stop_recording(&mut self) {
        if let Some(finished) = self.sink.stop_recording() {
            self.report(finished);
        }
        if let Some(settings) = self.deferred.take() {
            self.configure(settings);
        }
        self.reconcile();
    }

    fn report(&self, finished: Finished) {
        log!("recording finished {} ({} bytes)", finished.path.display(), finished.bytes);
        if let Some(message) = &finished.error {
            log!("recording error: {message}");
            let _ = self.events.send(Event::Error { message: message.clone(), clip: false });
        }
        if finished.bytes > 0 {
            let path = finished.path.display().to_string();
            let _ = self.events.send(Event::Saved { kind: SavedKind::Recording, path, bytes: finished.bytes });
        }
    }

    fn hung(&self) -> bool {
        self.video.is_some() && (self.clock.now_us() as u64 / 1000).saturating_sub(self.stats.beat_ms.load(Ordering::Relaxed)) > HANG_LIMIT_MS
    }

    fn poll(&mut self) {
        for finished in self.sink.take_finished() {
            self.report(finished);
        }
        if let Some(message) = self.sink.take_failure() {
            log!("recording problem: {message}");
            let _ = self.events.send(Event::Error { message, clip: false });
            self.stop_recording();
        }
    }

    fn status(&mut self) -> Status {
        self.track_game();
        let elapsed = self.last_sample.elapsed().as_secs_f32();
        if elapsed >= 1.0 {
            let frames = self.sink.frames();
            let bytes = self.stats.bytes.load(Ordering::Relaxed);
            self.rate_fps = (frames - self.last_frames.min(frames)) as f32 / elapsed;
            self.rate_kbps = (bytes.saturating_sub(self.last_bytes) as f32 * 8.0 / 1000.0 / elapsed) as u32;
            self.last_frames = frames;
            self.last_bytes = bytes;
            self.last_sample = Instant::now();
        }
        let s = &self.stats;
        let rec = self.sink.recording_stats();
        let take_peak = |source: &Mutex<Option<AudioSource>>| {
            let mut guard = source.lock().unwrap();
            let snapshot = guard.clone();
            if let Some(status) = guard.as_mut() {
                status.peak = 0.0; // the next report shows the loudest since this one
            }
            snapshot
        };
        Status {
            video: VideoStatus {
                running: s.running.load(Ordering::Relaxed),
                width: s.width.load(Ordering::Relaxed),
                height: s.height.load(Ordering::Relaxed),
                fps: self.rate_fps,
                method: s.method.lock().unwrap().clone(),
                encode_ms: s.encode_us.load(Ordering::Relaxed) as f32 / 1000.0,
                dropped: s.dropped.load(Ordering::Relaxed),
                bitrate_kbps: self.rate_kbps,
                problem: s.problem.lock().unwrap().clone(),
            },
            system: take_peak(&self.audio_shared.system),
            mic: take_peak(&self.audio_shared.mic),
            replay_on: self.settings.replay,
            replay_seconds: self.sink.replay_seconds() as f32,
            clip_seconds: self.sink.clip_seconds() as f32,
            replay_target: self.settings.replay_seconds,
            replay_bytes: self.sink.replay_bytes() as u64,
            recording: self.sink.is_recording(),
            recording_seconds: rec.micros as f32 / 1e6,
            recording_bytes: rec.bytes,
            recording_path: rec.path.map(|p| p.display().to_string()),
            restarts: self.restarts.load(Ordering::Relaxed),
            game: self.games.game(RECENT_GAME, Instant::now()),
        }
    }
}

/// Writes under a temporary name, so a half-written clip never appears as a playable file.
fn write_file(path: &std::path::Path, clip: &tape_core::sink::Clip) -> Result<u64, String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let part = path.with_extension("part");
    let write = || -> std::io::Result<u64> {
        let file = std::fs::File::create(&part)?;
        let bytes = write_clip(std::io::BufWriter::with_capacity(1 << 20, file), &clip.tracks, &clip.packets)?;
        std::fs::rename(&part, path)?;
        Ok(bytes)
    };
    write().map_err(|e| {
        let _ = std::fs::remove_file(&part);
        e.to_string()
    })
}

/// Runs until stdin closes (the host went away) or a shutdown command, then finishes any
/// recording so the file ends cleanly.
pub fn run(backend: Arc<dyn Backend>, input: impl BufRead + Send + 'static, mut output: impl Write + Send + 'static) -> i32 {
    let (events, events_rx) = channel::<Event>();
    let writer = thread::spawn(move || {
        for event in events_rx {
            let Ok(mut line) = serde_json::to_vec(&event) else { continue };
            line.push(b'\n');
            if output.write_all(&line).and_then(|_| output.flush()).is_err() {
                break;
            }
        }
    });
    let (commands, commands_rx) = channel::<Command>();
    thread::spawn(move || {
        for line in input.lines() {
            let Ok(line) = line else { break };
            match serde_json::from_str::<Command>(&line) {
                Ok(command) => {
                    if commands.send(command).is_err() {
                        break;
                    }
                }
                Err(e) => log!("bad command {line}: {e}"),
            }
        }
    });

    log!("engine started, version {}", env!("CARGO_PKG_VERSION"));
    let settings = Settings::default();
    let mut engine = Engine {
        sink: Sink::new(settings.replay, settings.replay_seconds),
        backend,
        deferred: None,
        clock: Clock::new(),
        events: events.clone(),
        video: None,
        audio: None,
        stats: Arc::default(),
        audio_shared: Arc::new(AudioShared {
            system_volume: AtomicU32::new(settings.system_volume),
            mic_volume: AtomicU32::new(settings.mic_volume),
            mic_delay_ms: AtomicU32::new(settings.mic_delay_ms),
            system: Mutex::new(None),
            mic: Mutex::new(None),
        }),
        restarts: Arc::default(),
        configured: false,
        games: Tracker::default(),
        settings,
        last_bytes: 0,
        last_frames: 0,
        last_sample: Instant::now(),
        rate_fps: 0.0,
        rate_kbps: 0,
    };

    let mut next_status = Instant::now();
    loop {
        let wait = next_status.saturating_duration_since(Instant::now());
        match commands_rx.recv_timeout(wait) {
            Ok(Command::Configure { settings }) => engine.configure(*settings),
            Ok(Command::SaveClip) => engine.save_clip(),
            Ok(Command::StartRecording) => engine.start_recording(),
            Ok(Command::StopRecording) => engine.stop_recording(),
            Ok(Command::SetReplay { on }) => {
                let mut settings = engine.settings.clone();
                settings.replay = on;
                engine.configure(settings);
            }
            Ok(Command::Probe) => {
                let probe = engine.backend.probe();
                let _ = events.send(Event::Probe { probe: Box::new(probe) });
            }
            Ok(Command::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
        engine.poll();
        if Instant::now() >= next_status {
            next_status = Instant::now() + STATUS_EVERY;
            if engine.hung() {
                // Silence is the signal: the host replaces this process.
                log!("video thread has stopped responding");
            } else {
                let status = engine.status();
                let _ = events.send(Event::Status { status: Box::new(status) });
            }
        }
    }

    log!("engine stopping");
    if let Some(finished) = engine.sink.stop_recording() {
        engine.report(finished);
    }
    // Pipeline threads aren't joined here: if one is what's stuck, joining would hang the exit.
    engine.stats.running.store(false, Ordering::Relaxed);
    drop(engine);
    drop(events);
    let _ = writer.join();
    0
}
