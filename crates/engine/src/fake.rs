//! A backend with no hardware behind it, for running the whole engine on any machine. Video and
//! audio come from pre-encoded elementary streams and `TAPE_FAKE_*` variables inject failures.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tape_core::game::Foreground;
use tape_core::ipc::{AudioDevice, CodecLimit, Monitor, Probe};
use tape_core::mp4::codec::parse_frame;
use tape_core::packet::{from_micros, Codec, VideoFormat, AAC_FRAME, AUDIO_RATE};
use tape_core::settings::Settings;

use crate::backend::{AacEncoder, AudioBlock, AudioInput, Backend, Clock, Source, VideoContext};
use crate::log;

/// Reads length-prefixed frames: a little-endian u32, then one access unit.
fn read_frames(path: &str) -> Vec<Vec<u8>> {
    let data = std::fs::read(path).unwrap_or_default();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 4 <= data.len() {
        let size = u32::from_le_bytes(data[i..i + 4].try_into().unwrap()) as usize;
        out.push(data[i + 4..i + 4 + size].to_vec());
        i += 4 + size;
    }
    out
}

pub struct Fake {
    video: Vec<Vec<u8>>,
    audio: Arc<Vec<Vec<u8>>>,
    folder: PathBuf,
    gop: usize,
    /// Fail the video pipeline after this many frames, this many times.
    fail_after: u32,
    failures_left: AtomicU32,
    /// Sleep forever after this many frames, as a driver hang would.
    hang_after: u32,
    /// Abort the process after this many frames, as a driver crash would.
    crash_after: u32,
    audio_fails: AtomicU32,
    /// The AAC encoder can't be created, as on Windows N.
    no_aac: bool,
    /// Report capture as paused after this many frames, as the lock screen does, and keep
    /// encoding the last image.
    pause_after: u32,
    /// Skip one frame in every this many, as capture missing its time under load does.
    drop_every: u32,
    /// Deliver one block in every this many as overlapping the last, which the mixer has to cut.
    glitch_every: u32,
    /// Leaving tape's sounds out of game audio fails, as before Windows 10 2004.
    no_exclusion: bool,
}

impl Fake {
    pub fn from_env() -> Fake {
        let var = |name: &str| std::env::var(name).unwrap_or_default();
        let number = |name: &str| var(name).parse().unwrap_or(0);
        Fake {
            video: read_frames(&var("TAPE_FAKE_VIDEO")),
            audio: Arc::new(read_frames(&var("TAPE_FAKE_AUDIO"))),
            folder: PathBuf::from(var("TAPE_FAKE_FOLDER")),
            gop: number("TAPE_FAKE_GOP").max(1) as usize,
            fail_after: number("TAPE_FAKE_FAIL_AFTER"),
            failures_left: AtomicU32::new(number("TAPE_FAKE_FAILURES")),
            hang_after: number("TAPE_FAKE_HANG_AFTER"),
            crash_after: number("TAPE_FAKE_CRASH_AFTER"),
            audio_fails: AtomicU32::new(number("TAPE_FAKE_AUDIO_FAILS")),
            no_aac: number("TAPE_FAKE_NO_AAC") != 0,
            pause_after: number("TAPE_FAKE_PAUSE_AFTER"),
            drop_every: number("TAPE_FAKE_DROP_EVERY"),
            glitch_every: number("TAPE_FAKE_GLITCH_EVERY"),
            no_exclusion: number("TAPE_FAKE_NO_EXCLUSION") != 0,
        }
    }
}

struct Tone {
    clock: Clock,
    next: i64,
    fails: u32,
    glitch_every: u32,
    blocks: u32,
    excludes_tape: bool,
}

impl AudioInput for Tone {
    fn name(&self) -> String {
        "Fake device".into()
    }

    fn excludes_tape(&self) -> bool {
        self.excludes_tape
    }

    fn read(&mut self, out: &mut Vec<AudioBlock>) -> Result<(), String> {
        if self.fails > 0 {
            self.fails -= 1;
            return Err("fake device unplugged".into());
        }
        let now = self.clock.now_samples();
        if self.next == 0 {
            self.next = now;
        }
        let frames = (now - self.next).max(0) as usize;
        if frames > 0 {
            self.blocks += 1;
            // 50 ms early: well past what the timeline steers out.
            let early = if self.glitch_every > 0 && self.blocks.is_multiple_of(self.glitch_every) { 2400 } else { 0 };
            out.push(AudioBlock { at: self.next - early, stereo: vec![0.1; frames * 2], discontinuity: false });
            self.next = now;
        }
        Ok(())
    }
}

/// Plays back pre-encoded AAC frames, ignoring the audio it's given.
struct Canned {
    frames: Arc<Vec<Vec<u8>>>,
    next: usize,
}

impl AacEncoder for Canned {
    fn encode(&mut self, pcm: &[u8]) -> Result<Vec<Vec<u8>>, String> {
        assert_eq!(pcm.len(), AAC_FRAME * 4);
        self.next += 1;
        Ok(vec![self.frames[self.next % self.frames.len()].clone()])
    }
}

impl Backend for Fake {
    fn probe(&self) -> Probe {
        Probe {
            gpu: "Fake GPU".into(),
            driver: "0".into(),
            codecs: vec![Codec::H264],
            limits: vec![CodecLimit { codec: Codec::H264, max_width: 4096, max_height: 4096 }],
            nvenc_error: None,
            monitors: vec![Monitor { id: "fake".into(), name: "Fake monitor".into(), width: 640, height: 360, hz: 60, primary: true }],
            outputs: vec![AudioDevice { id: "out".into(), name: "Fake speakers".into() }],
            inputs: vec![AudioDevice { id: "in".into(), name: "Fake microphone".into() }],
        }
    }

    fn run_video(&self, settings: &Settings, ctx: &VideoContext) -> Result<(), String> {
        let mut format = VideoFormat { codec: Codec::H264, width: 640, height: 360, fps: settings.fps, config: vec![] };
        *ctx.stats.method.lock().unwrap() = "fake".into();
        ctx.stats.width.store(640, Ordering::Relaxed);
        ctx.stats.height.store(360, Ordering::Relaxed);
        ctx.stats.running.store(true, Ordering::Relaxed);
        let mut produced = 0u32;
        let mut index = 0usize;
        let mut force_key = true;
        let period = 1_000_000 / settings.fps as i64;
        let mut next_tick = ctx.clock.now_us();
        // Logged like the real pipeline, so tests can tell a new session from a change in place.
        let live = |fallback: u32| match ctx.bitrate_mbps.load(Ordering::Relaxed) {
            0 => fallback,
            mbps => mbps,
        };
        let mut bitrate = live(settings.bitrate_mbps);
        log!("video: fake pipeline started at {bitrate} Mbps");
        loop {
            if ctx.stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            if live(bitrate) != bitrate {
                bitrate = live(bitrate);
                log!("video: bitrate now {bitrate} Mbps, same session");
                force_key = true;
            }
            ctx.stats.beat(&ctx.clock);
            if self.hang_after > 0 && produced >= self.hang_after {
                loop {
                    std::thread::sleep(Duration::from_secs(3600));
                }
            }
            if self.crash_after > 0 && produced >= self.crash_after {
                std::process::abort();
            }
            if self.fail_after > 0 && produced >= self.fail_after {
                let left = self.failures_left.load(Ordering::Relaxed);
                if left > 0 {
                    self.failures_left.store(left - 1, Ordering::Relaxed);
                    return Err("fake encoder failure".into());
                }
            }
            force_key |= ctx.sink.take_keyframe_request();
            // Jump to the next keyframe in the canned stream when one is asked for.
            if force_key && !index.is_multiple_of(self.gop) {
                index += self.gop - index % self.gop;
            }
            force_key = false;
            let frame = &self.video[index % self.video.len()];
            let key = index.is_multiple_of(self.gop);
            let parsed = parse_frame(Codec::H264, frame);
            if let Some(config) = parsed.config {
                format.config = config;
            }
            ctx.stats.bytes.fetch_add(parsed.sample.len() as u64, Ordering::Relaxed);
            if self.pause_after > 0 && produced >= self.pause_after {
                ctx.stats.transient.store(true, Ordering::Relaxed);
                *ctx.stats.problem.lock().unwrap() = Some("Capture is paused by a Windows security prompt or the lock screen".into());
            } else {
                *ctx.stats.problem.lock().unwrap() = None;
            }
            ctx.sink.video(&format, from_micros(next_tick, settings.fps), key, parsed.sample);
            produced += 1;
            index += 1;
            next_tick += period;
            if self.drop_every > 0 && produced.is_multiple_of(self.drop_every) {
                next_tick += period;
                ctx.stats.dropped.fetch_add(1, Ordering::Relaxed);
            }
            let wait = next_tick - ctx.clock.now_us();
            if wait > 0 {
                std::thread::sleep(Duration::from_micros(wait as u64));
            }
        }
    }

    fn open_audio(&self, _source: Source, _device: &str, clock: Clock) -> Result<Box<dyn AudioInput>, String> {
        let fails = self.audio_fails.swap(0, Ordering::Relaxed);
        Ok(Box::new(Tone { clock, next: 0, fails, glitch_every: self.glitch_every, blocks: 0, excludes_tape: false }))
    }

    fn open_excluding(&self, _pid: u32, clock: Clock) -> Result<Box<dyn AudioInput>, String> {
        if self.no_exclusion {
            return Err("process loopback unavailable".into());
        }
        let fails = self.audio_fails.swap(0, Ordering::Relaxed);
        Ok(Box::new(Tone { clock, next: 0, fails, glitch_every: self.glitch_every, blocks: 0, excludes_tape: true }))
    }

    fn aac_encoder(&self) -> Result<Box<dyn AacEncoder>, String> {
        if self.no_aac {
            return Err("AAC encoder unavailable".into());
        }
        Ok(Box::new(Canned { frames: self.audio.clone(), next: 0 }))
    }

    fn foreground(&self) -> Option<Foreground> {
        Some(Foreground { title: "Test Game".into(), exe: r"C:\games\test.exe".into(), fullscreen: true })
    }

    fn default_folder(&self) -> PathBuf {
        self.folder.clone()
    }

    fn self_test(&self, _settings: &Settings) -> String {
        format!("fake backend, {} frames at {} Hz audio", self.video.len(), AUDIO_RATE)
    }
}

/// Stands in for the shared copy where there's no Media Foundation: the clip itself, in steps so
/// progress shows, or a trim of it for a range. `TAPE_FAKE_SHARE_FAIL` makes it fail, for the UI.
pub fn share_copy(
    input: &std::path::Path,
    output: &std::path::Path,
    range: Option<(f64, f64)>,
    progress: &mut dyn FnMut(f64),
) -> Result<(u32, u32, f64, &'static str), String> {
    if let Ok(reason) = std::env::var("TAPE_FAKE_SHARE_FAIL") {
        return Err(reason);
    }
    for step in 1..=4 {
        std::thread::sleep(Duration::from_millis(150));
        progress(step as f64 / 4.0);
    }
    if let Some((start, end)) = range {
        let (begin, finish) = tape_core::mp4::trim::trim(input, output, start, end).map_err(|e| e.to_string())?;
        return Ok((1920, 1080, finish - begin, "trim"));
    }
    std::fs::copy(input, output).map_err(|e| e.to_string())?;
    Ok((1920, 1080, 0.0, "copy"))
}
