//! Audio timing and mixing. Devices deliver packets on their own clocks, go silent when nothing
//! is playing and disappear when unplugged, so each source is placed on a shared timeline by its
//! timestamps and read back at a fixed pace. A stalled source reads as silence and can't hold up
//! the other one or the video.

use std::collections::VecDeque;

/// Differences below this between where a packet says it belongs and where the timeline has got
/// to are clock jitter or drift, not gaps, and are steered out slowly instead of filled or cut.
const TOLERANCE: i64 = 960;
/// A device's clock runs a little fast or slow against the engine's, 50-100 ppm is common. Past
/// this many samples of smoothed error the timeline starts stretching or squeezing the audio.
const DRIFT_DEADBAND: f64 = 96.0;
/// Rate change per sample of error, and its limit: 0.1%, inaudible as pitch, corrects up to
/// 1000 ppm.
const DRIFT_GAIN: f64 = 1e-6;
const MAX_DRIFT_ADJUST: f64 = 0.001;
/// Weight of each packet in the smoothed error, so timestamp jitter doesn't wobble the rate.
const DRIFT_SMOOTHING: f64 = 0.05;
/// How long the mixer waits for late packets before a block is encoded, in samples.
pub const MIX_DELAY: i64 = 7200;
const MAX_BUFFERED: usize = 48_000 * 4;

/// Interleaved input of any channel count to interleaved stereo, using standard WAV channel
/// order (FL FR FC LFE BL BR SL SR).
pub fn to_stereo(input: &[f32], channels: usize, out: &mut Vec<f32>) {
    match channels {
        0 => {}
        1 => {
            for &s in input {
                out.extend_from_slice(&[s, s]);
            }
        }
        2 => out.extend_from_slice(input),
        _ => {
            const SIDE: f32 = std::f32::consts::FRAC_1_SQRT_2;
            let norm = 1.0 / (1.0 + SIDE + SIDE);
            for f in input.chunks_exact(channels) {
                let get = |i: usize| f.get(i).copied().unwrap_or(0.0);
                let center = get(2) * SIDE;
                let left = get(0) + center + (get(4) + get(6)) * SIDE;
                let right = get(1) + center + (get(5) + get(7)) * SIDE;
                out.extend_from_slice(&[left * norm, right * norm]);
            }
        }
    }
}

/// Stereo sample rate conversion by Catmull-Rom interpolation. Only runs for devices that aren't
/// already at 48 kHz.
pub struct Resampler {
    /// Input frames per output frame.
    step: f64,
    nominal: f64,
    position: f64,
    history: [[f32; 2]; 3],
    /// Equal rates and never adjusted: samples are copied as they are.
    passthrough: bool,
}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Self {
        let step = from as f64 / to as f64;
        Resampler { step, nominal: step, position: 0.0, history: [[0.0; 2]; 3], passthrough: step == 1.0 }
    }

    pub fn is_identity(&self) -> bool {
        self.passthrough
    }

    /// Runs `scale` times faster through the input than the rates say, to follow a clock that
    /// drifts. Once used, samples are always interpolated, so switching never skips any.
    pub fn set_speed(&mut self, scale: f64) {
        self.step = self.nominal * scale;
        self.passthrough = false;
    }

    /// Where the next output frame comes from, in input frames from the start of the next buffer:
    /// zero or a little below, for the frames held back to interpolate.
    pub fn position(&self) -> f64 {
        if self.passthrough { 0.0 } else { self.position }
    }

    /// Forgets the frames held back, for a jump in the stream.
    pub fn restart(&mut self) {
        self.position = 0.0;
        self.history = [[0.0; 2]; 3];
    }

    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let frames = input.len() / 2;
        if self.passthrough {
            out.extend_from_slice(&input[..frames * 2]);
            self.remember(input);
            return;
        }
        let at = |i: isize, c: usize, h: &[[f32; 2]; 3]| -> f32 {
            if i < 0 {
                h[(3 + i) as usize][c]
            } else {
                input[i as usize * 2 + c]
            }
        };
        // `position` is measured from the first input frame and can start just before it, inside
        // the history. Interpolating needs two frames ahead, so the rest waits for the next buffer.
        while (self.position.floor() as isize) + 2 < frames as isize {
            let i = self.position.floor() as isize;
            let t = (self.position - i as f64) as f32;
            for c in 0..2 {
                let (p0, p1, p2, p3) = (
                    at(i - 1, c, &self.history),
                    at(i, c, &self.history),
                    at(i + 1, c, &self.history),
                    at(i + 2, c, &self.history),
                );
                let a = -0.5 * p0 + 1.5 * p1 - 1.5 * p2 + 0.5 * p3;
                let b = p0 - 2.5 * p1 + 2.0 * p2 - 0.5 * p3;
                let d = -0.5 * p0 + 0.5 * p2;
                out.push(((a * t + b) * t + d) * t + p1);
            }
            self.position += self.step;
        }
        self.position -= frames as f64;
        self.remember(input);
    }

    /// Keeps the last three frames, which the next buffer interpolates from.
    fn remember(&mut self, input: &[f32]) {
        let frames = input.len() / 2;
        let at = |i: isize, c: usize, h: &[[f32; 2]; 3]| -> f32 {
            if i < 0 {
                h[(3 + i) as usize][c]
            } else {
                input[i as usize * 2 + c]
            }
        };
        for k in 0..3 {
            let i = frames as isize - 3 + k as isize;
            let frame = [at(i, 0, &self.history), at(i, 1, &self.history)];
            self.history[k] = frame;
        }
    }
}

/// One source's samples, indexed by position on the shared timeline.
pub struct Timeline {
    base: Option<i64>,
    samples: VecDeque<f32>,
    /// Stretches or squeezes the audio by a fraction of a percent to follow the device's clock.
    drift: Resampler,
    /// Packet timestamps less where the timeline has got to, averaged.
    error: f64,
    steering: bool,
    jumps: u64,
    /// Overlaps cut so far, which `jumps` includes.
    cuts: u64,
    scratch: Vec<f32>,
}

impl Default for Timeline {
    fn default() -> Self {
        Timeline {
            base: None,
            samples: VecDeque::new(),
            drift: Resampler::new(crate::packet::AUDIO_RATE, crate::packet::AUDIO_RATE),
            error: 0.0,
            steering: false,
            jumps: 0,
            cuts: 0,
            scratch: Vec::new(),
        }
    }
}

impl Timeline {
    fn end(&self) -> Option<i64> {
        self.base.map(|b| b + self.samples.len() as i64 / 2)
    }

    /// Adds stereo samples whose first frame belongs at `at`. Small differences between that and
    /// where the timeline has got to are drift and are steered out by changing the rate a little;
    /// only real gaps and overlaps are filled with silence or cut.
    pub fn push(&mut self, at: i64, stereo: &[f32]) {
        let Some(end) = self.end() else {
            self.base = Some(at);
            self.append(stereo);
            return;
        };
        let gap = at - end;
        let mut input = stereo;
        if gap > TOLERANCE {
            self.jump();
            let fill = (gap as usize).min(MAX_BUFFERED);
            self.samples.extend(std::iter::repeat_n(0.0, fill * 2));
            if gap as usize > MAX_BUFFERED {
                // Longer than anything buffered could need: start over at the new position.
                self.samples.clear();
                self.base = Some(at);
            }
        } else if gap < -TOLERANCE {
            self.jump();
            self.cuts += 1;
            let drop = ((-gap) as usize * 2).min(input.len());
            input = &input[drop..];
        } else {
            // The next frame out of the resampler is the one `position` frames into this packet.
            self.steer(at as f64 + self.drift.position() - end as f64);
        }
        self.append(input);
        let excess = self.samples.len().saturating_sub(MAX_BUFFERED * 2);
        if excess > 0 {
            self.samples.drain(..excess - excess % 2);
            self.base = self.base.map(|b| b + (excess / 2) as i64);
        }
    }

    /// Gaps filled with silence or overlaps cut so far. Drift alone never causes one.
    pub fn jumps(&self) -> u64 {
        self.jumps
    }

    /// Overlaps cut so far: audio delivered for time already filled, which is always a fault. Gaps
    /// are the rest of `jumps`, and also what a loopback source does when nothing is playing.
    pub fn cuts(&self) -> u64 {
        self.cuts
    }

    fn append(&mut self, stereo: &[f32]) {
        self.scratch.clear();
        self.drift.process(stereo, &mut self.scratch);
        self.samples.extend(&self.scratch);
    }

    fn jump(&mut self) {
        self.jumps += 1;
        self.drift.restart();
        self.error = 0.0;
    }

    /// Proportional: the rate goes off nominal in step with the smoothed error, which settles
    /// where the change matches the drift (about 100 samples at 100 ppm).
    fn steer(&mut self, error: f64) {
        self.error += (error - self.error) * DRIFT_SMOOTHING;
        if !self.steering && self.error.abs() < DRIFT_DEADBAND {
            return; // a device in step with the clock is copied untouched
        }
        self.steering = true;
        // Behind (positive error) needs more output per input, so a smaller step through it.
        let adjust = (self.error * DRIFT_GAIN).clamp(-MAX_DRIFT_ADJUST, MAX_DRIFT_ADJUST);
        self.drift.set_speed(1.0 - adjust);
    }

    /// Fills `out` with `out.len() / 2` frames starting at `from`, silent wherever nothing was
    /// delivered, and forgets everything before the end of the range.
    pub fn pull(&mut self, from: i64, out: &mut [f32]) {
        out.fill(0.0);
        let frames = out.len() as i64 / 2;
        let Some(base) = self.base else { return };
        let start = from.max(base);
        let stop = (from + frames).min(base + self.samples.len() as i64 / 2);
        for frame in start..stop {
            let src = ((frame - base) * 2) as usize;
            let dst = ((frame - from) * 2) as usize;
            out[dst] = self.samples[src];
            out[dst + 1] = self.samples[src + 1];
        }
        let consumed = (from + frames - base).clamp(0, self.samples.len() as i64 / 2);
        self.samples.drain(..consumed as usize * 2);
        self.base = Some(base + consumed);
    }
}

pub fn add_scaled(dst: &mut [f32], src: &[f32], gain: f32) {
    for (d, s) in dst.iter_mut().zip(src) {
        *d += s * gain;
    }
}

/// Soft knee above 0.8 so a loud mix of game and mic rounds off instead of clipping.
pub fn limit(samples: &mut [f32]) {
    for s in samples {
        let a = s.abs();
        if a > 0.8 {
            let over = a - 0.8;
            *s = s.signum() * (0.8 + 0.2 * (over / (over + 0.2)));
        }
    }
}

pub fn peak(samples: &[f32]) -> f32 {
    samples.iter().fold(0.0, |p, s| p.max(s.abs()))
}

pub fn to_i16(samples: &[f32], out: &mut Vec<u8>) {
    for s in samples {
        let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mono_and_stereo_pass_through() {
        let mut out = vec![];
        to_stereo(&[0.5, -0.5], 1, &mut out);
        assert_eq!(out, [0.5, 0.5, -0.5, -0.5]);
        out.clear();
        to_stereo(&[0.1, 0.2], 2, &mut out);
        assert_eq!(out, [0.1, 0.2]);
    }

    #[test]
    fn surround_downmix_keeps_sides_on_their_side_and_never_clips() {
        let mut out = vec![];
        // 5.1 with only front-left.
        to_stereo(&[1.0, 0.0, 0.0, 0.0, 0.0, 0.0], 6, &mut out);
        assert!(out[0] > 0.3 && out[1] == 0.0);
        out.clear();
        to_stereo(&[1.0; 6], 6, &mut out);
        assert!(out[0] <= 1.0 && out[1] <= 1.0);
    }

    #[test]
    fn resampler_keeps_rate_and_pitch_across_buffers() {
        // 1 kHz at 44.1 kHz in 441-frame buffers, converted to 48 kHz.
        let mut r = Resampler::new(44_100, 48_000);
        let mut out = vec![];
        let tone = |n: usize| (n as f64 * 1000.0 * std::f64::consts::TAU / 44_100.0).sin() as f32;
        let mut n = 0;
        for _ in 0..100 {
            let mut buf = vec![];
            for _ in 0..441 {
                buf.extend_from_slice(&[tone(n), tone(n)]);
                n += 1;
            }
            r.process(&buf, &mut out);
        }
        let frames = out.len() / 2;
        assert!((frames as i64 - 48_000).abs() < 16, "got {frames} frames");
        // Rising zero crossings give the pitch; a discontinuity between buffers would add some.
        let mono: Vec<f32> = out.chunks(2).map(|c| c[0]).collect();
        let crossings = mono.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
        assert!((crossings as i64 - 1000).abs() <= 2, "got {crossings} crossings");
        // The largest step of a 1 kHz sine at 48 kHz is 0.1309. Anything above it is a glitch.
        let worst = mono.windows(2).skip(8).map(|w| (w[1] - w[0]).abs()).fold(0.0, f32::max);
        assert!(worst < 0.1315, "jump of {worst}");
    }

    #[test]
    fn identity_resampler_copies() {
        let mut r = Resampler::new(48_000, 48_000);
        let mut out = vec![];
        r.process(&[0.25, 0.5], &mut out);
        assert_eq!(out, [0.25, 0.5]);
    }

    fn pull(t: &mut Timeline, from: i64, frames: usize) -> Vec<f32> {
        let mut out = vec![9.0; frames * 2];
        t.pull(from, &mut out);
        out
    }

    #[test]
    fn contiguous_packets_read_back_in_order() {
        let mut t = Timeline::default();
        t.push(100, &[1.0, 1.0, 2.0, 2.0]);
        t.push(102, &[3.0, 3.0]);
        assert_eq!(pull(&mut t, 100, 3), [1.0, 1.0, 2.0, 2.0, 3.0, 3.0]);
    }

    #[test]
    fn missing_ranges_read_as_silence() {
        let mut t = Timeline::default();
        t.push(10, &[1.0, 1.0]);
        assert_eq!(pull(&mut t, 8, 4), [0.0, 0.0, 0.0, 0.0, 1.0, 1.0, 0.0, 0.0]);
    }

    #[test]
    fn silence_gap_stays_in_place() {
        // Loopback delivers nothing while nothing plays.
        let mut t = Timeline::default();
        t.push(0, &[1.0, 1.0]);
        t.push(5000, &[2.0, 2.0]);
        assert_eq!((t.jumps(), t.cuts()), (1, 0));
        let out = pull(&mut t, 0, 5001);
        assert_eq!(out[0], 1.0);
        assert_eq!(out[2], 0.0);
        assert_eq!(out[5000 * 2], 2.0);
    }

    #[test]
    fn jitter_within_tolerance_does_not_add_gaps() {
        let mut t = Timeline::default();
        t.push(0, &[1.0, 1.0]);
        t.push(11, &[2.0, 2.0]); // 10 frames "late", under tolerance
        assert_eq!(pull(&mut t, 0, 2), [1.0, 1.0, 2.0, 2.0]);
    }

    #[test]
    fn overlap_beyond_tolerance_drops_the_old_part() {
        let mut t = Timeline::default();
        let block = vec![1.0; 4000];
        t.push(0, &block);
        let mut next = vec![2.0; 4000];
        next[2 * 1500] = 3.0;
        t.push(500, &next); // 1500 frames early
        assert_eq!((t.jumps(), t.cuts()), (1, 1));
        let out = pull(&mut t, 0, 2001);
        assert_eq!(out[2 * 1999], 1.0);
        assert_eq!(out[2 * 2000], 3.0);
    }

    #[test]
    fn huge_gap_restarts_instead_of_buffering_seconds_of_silence() {
        let mut t = Timeline::default();
        t.push(0, &[1.0, 1.0]);
        t.push(48_000 * 3600, &[2.0, 2.0]);
        assert!(t.samples.len() <= 2);
        assert_eq!(pull(&mut t, 48_000 * 3600, 1), [2.0, 2.0]);
    }

    /// Ten minutes of a device whose clock runs `ppm` off the engine's, in 10 ms packets with
    /// timestamp jitter, read back by a mixer the way mix.rs does. Returns the timeline and every
    /// frame read, with the timeline's error at the end.
    fn drifting(ppm: f64) -> (Timeline, Vec<f32>, f64) {
        let mut t = Timeline::default();
        let mut out = Vec::new();
        let mut next = -MIX_DELAY;
        let mut block = vec![0.0; 1024 * 2];
        let packet = vec![1.0f32; 480 * 2];
        let mut seed = 1u32;
        let mut worst = 0.0f64;
        for k in 0..60_000i64 {
            // 480 device samples take 480 / (1 + ppm) samples of engine time.
            let true_at = k as f64 * 480.0 / (1.0 + ppm * 1e-6);
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            let jitter = (seed >> 16) as f64 / 65_536.0 * 96.0 - 48.0;
            t.push((true_at + jitter).round() as i64, &packet);
            if k > 6_000 {
                // Where the newest sample sits against where its timestamp says.
                let error = true_at + 480.0 - t.end().unwrap() as f64;
                worst = worst.max(error.abs());
            }
            let now = (true_at + 480.0) as i64;
            while next + 1024 + MIX_DELAY <= now {
                t.pull(next, &mut block);
                out.extend_from_slice(&block);
                next += 1024;
            }
        }
        (t, out, worst)
    }

    #[test]
    fn clock_drift_is_steered_out_without_gaps() {
        for ppm in [100.0, -100.0, 50.0] {
            let (t, out, worst) = drifting(ppm);
            assert_eq!(t.jumps(), 0, "{ppm} ppm: silence inserted or audio dropped");
            // The mixer starts MIX_DELAY before the first packet; from there on every frame read
            // must be sound.
            let holes = out.iter().skip(2 * 8 * 1024).filter(|s| **s < 0.99).count();
            assert_eq!(holes, 0, "{ppm} ppm: {holes} silent samples");
            // Settles near 100 samples at 100 ppm; the timeline never drifts near the tolerance.
            assert!(worst < 400.0, "{ppm} ppm: off by {worst} samples");
        }
    }

    #[test]
    fn a_device_in_step_is_copied_untouched() {
        let mut t = Timeline::default();
        for k in 0..100 {
            let packet: Vec<f32> = (0..960).map(|i| ((k * 960 + i) % 7) as f32).collect();
            t.push(k as i64 * 480 + (k % 3) as i64 * 20 - 20, &packet);
        }
        assert!(t.drift.is_identity());
        let out = pull(&mut t, -20, 4);
        assert_eq!(out, [0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 0.0]);
    }

    #[test]
    fn unread_samples_are_bounded() {
        let mut t = Timeline::default();
        for i in 0..1000 {
            t.push(i * 4800, &vec![0.5; 9600]);
        }
        assert!(t.samples.len() <= MAX_BUFFERED * 2);
    }

    #[test]
    fn limiter_only_touches_loud_samples() {
        let mut s = [0.5, 3.0, -3.0];
        limit(&mut s);
        assert_eq!(s[0], 0.5);
        assert!(s[1] > 0.8 && s[1] < 1.0);
        assert_eq!(s[2], -s[1]);
    }

    #[test]
    fn pcm_conversion_clamps() {
        let mut out = vec![];
        to_i16(&[2.0, -2.0], &mut out);
        assert_eq!(out, [0xff, 0x7f, 0x01, 0x80]);
    }
}
