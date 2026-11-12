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
    /// How long one video frame lasts.
    frame_us: i64,
    /// Start of the newest packet of each audio track, by track number less one.
    newest_audio_us: Vec<i64>,
    /// Where the previous clip ended. The next clip starts at the first keyframe after it.
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
        self.newest_audio_us.clear();
        self.cut_us = None;
    }

    pub fn push(&mut self, packet: Packet, tracks: &Tracks) {
        let us = packet.micros(tracks.timescale(packet.track));
        self.newest_us = self.newest_us.max(us);
        if packet.track == 0 {
            self.newest_video_us = us;
            self.frame_us = 1_000_000 / tracks.video.fps.max(1) as i64;
        } else {
            let i = packet.track as usize - 1;
            if self.newest_audio_us.len() <= i {
                self.newest_audio_us.resize(i + 1, i64::MIN);
            }
            self.newest_audio_us[i] = self.newest_audio_us[i].max(us);
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
        let Some(start) = self.clip_start(window, self.newest_us) else { return 0.0 };
        ((self.newest_us - start) as f64 / 1e6).min(window as f64)
    }

    /// The newest video frame, the last one in a clip saved now.
    pub fn newest_video_us(&self) -> i64 {
        self.newest_video_us
    }

    /// Whether every audio track has caught up with a clip whose last frame is at `end_us`.
    /// Audio comes out of the mixer at least 150 ms behind the video.
    pub fn audio_reached(&self, end_us: i64, tracks: usize) -> bool {
        let sound_end = end_us + self.frame_us;
        (0..tracks).all(|i| self.newest_audio_us.get(i).is_some_and(|us| *us >= sound_end))
    }

    /// The keyframe at or before the window, or the first one after the last clip if that's later.
    fn clip_start(&self, seconds: u32, end_us: i64) -> Option<i64> {
        let target = end_us - seconds as i64 * 1_000_000;
        let mut start = None;
        for (us, p) in &self.packets {
            if *us > end_us {
                break;
            }
            if p.track == 0 && p.key && self.cut_us.is_none_or(|cut| *us > cut) {
                if *us <= target || start.is_none() {
                    start = Some(*us);
                } else {
                    break;
                }
            }
        }
        start.filter(|us| self.cut_us.is_none() || end_us - us >= MIN_FOLLOW_UP_US)
    }

    /// The newest `seconds` of everything that isn't already in a clip, starting on a keyframe.
    pub fn snapshot(&self, seconds: u32) -> Vec<Packet> {
        self.snapshot_until(seconds, self.newest_us)
    }

    /// `snapshot` of the `seconds` up to the frame at `end_us`. Its sound runs on to where that
    /// frame ends, which is where the next clip's first frame (the keyframe asked for on saving)
    /// and sound begin, so back-to-back clips join without a gap.
    pub fn snapshot_until(&self, seconds: u32, end_us: i64) -> Vec<Packet> {
        let Some(start) = self.clip_start(seconds, end_us) else { return Vec::new() };
        let sound_end = end_us + self.frame_us;
        self.packets
            .iter()
            .filter(|(us, p)| *us >= start && if p.track == 0 { *us <= end_us } else { *us < sound_end })
            .map(|(_, p)| p.clone())
            .collect()
    }

    /// Everything so far counts as clipped. Returns the old cut, for `restore_cut`.
    pub fn cut(&mut self) -> Option<i64> {
        self.cut_to(self.newest_video_us)
    }

    /// Everything up to `end_us` counts as clipped. Never moves the cut back, which an earlier
    /// save finishing after a later one would otherwise do. Returns the old cut.
    pub fn cut_to(&mut self, end_us: i64) -> Option<i64> {
        let old = self.cut_us;
        self.cut_us = Some(old.map_or(end_us, |cut| cut.max(end_us)));
        old
    }

    /// Puts back the cut from before a clip that wasn't saved, as long as no other clip has moved
    /// it since: saves run on threads of their own, and undoing a later clip's cut would make the
    /// next clip repeat that one.
    pub fn restore_cut(&mut self, previous: Option<i64>, ours: i64) {
        if self.cut_us == Some(ours) {
            self.cut_us = previous;
        }
    }

    /// Where the last clip ended.
    pub fn cut_us(&self) -> Option<i64> {
        self.cut_us
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
        ring.restore_cut(old, ring.cut_us().unwrap());
        assert_eq!(ring.snapshot(10).len(), before);
    }

    #[test]
    fn a_failed_clip_leaves_a_later_clips_cut_alone() {
        let t = tracks();
        let mut ring = Ring::new(60);
        fill(&mut ring, 10);
        // Clip A ends at 9.9 s and its save is slow.
        let a_previous = ring.cut_to(9_900_000);
        for frame in 100..200 {
            ring.push(Packet { track: 0, pts: frame, key: frame % 20 == 0, data: Arc::new(vec![]) }, &t);
        }
        // Clip B ends at 19.9 s and is saved, then A fails.
        ring.cut_to(19_900_000);
        ring.restore_cut(a_previous, 9_900_000);
        assert_eq!(ring.cut_us(), Some(19_900_000), "the next clip must not repeat B");
        // B failing too puts its own cut back.
        ring.restore_cut(Some(9_900_000), 19_900_000);
        assert_eq!(ring.cut_us(), Some(9_900_000));
    }

    #[test]
    fn an_earlier_end_never_moves_the_cut_back() {
        let mut ring = Ring::new(60);
        fill(&mut ring, 10);
        ring.cut_to(9_000_000);
        assert_eq!(ring.cut_to(5_000_000), Some(9_000_000));
        assert_eq!(ring.cut_us(), Some(9_000_000));
    }

    /// Video to frame 299 (29.9 s); audio only to 29.75 s, as the mixer delivers it.
    fn fill_with_late_audio(ring: &mut Ring) {
        let t = tracks();
        for frame in 0..300 {
            ring.push(Packet { track: 0, pts: frame, key: frame % 20 == 0, data: Arc::new(vec![0; 100]) }, &t);
        }
        for n in 0..(29_750 * AUDIO_RATE as i64 / 1000) / 1024 {
            ring.push(Packet { track: 1, pts: n * 1024, key: true, data: Arc::new(vec![0; 10]) }, &t);
        }
    }

    fn push_audio_until(ring: &mut Ring, until_us: i64) {
        let t = tracks();
        let first = ring.newest_audio_us[0] * AUDIO_RATE as i64 / 1_000_000 / 1024 + 1;
        for n in first.. {
            let p = Packet { track: 1, pts: n * 1024, key: true, data: Arc::new(vec![0; 10]) };
            let us = p.micros(AUDIO_RATE);
            ring.push(p, &t);
            if us >= until_us {
                break;
            }
        }
    }

    #[test]
    fn a_clip_waits_for_audio_to_reach_its_end() {
        let mut ring = Ring::new(10);
        fill_with_late_audio(&mut ring);
        let end = ring.newest_video_us();
        assert_eq!(end, 29_900_000);
        assert!(!ring.audio_reached(end, 1));
        assert!(ring.audio_reached(end, 0), "no audio tracks, nothing to wait for");
        // The last frame lasts 100 ms, and its sound with it.
        push_audio_until(&mut ring, end + 90_000);
        assert!(!ring.audio_reached(end, 1));
        push_audio_until(&mut ring, end + 100_000);
        assert!(ring.audio_reached(end, 1));
    }

    #[test]
    fn back_to_back_clips_keep_the_audio_at_the_join() {
        let t = tracks();
        let mut ring = Ring::new(60);
        fill_with_late_audio(&mut ring);
        let end = ring.newest_video_us();
        // Frames after the save, starting with the keyframe it asks for, arrive while it waits.
        for frame in 300..305 {
            ring.push(Packet { track: 0, pts: frame, key: frame == 300, data: Arc::new(vec![0; 100]) }, &t);
        }
        push_audio_until(&mut ring, end + 100_000);
        assert!(ring.audio_reached(end, 1));
        let first = ring.snapshot_until(60, end);
        ring.cut_to(end);
        assert_eq!(first.iter().filter(|p| p.track == 0).last().unwrap().pts, 299, "no frame after the end");

        for frame in 305..330 {
            ring.push(Packet { track: 0, pts: frame, key: false, data: Arc::new(vec![0; 100]) }, &t);
        }
        push_audio_until(&mut ring, 33_000_000);
        let second = ring.snapshot_until(60, ring.newest_video_us());
        assert_eq!(second[0].pts, 300, "the next clip starts on the keyframe after the end");
        // The sound of the two clips is one unbroken run of packets, ending and starting at 30 s.
        let last = first.iter().filter(|p| p.track == 1).last().unwrap();
        let next = second.iter().find(|p| p.track == 1).unwrap();
        assert_eq!(next.pts, last.pts + 1024);
        assert!(last.micros(AUDIO_RATE) < 30_000_000 && next.micros(AUDIO_RATE) >= 30_000_000);
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
