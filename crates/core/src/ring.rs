//! The instant replay buffer: encoded packets in RAM, oldest evicted as new ones arrive. Nothing
//! is written to disk until a clip is saved.

use std::collections::VecDeque;

use crate::packet::{Packet, Tracks};

/// Longest gap between keyframes, plus slack. A clip has to start on a keyframe, so the ring
/// keeps this much beyond the replay length.
const KEYFRAME_MARGIN_US: i64 = 3_000_000;
/// A clip right after another must have at least this much new footage, so a double press doesn't
/// leave a file a few frames long.
const MIN_FOLLOW_UP_US: i64 = 1_000_000;

#[derive(Default)]
pub struct Ring {
    packets: VecDeque<(i64, Packet)>,
    bytes: usize,
    retain_us: i64,
    newest_us: i64,
    newest_video_us: i64,
    /// The last video frame in the previous clip. The next clip starts after it.
    cut_us: Option<i64>,
}

impl Ring {
    pub fn new(seconds: u32) -> Self {
        let mut r = Ring::default();
        r.set_length(seconds);
        r
    }

    pub fn set_length(&mut self, seconds: u32) {
        self.retain_us = seconds as i64 * 1_000_000 + KEYFRAME_MARGIN_US;
        self.evict();
    }

    pub fn clear(&mut self) {
        self.packets.clear();
        self.bytes = 0;
        self.newest_us = 0;
        self.newest_video_us = 0;
        self.cut_us = None;
    }

    pub fn push(&mut self, packet: Packet, tracks: &Tracks) {
        let us = packet.micros(tracks.timescale(packet.track));
        self.newest_us = self.newest_us.max(us);
        if packet.track == 0 {
            self.newest_video_us = us;
        }
        self.bytes += packet.data.len();
        self.packets.push_back((us, packet));
        self.evict();
    }

    fn evict(&mut self) {
        let cutoff = self.newest_us - self.retain_us;
        while self.packets.front().is_some_and(|(us, _)| *us < cutoff) {
            let (_, p) = self.packets.pop_front().unwrap();
            self.bytes -= p.data.len();
        }
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    fn first_key_us(&self) -> Option<i64> {
        self.packets.iter().find(|(_, p)| p.track == 0 && p.key).map(|(us, _)| *us)
    }

    /// Seconds a clip saved right now would hold.
    pub fn seconds(&self, window: u32) -> f64 {
        let Some(first) = self.first_key_us() else { return 0.0 };
        ((self.newest_us - first) as f64 / 1e6).min(window as f64)
    }

    /// Seconds a clip saved right now would hold, leaving out what the last clip already has.
    pub fn clip_seconds(&self, window: u32) -> f64 {
        let Some(start) = self.clip_start(window) else { return 0.0 };
        ((self.newest_us - start) as f64 / 1e6).min(window as f64)
    }

    /// The keyframe at or before the window, or the first one after the last clip if that's later.
    fn clip_start(&self, seconds: u32) -> Option<i64> {
        let target = self.newest_us - seconds as i64 * 1_000_000;
        let mut start = None;
        for (us, p) in &self.packets {
            if p.track == 0 && p.key && self.cut_us.is_none_or(|cut| *us > cut) {
                if *us <= target || start.is_none() {
                    start = Some(*us);
                } else {
                    break;
                }
            }
        }
        start.filter(|us| self.cut_us.is_none() || self.newest_us - us >= MIN_FOLLOW_UP_US)
    }

    /// The newest `seconds` of everything that isn't already in a clip, starting on a keyframe.
    pub fn snapshot(&self, seconds: u32) -> Vec<Packet> {
        let Some(start) = self.clip_start(seconds) else { return Vec::new() };
        self.packets.iter().filter(|(us, _)| *us >= start).map(|(_, p)| p.clone()).collect()
    }

    /// Everything so far counts as clipped. Returns the old cut, for `restore_cut`.
    pub fn cut(&mut self) -> Option<i64> {
        self.cut_us.replace(self.newest_video_us)
    }

    pub fn restore_cut(&mut self, cut: Option<i64>) {
        self.cut_us = cut;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::{AudioFormat, Codec, VideoFormat, AUDIO_RATE};
    use std::sync::Arc;

    fn tracks() -> Tracks {
        Tracks {
            video: VideoFormat { codec: Codec::H264, width: 16, height: 16, fps: 10, config: vec![] },
            audio: vec![AudioFormat { name: "a".into(), asc: vec![] }],
        }
    }

    fn fill(ring: &mut Ring, seconds: i64) {
        let t = tracks();
        for frame in 0..seconds * 10 {
            let key = frame % 20 == 0;
            ring.push(Packet { track: 0, pts: frame, key, data: Arc::new(vec![0; 100]) }, &t);
            let pts = frame * AUDIO_RATE as i64 / 10;
            ring.push(Packet { track: 1, pts, key: true, data: Arc::new(vec![0; 10]) }, &t);
        }
    }

    #[test]
    fn clip_starts_on_keyframe_at_or_before_the_window() {
        let mut ring = Ring::new(10);
        fill(&mut ring, 30);
        let clip = ring.snapshot(10);
        let first = &clip[0];
        assert_eq!(first.track, 0);
        assert!(first.key);
        // Newest frame is 29.9 s, so the window starts at 19.9 s and the keyframe before is 18 s.
        assert_eq!(first.pts, 180);
        assert_eq!(clip.iter().filter(|p| p.track == 0).count(), 120);
    }

    #[test]
    fn audio_from_before_the_keyframe_is_left_out() {
        let mut ring = Ring::new(10);
        fill(&mut ring, 30);
        let clip = ring.snapshot(10);
        let first_audio = clip.iter().find(|p| p.track == 1).unwrap();
        assert_eq!(first_audio.pts, 18 * AUDIO_RATE as i64);
    }

    #[test]
    fn eviction_bounds_memory() {
        let mut ring = Ring::new(10);
        fill(&mut ring, 300);
        // 13 s retained at (100 + 10) bytes * 10 per second.
        assert!(ring.bytes() <= 14 * 1100, "{}", ring.bytes());
    }

    #[test]
    fn short_buffer_returns_everything_from_the_first_keyframe() {
        let mut ring = Ring::new(60);
        fill(&mut ring, 5);
        let clip = ring.snapshot(60);
        assert_eq!(clip.iter().filter(|p| p.track == 0).count(), 50);
        assert_eq!(clip[0].pts, 0);
    }

    #[test]
    fn empty_and_keyframeless_rings_give_nothing() {
        let t = tracks();
        let mut ring = Ring::new(10);
        assert!(ring.snapshot(10).is_empty());
        ring.push(Packet { track: 0, pts: 1, key: false, data: Arc::new(vec![]) }, &t);
        assert!(ring.snapshot(10).is_empty());
        assert_eq!(ring.seconds(10), 0.0);
    }

    #[test]
    fn next_clip_starts_after_the_last_one() {
        let t = tracks();
        let mut ring = Ring::new(10);
        fill(&mut ring, 30);
        ring.cut();
        for frame in 300..330 {
            // 300 is the keyframe the sink asks for after a clip.
            ring.push(Packet { track: 0, pts: frame, key: frame % 20 == 0 || frame == 300, data: Arc::new(vec![0; 100]) }, &t);
        }
        let clip = ring.snapshot(10);
        assert_eq!(clip[0].pts, 300);
        assert_eq!(clip.len(), 30);
        assert!((ring.clip_seconds(10) - 2.9).abs() < 1e-9);
        assert!(ring.seconds(10) >= 10.0, "the buffer itself is still full");
    }

    #[test]
    fn a_clip_longer_than_the_window_after_the_last_one_is_a_normal_clip() {
        let mut ring = Ring::new(10);
        fill(&mut ring, 5);
        ring.cut();
        let t = tracks();
        for frame in 50..300 {
            ring.push(Packet { track: 0, pts: frame, key: frame % 20 == 0, data: Arc::new(vec![0; 100]) }, &t);
        }
        // Newest frame is 29.9 s, as in the first test.
        assert_eq!(ring.snapshot(10)[0].pts, 180);
    }

    #[test]
    fn nothing_new_after_a_clip_gives_nothing() {
        let t = tracks();
        let mut ring = Ring::new(10);
        fill(&mut ring, 30);
        ring.cut();
        assert!(ring.snapshot(10).is_empty());
        assert_eq!(ring.clip_seconds(10), 0.0);
        // A double press: a new keyframe, but barely any footage after it.
        ring.push(Packet { track: 0, pts: 300, key: true, data: Arc::new(vec![]) }, &t);
        ring.push(Packet { track: 0, pts: 301, key: false, data: Arc::new(vec![]) }, &t);
        assert!(ring.snapshot(10).is_empty());
    }

    #[test]
    fn audio_ahead_of_video_at_the_cut_does_not_skip_the_next_keyframe() {
        let t = tracks();
        let mut ring = Ring::new(10);
        fill(&mut ring, 30);
        ring.push(Packet { track: 1, pts: 31 * AUDIO_RATE as i64, key: true, data: Arc::new(vec![]) }, &t);
        ring.cut();
        for frame in 300..320 {
            ring.push(Packet { track: 0, pts: frame, key: frame == 300, data: Arc::new(vec![]) }, &t);
        }
        let clip = ring.snapshot(10);
        assert_eq!(clip.iter().find(|p| p.track == 0).unwrap().pts, 300);
    }

    #[test]
    fn restoring_the_cut_gets_the_footage_back() {
        let mut ring = Ring::new(10);
        fill(&mut ring, 30);
        let before = ring.snapshot(10).len();
        let old = ring.cut();
        ring.restore_cut(old);
        assert_eq!(ring.snapshot(10).len(), before);
    }

    #[test]
    fn clearing_forgets_the_cut() {
        let t = tracks();
        let mut ring = Ring::new(10);
        fill(&mut ring, 30);
        ring.cut();
        ring.clear();
        ring.push(Packet { track: 0, pts: 1, key: true, data: Arc::new(vec![]) }, &t);
        assert_eq!(ring.snapshot(10).len(), 1);
    }

    #[test]
    fn shortening_the_replay_frees_memory_now() {
        let mut ring = Ring::new(60);
        fill(&mut ring, 60);
        let before = ring.bytes();
        ring.set_length(5);
        assert!(ring.bytes() < before / 3);
    }
}
