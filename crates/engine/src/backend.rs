//! The boundary between the platform-free engine and the code that touches hardware.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tape_core::game::Foreground;
use tape_core::ipc::{AudioSource, Probe};
use tape_core::settings::Settings;
use tape_core::sink::Sink;

/// The engine's timeline. It runs from process start and survives pipeline restarts, which is
/// what lets a recording carry on across one.
#[derive(Clone, Copy)]
pub struct Clock {
    start: Instant,
}

impl Clock {
    pub fn new() -> Self {
        Clock { start: Instant::now() }
    }

    pub fn now_us(&self) -> i64 {
        self.start.elapsed().as_micros() as i64
    }

    pub fn now_samples(&self) -> i64 {
        tape_core::packet::from_micros(self.now_us(), tape_core::packet::AUDIO_RATE)
    }
}

impl Default for Clock {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Default)]
pub struct VideoStats {
    /// Clock milliseconds at the video thread's last sign of life.
    pub beat_ms: AtomicU64,
    pub encoded: AtomicU64,
    pub dropped: AtomicU64,
    pub encode_us: AtomicU32,
    pub bytes: AtomicU64,
    pub width: AtomicU32,
    pub height: AtomicU32,
    pub running: AtomicBool,
    pub method: Mutex<String>,
    pub problem: Mutex<Option<String>>,
    /// `problem` is Windows holding capture back for a while, see `VideoStatus::transient`.
    pub transient: AtomicBool,
}

impl VideoStats {
    pub fn beat(&self, clock: &Clock) {
        self.beat_ms.store(clock.now_us() as u64 / 1000, Ordering::Relaxed);
    }
}

#[derive(Clone)]
pub struct VideoContext {
    pub sink: Arc<Sink>,
    pub clock: Clock,
    pub stop: Arc<AtomicBool>,
    pub stats: Arc<VideoStats>,
}

pub struct AudioShared {
    /// Percent, live: volume changes need no restart.
    pub system_volume: AtomicU32,
    pub mic_volume: AtomicU32,
    /// Milliseconds, live like the volumes.
    pub mic_delay_ms: AtomicU32,
    pub system: Mutex<Option<AudioSource>>,
    pub mic: Mutex<Option<AudioSource>>,
}

#[derive(Clone)]
pub struct AudioContext {
    pub sink: Arc<Sink>,
    pub clock: Clock,
    pub stop: Arc<AtomicBool>,
    pub shared: Arc<AudioShared>,
}

/// Stereo float samples, placed on the timeline.
pub struct AudioBlock {
    pub at: i64,
    pub stereo: Vec<f32>,
}

pub trait AudioInput {
    fn name(&self) -> String;
    /// Name and format, for the log.
    fn describe(&self) -> String {
        self.name()
    }
    /// Everything available now, without blocking. An error means reopen the device.
    fn read(&mut self, out: &mut Vec<AudioBlock>) -> Result<(), String>;
    /// True when this input follows the Windows default and the default has moved.
    fn is_stale(&self) -> bool {
        false
    }
}

pub trait AacEncoder {
    /// One AAC_FRAME of 16-bit stereo. Returns finished frames, which may be none while the
    /// encoder fills its delay line.
    fn encode(&mut self, pcm: &[u8]) -> Result<Vec<Vec<u8>>, String>;
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    System,
    Mic,
}

pub trait Backend: Send + Sync + 'static {
    fn probe(&self) -> Probe;
    /// Blocks until `ctx.stop` is set or the pipeline fails.
    fn run_video(&self, settings: &Settings, ctx: &VideoContext) -> Result<(), String>;
    fn open_audio(&self, source: Source, device: &str, clock: Clock) -> Result<Box<dyn AudioInput>, String>;
    fn aac_encoder(&self) -> Result<Box<dyn AacEncoder>, String>;
    /// Called on each audio thread before it opens anything.
    fn audio_thread_setup(&self) {}
    fn foreground(&self) -> Option<Foreground>;
    fn default_folder(&self) -> std::path::PathBuf;
    fn self_test(&self, settings: &Settings) -> String;
}
