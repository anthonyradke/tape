//! The audio thread: reads both sources, mixes on a fixed schedule, encodes. A device failing
//! only silences that source and is retried; it never stops the thread or touches video.

use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tape_core::audio::{add_scaled, limit, peak, to_i16, Timeline, MIX_DELAY};
use tape_core::ipc::AudioSource;
use tape_core::packet::{AudioFormat, AAC_FRAME};
use tape_core::settings::{Settings, NO_DEVICE};

use crate::backend::{AudioBlock, AudioContext, AudioInput, AudioShared, Backend, Source};

const ASC_AAC_LC_48K_STEREO: [u8; 2] = [0x11, 0x90];
const RETRY: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(5);
/// Past this much backlog the thread was stalled, and it skips ahead rather than racing through
/// audio that has nothing in it.
const MAX_BEHIND: i64 = 48_000 * 2;

pub fn tracks(settings: &Settings) -> Vec<AudioFormat> {
    let system = settings.system_audio != NO_DEVICE;
    let mic = settings.microphone != NO_DEVICE;
    let format = |name: &str| AudioFormat { name: name.into(), asc: ASC_AAC_LC_48K_STEREO.to_vec() };
    match (system, mic, settings.separate_tracks) {
        (true, true, true) => vec![format("Game audio"), format("Microphone")],
        (true, true, false) => vec![format("Game and microphone")],
        (true, false, _) => vec![format("Game audio")],
        (false, true, _) => vec![format("Microphone")],
        (false, false, _) => vec![],
    }
}

struct Channel {
    source: Source,
    device: String,
    input: Option<Box<dyn AudioInput>>,
    next_open: Instant,
    timeline: Timeline,
    blocks: Vec<AudioBlock>,
    block: Vec<f32>,
}

impl Channel {
    fn new(source: Source, device: &str) -> Self {
        Channel {
            source,
            device: device.to_string(),
            input: None,
            next_open: Instant::now(),
            timeline: Timeline::default(),
            blocks: Vec::new(),
            block: vec![0.0; AAC_FRAME * 2],
        }
    }

    fn status<'a>(&self, shared: &'a AudioShared) -> &'a std::sync::Mutex<Option<AudioSource>> {
        match self.source {
            Source::System => &shared.system,
            Source::Mic => &shared.mic,
        }
    }

    fn pump(&mut self, backend: &dyn Backend, ctx: &AudioContext) {
        if self.input.as_ref().is_some_and(|i| i.is_stale()) {
            self.input = None;
            self.next_open = Instant::now();
        }
        if self.input.is_none() && Instant::now() >= self.next_open {
            self.next_open = Instant::now() + RETRY;
            match backend.open_audio(self.source, &self.device, ctx.clock) {
                Ok(input) => {
                    let status = AudioSource { name: input.name(), running: true, peak: 0.0, problem: None };
                    *self.status(&ctx.shared).lock().unwrap() = Some(status);
                    self.input = Some(input);
                }
                Err(e) => self.fail(&ctx.shared, e),
            }
        }
        let Some(input) = &mut self.input else { return };
        self.blocks.clear();
        if let Err(e) = input.read(&mut self.blocks) {
            self.input = None;
            self.fail(&ctx.shared, e);
            return;
        }
        for b in &self.blocks {
            self.timeline.push(b.at, &b.stereo);
        }
    }

    fn fail(&self, shared: &AudioShared, problem: String) {
        let mut status = self.status(shared).lock().unwrap();
        let s = status.get_or_insert_with(AudioSource::default);
        s.running = false;
        s.peak = 0.0;
        s.problem = Some(problem);
    }
}

/// Runs until `ctx.stop` is set. The only failure that ends it is the AAC encoder, which the
/// engine restarts like any other pipeline failure.
pub fn run(backend: &dyn Backend, settings: &Settings, ctx: &AudioContext) -> Result<(), String> {
    backend.audio_thread_setup();
    let formats = tracks(settings);
    if formats.is_empty() {
        // Both sources off is a normal setup, and returning would read as a failure.
        while !ctx.stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(50));
        }
        return Ok(());
    }
    let mut channels = Vec::new();
    if settings.system_audio != NO_DEVICE {
        channels.push(Channel::new(Source::System, &settings.system_audio));
    }
    if settings.microphone != NO_DEVICE {
        channels.push(Channel::new(Source::Mic, &settings.microphone));
    }
    let separate = formats.len() == 2;
    let mut encoders = Vec::new();
    for _ in 0..formats.len() {
        encoders.push(backend.aac_encoder()?);
    }
    let mut queues: Vec<VecDeque<i64>> = vec![VecDeque::new(); formats.len()];
    let mut next = ctx.clock.now_samples() - MIX_DELAY;
    let mut mixed = vec![0.0f32; AAC_FRAME * 2];
    let mut pcm = Vec::with_capacity(AAC_FRAME * 4);

    while !ctx.stop.load(Ordering::Relaxed) {
        for c in &mut channels {
            c.pump(backend, ctx);
        }
        let now = ctx.clock.now_samples();
        if now - next > MAX_BEHIND {
            next = now - MIX_DELAY;
        }
        while next + AAC_FRAME as i64 + MIX_DELAY <= now {
            for c in &mut channels {
                c.timeline.pull(next, &mut c.block);
                let gain = volume(c.source, &ctx.shared);
                let level = peak(&c.block) * gain;
                if let Some(s) = c.status(&ctx.shared).lock().unwrap().as_mut() {
                    s.peak = s.peak.max(level);
                }
            }
            for track in 0..formats.len() {
                mixed.fill(0.0);
                for (i, c) in channels.iter().enumerate() {
                    if !separate || i == track {
                        add_scaled(&mut mixed, &c.block, volume(c.source, &ctx.shared));
                    }
                }
                limit(&mut mixed);
                pcm.clear();
                to_i16(&mixed, &mut pcm);
                queues[track].push_back(next);
                for frame in encoders[track].encode(&pcm)? {
                    if let Some(pts) = queues[track].pop_front() {
                        ctx.sink.audio(track, pts, frame);
                    }
                }
            }
            next += AAC_FRAME as i64;
        }
        std::thread::sleep(POLL);
    }
    Ok(())
}

fn volume(source: Source, shared: &Arc<AudioShared>) -> f32 {
    let percent = match source {
        Source::System => shared.system_volume.load(Ordering::Relaxed),
        Source::Mic => shared.mic_volume.load(Ordering::Relaxed),
    };
    percent as f32 / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_layout_follows_settings() {
        let names = |system: &str, mic: &str, separate| {
            let s = Settings { system_audio: system.into(), microphone: mic.into(), separate_tracks: separate, ..Default::default() };
            tracks(&s).into_iter().map(|t| t.name).collect::<Vec<_>>()
        };
        assert_eq!(names("default", "default", false), ["Game and microphone"]);
        assert_eq!(names("default", "default", true), ["Game audio", "Microphone"]);
        assert_eq!(names("default", "", true), ["Game audio"]);
        assert_eq!(names("", "x", false), ["Microphone"]);
        assert!(names("", "", false).is_empty());
    }
}
