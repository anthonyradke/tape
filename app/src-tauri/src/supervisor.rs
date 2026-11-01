//! Keeps the engine alive. It runs as a child process, so the failures that would otherwise take
//! the whole app down silently (a crash or hang inside a graphics driver) are only a restart.
//! If it still can't get back to work, the user hears about it instead of finding out later that
//! nothing was being saved.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command as Process, Stdio};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{channel, RecvTimeoutError};
use std::time::{Duration, Instant};

use tape_core::ipc::{Command, Event, SavedKind, Status};
use tauri::{AppHandle, Emitter, Manager};

use crate::cue::{self, Cue};
use crate::state::{self, Light, Shared};
use crate::tray;

/// The engine reports twice a second. This much silence means it's stuck.
const SILENCE_LIMIT: Duration = Duration::from_secs(5);
/// How long replay may be down before an alert sound. Short restarts stay silent.
const ALERT_AFTER: Duration = Duration::from_secs(10);
/// An engine that lasted this long was fine, so the next crash restarts it immediately.
const STABLE: Duration = Duration::from_secs(60);
const BACKOFF: [Duration; 4] = [Duration::from_millis(250), Duration::from_secs(1), Duration::from_secs(2), Duration::from_secs(5)];
/// A loop that should take at most a second took this long: the PC was asleep, not the engine.
const SLEPT: Duration = Duration::from_secs(3);
/// This many engine restarts within `CRASH_WINDOW` also earn the alert, even if each one comes
/// back fine: every restart empties the replay buffer.
const CRASH_LIMIT: usize = 3;
const CRASH_WINDOW: Duration = Duration::from_secs(120);

/// Below this a recording holds only its header: the first fragment is megabytes.
const EMPTY_RECORDING_BYTES: u64 = 16 * 1024;

/// Decides when a failure deserves a sound. It outlives any one engine, because a crash loop is
/// the case it most needs to catch.
#[derive(Default)]
struct Alarm {
    down_since: Option<Instant>,
    crashes: Vec<Instant>,
    alerted: bool,
}

impl Alarm {
    fn working(&mut self) {
        self.down_since = None;
        if self.crashes.is_empty() {
            self.alerted = false;
        }
    }

    fn down(&mut self) {
        self.down_since.get_or_insert_with(Instant::now);
    }

    fn crashed(&mut self) {
        self.down();
        self.crashes.push(Instant::now());
    }

    fn check(&mut self, app: &AppHandle) {
        self.crashes.retain(|t| t.elapsed() < CRASH_WINDOW);
        let long_down = self.down_since.is_some_and(|since| since.elapsed() >= ALERT_AFTER);
        if (long_down || self.crashes.len() >= CRASH_LIMIT) && !self.alerted {
            self.alerted = true;
            if app.state::<Shared>().settings.lock().unwrap().sounds {
                cue::play(Cue::Failed);
            }
        }
    }
}

pub fn start(app: AppHandle) {
    std::thread::Builder::new()
        .name("supervisor".into())
        .spawn(move || run(app))
        .expect("spawn supervisor");
}

/// This same executable, with no console window.
pub fn own_process() -> std::io::Result<Process> {
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut command = Process::new(std::env::current_exe()?);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    Ok(command)
}

fn spawn_engine(shared: &Shared) -> std::io::Result<Child> {
    let mut command = own_process()?;
    command.arg("--engine").arg(&shared.log_path).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    command.spawn()
}

fn run(app: AppHandle) {
    let mut failures = 0usize;
    let mut alarm = Alarm::default();
    loop {
        let shared = app.state::<Shared>();
        if shared.quitting.load(Ordering::Relaxed) {
            return;
        }
        let started = Instant::now();
        match spawn_engine(&shared) {
            Ok(child) => {
                let reason = watch(&app, child, &mut alarm);
                if shared.quitting.load(Ordering::Relaxed) {
                    return;
                }
                *shared.engine_stdin.lock().unwrap() = None;
                let mut health = shared.health.lock().unwrap();
                health.engine_restarts += 1;
                health.last_restart = Some(reason);
            }
            Err(e) => {
                let message = format!("couldn't start the engine: {e}");
                shared.health.lock().unwrap().last_restart = Some(message.clone());
                set_trouble(&app, &message);
            }
        }
        alarm.crashed();
        alarm.check(&app);
        if started.elapsed() >= STABLE {
            failures = 0;
        }
        std::thread::sleep(BACKOFF[failures.min(BACKOFF.len() - 1)]);
        failures += 1;
    }
}

/// Runs one engine until it dies or stops responding, and says why.
fn watch(app: &AppHandle, mut child: Child, alarm: &mut Alarm) -> String {
    let shared = app.state::<Shared>();
    let stdout = child.stdout.take().unwrap();
    *shared.engine_stdin.lock().unwrap() = child.stdin.take();
    shared.engine_generation.fetch_add(1, Ordering::Relaxed);

    let (tx, rx) = channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Ok(event) = serde_json::from_str::<Event>(&line) {
                if tx.send(event).is_err() {
                    break;
                }
            }
        }
    });

    // Bring the new engine up to where the last one was.
    let settings = shared.settings.lock().unwrap().clone();
    state::send(app, &Command::Configure { settings: Box::new(settings) });
    state::send(app, &Command::Probe);
    if shared.recording_wanted.load(Ordering::Relaxed) {
        state::send(app, &Command::StartRecording);
    }

    let mut last_event = Instant::now();
    let mut last_loop = Instant::now();
    loop {
        alarm.check(app);
        let event = rx.recv_timeout(Duration::from_secs(1));
        if last_loop.elapsed() >= SLEPT {
            last_event = Instant::now();
        }
        last_loop = Instant::now();
        match event {
            Ok(event) => {
                last_event = Instant::now();
                handle(app, event, alarm);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                let code = child.wait().ok().and_then(|s| s.code());
                remove_empty_recording(&shared);
                set_trouble(app, "The engine stopped, restarting it");
                return match code {
                    Some(code) => format!("the engine crashed (0x{code:08X})"),
                    None => "the engine crashed".into(),
                };
            }
        }
        if shared.quitting.load(Ordering::Relaxed) {
            finish(&mut child);
            return String::new();
        }
        if last_event.elapsed() >= SILENCE_LIMIT {
            // Stuck inside a driver call, which nothing in that process can get out of.
            let _ = child.kill();
            let _ = child.wait();
            remove_empty_recording(&shared);
            set_trouble(app, "The engine stopped responding, restarting it");
            return "the engine stopped responding".into();
        }
    }
}

/// A crash in a recording's first seconds leaves a file with a header and no video in it.
fn remove_empty_recording(shared: &Shared) {
    let path = shared.status.lock().unwrap().recording_path.clone();
    let Some(path) = path else { return };
    if std::fs::metadata(&path).is_ok_and(|m| m.len() < EMPTY_RECORDING_BYTES) {
        let _ = std::fs::remove_file(&path);
    }
}

fn set_trouble(app: &AppHandle, problem: &str) {
    let shared = app.state::<Shared>();
    {
        let mut health = shared.health.lock().unwrap();
        health.light = Light::Trouble;
        health.problem = Some(problem.to_string());
    }
    // The buffer died with the engine, so nothing it last reported is true any more.
    let settings = shared.settings.lock().unwrap().clone();
    *shared.status.lock().unwrap() =
        Status { replay_on: settings.replay, replay_target: settings.replay_seconds, ..Status::default() };
    tray::refresh(app);
    let _ = app.emit("changed", ());
}

/// Lets a shutting-down engine finish its file. The supervisor is its only owner by now.
fn finish(child: &mut Child) {
    let until = Instant::now() + Duration::from_secs(4);
    while Instant::now() < until {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
}

fn handle(app: &AppHandle, event: Event, alarm: &mut Alarm) {
    let shared = app.state::<Shared>();
    let sounds = shared.settings.lock().unwrap().sounds;
    match event {
        Event::Status { status } => {
            let wants_video = status.replay_on || status.recording;
            // A problem with video running means capture is down and the encoder is repeating
            // the last image, which from where the user sits isn't working either.
            let working = status.video.running && status.video.problem.is_none();
            let audio_problem = [&status.system, &status.mic]
                .into_iter()
                .flatten()
                .find_map(|a| a.problem.clone());

            let health = {
                let mut health = shared.health.lock().unwrap();
                let before = (health.light, health.problem.clone());
                if wants_video && !working {
                    health.light = Light::Trouble;
                    health.problem = Some(status.video.problem.clone().unwrap_or_else(|| "Starting capture".into()));
                    alarm.down();
                } else {
                    alarm.working();
                    health.problem = audio_problem;
                    health.light = if status.recording {
                        Light::Recording
                    } else if status.replay_on {
                        Light::Replay
                    } else {
                        Light::Idle
                    };
                }
                (before != (health.light, health.problem.clone())).then(|| health.clone())
            };
            *shared.status.lock().unwrap() = *status.clone();
            if health.is_some() {
                tray::refresh(app);
                let _ = app.emit("changed", ());
            }
            let _ = app.emit("status", &*status);
        }
        Event::Probe { probe } => {
            *shared.probe.lock().unwrap() = *probe;
            let _ = app.emit("changed", ());
        }
        Event::Saved { kind, path, .. } => {
            if sounds {
                cue::play(if kind == SavedKind::Clip { Cue::ClipSaved } else { Cue::RecordingStopped });
            }
            // The gallery reloads on this, and the status that says it's finished is up to half a
            // second away. Until then the file would count as still being written.
            {
                let mut status = shared.status.lock().unwrap();
                if status.recording_path.as_ref() == Some(&path) {
                    status.recording_path = None;
                }
            }
            let _ = app.emit("saved", path);
            let _ = app.emit("changed", ());
        }
        Event::Error { message, clip } => {
            if clip && sounds {
                cue::play(Cue::Failed);
            }
            if !clip {
                // The engine stopped the recording itself.
                shared.recording_wanted.store(false, Ordering::Relaxed);
            }
            let _ = app.emit("error", message);
        }
    }
}

pub fn shutdown(app: &AppHandle) {
    let shared = app.state::<Shared>();
    shared.quitting.store(true, Ordering::Relaxed);
    state::send(app, &Command::Shutdown);
    // Time for a recording to be finished off before the process goes.
    let until = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < until && shared.status.lock().unwrap().recording {
        std::thread::sleep(Duration::from_millis(50));
    }
}
