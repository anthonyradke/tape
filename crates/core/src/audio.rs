//! Audio timing and mixing. Devices deliver packets on their own clocks, go silent when nothing
//! is playing and disappear when unplugged, so each source is placed on a shared timeline by its
//! timestamps and read back at a fixed pace. A stalled source reads as silence and can't hold up
//! the other one or the video.

use std::collections::VecDeque;

/// Differences below this between where a packet says it belongs and where the timeline has got
/// to are clock jitter, not gaps.
const TOLERANCE: i64 = 960;
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
    step: f64,
    position: f64,
    history: [[f32; 2]; 3],
}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Self {
        Resampler { step: from as f64 / to as f64, position: 0.0, history: [[0.0; 2]; 3] }
    }

    pub fn is_identity(&self) -> bool {
        self.step == 1.0
    }

    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.is_identity() {
            out.extend_from_slice(input);
            return;
        }
        let frames = input.len() / 2;
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
        for k in 0..3 {
            let i = frames as isize - 3 + k as isize;
            let frame = [at(i, 0, &self.history), at(i, 1, &self.history)];
            self.history[k] = frame;
        }
    }
}

/// One source's samples, indexed by position on the shared timeline.
#[derive(Default)]
pub struct Timeline {
    base: Option<i64>,
    samples: VecDeque<f32>,
}

impl Timeline {
    fn end(&self) -> Option<i64> {
        self.base.map(|b| b + self.samples.len() as i64 / 2)
    }

    /// Adds stereo samples whose first frame belongs at `at`.
    pub fn push(&mut self, at: i64, stereo: &[f32]) {
        let Some(end) = self.end() else {
            self.base = Some(at);
            self.samples.extend(stereo);
            return;
        };
        let gap = at - end;
        let mut input = stereo;
        if gap > TOLERANCE {
            let fill = (gap as usize).min(MAX_BUFFERED);
            self.samples.extend(std::iter::repeat_n(0.0, fill * 2));
            if gap as usize > MAX_BUFFERED {
                // Longer than anything buffered could need: start over at the new position.
                self.samples.clear();
                self.base = Some(at);
            }
        } else if gap < -TOLERANCE {
            let drop = ((-gap) as usize * 2).min(input.len());
            input = &input[drop..];
        }
        self.samples.extend(input);
        let excess = self.samples.len().saturating_sub(MAX_BUFFERED * 2);
        if excess > 0 {
            self.samples.drain(..excess - excess % 2);
            self.base = self.base.map(|b| b + (excess / 2) as i64);
        }
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
