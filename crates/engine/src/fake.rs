//! A backend with no hardware behind it, for running the whole engine on any machine. Video and
//! audio come from pre-encoded elementary streams and `TAPE_FAKE_*` variables inject failures.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tape_core::game::Foreground;
use tape_core::ipc::{AudioDevice, Monitor, Probe};
use tape_core::mp4::codec::parse_frame;
use tape_core::packet::{from_micros, Codec, VideoFormat, AAC_FRAME, AUDIO_RATE};
use tape_core::settings::Settings;

use crate::backend::{AacEncoder, AudioBlock, AudioInput, Backend, Clock, Source, VideoContext};

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
        }
    }
}

struct Tone {
    clock: Clock,
    next: i64,
    fails: u32,
}

impl AudioInput for Tone {
    fn name(&self) -> String {
        "Fake device".into()
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
            out.push(AudioBlock { at: self.next, stereo: vec![0.1; frames * 2] });
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
        loop {
            if ctx.stop.load(Ordering::Relaxed) {
                return Ok(());
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
            if force_key && index % self.gop != 0 {
                index += self.gop - index % self.gop;
            }
            force_key = false;
            let frame = &self.video[index % self.video.len()];
            let key = index % self.gop == 0;
            let parsed = parse_frame(Codec::H264, frame);
            if let Some(config) = parsed.config {
                format.config = config;
            }
            ctx.stats.bytes.fetch_add(parsed.sample.len() as u64, Ordering::Relaxed);
            *ctx.stats.problem.lock().unwrap() = None;
            ctx.sink.video(&format, from_micros(next_tick, settings.fps), key, parsed.sample);
            produced += 1;
            index += 1;
            next_tick += period;
            let wait = next_tick - ctx.clock.now_us();
            if wait > 0 {
                std::thread::sleep(Duration::from_micros(wait as u64));
            }
        }
    }

    fn open_audio(&self, _source: Source, _device: &str, clock: Clock) -> Result<Box<dyn AudioInput>, String> {
        let fails = self.audio_fails.swap(0, Ordering::Relaxed);
        Ok(Box::new(Tone { clock, next: 0, fails }))
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
/// progress shows. `TAPE_FAKE_SHARE_FAIL` makes it fail, for the UI.
pub fn share_copy(input: &std::path::Path, output: &std::path::Path, progress: &mut dyn FnMut(f64)) -> Result<(u32, u32, f64, &'static str), String> {
    if let Ok(reason) = std::env::var("TAPE_FAKE_SHARE_FAIL") {
        return Err(reason);
    }
    for step in 1..=4 {
        std::thread::sleep(Duration::from_millis(150));
        progress(step as f64 / 4.0);
    }
    std::fs::copy(input, output).map_err(|e| e.to_string())?;
    Ok((1920, 1080, 0.0, "copy"))
}
