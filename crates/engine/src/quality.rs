//! Keeps the quality counters for the status: totals since replay or recording started, and what
//! happened in the last few minutes, which is what the app shows. Also says when a summary is due
//! in the log.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use tape_core::ipc::{Quality, QualityCounts};

/// How far back "recent" reaches.
pub const RECENT: Duration = Duration::from_secs(300);
/// How often the counters are sampled for the recent window. Recent counts are this coarse.
const SAMPLE_EVERY: Duration = Duration::from_secs(10);
const LOG_EVERY: Duration = Duration::from_secs(600);

pub struct Meter {
    started: Instant,
    base: QualityCounts,
    /// Totals as they were at each sample, oldest first, none older than `RECENT`.
    samples: VecDeque<(Instant, QualityCounts)>,
    logged: (Instant, QualityCounts),
}

impl Meter {
    pub fn new(now: Instant, totals: QualityCounts) -> Self {
        Meter { started: now, base: totals, samples: VecDeque::from([(now, totals)]), logged: (now, totals) }
    }

    /// Starts counting over, for replay or recording starting from off.
    pub fn reset(&mut self, now: Instant, totals: QualityCounts) {
        *self = Meter::new(now, totals);
    }

    /// `totals` are the engine's counters, which only ever grow.
    pub fn report(&mut self, now: Instant, totals: QualityCounts) -> Quality {
        if self.samples.back().is_none_or(|(at, _)| now.duration_since(*at) >= SAMPLE_EVERY) {
            self.samples.push_back((now, totals));
        }
        // The oldest sample kept is at most `RECENT` old, so the window never reaches further.
        while self.samples.len() > 1 && self.samples.front().is_some_and(|(at, _)| now.duration_since(*at) > RECENT) {
            self.samples.pop_front();
        }
        let (since, earlier) = self.samples.front().copied().unwrap_or((now, totals));
        Quality {
            total: totals.since(&self.base),
            seconds: now.duration_since(self.started).as_secs_f32(),
            recent: totals.since(&earlier),
            recent_seconds: now.duration_since(since).as_secs_f32(),
        }
    }

    /// A line for the log every `LOG_EVERY`, covering the time since the last one.
    pub fn log_line(&mut self, now: Instant, totals: QualityCounts, fps: f32) -> Option<String> {
        let (at, earlier) = self.logged;
        if now.duration_since(at) < LOG_EVERY {
            return None;
        }
        self.logged = (now, totals);
        let c = totals.since(&earlier);
        let minutes = now.duration_since(at).as_secs() / 60;
        Some(format!(
            "last {minutes} min: {} dropped frames, {} late frames, {} game audio and {} mic glitches, {} slow disk, {fps:.1} fps now",
            c.dropped_frames, c.late_frames, c.system_glitches, c.mic_glitches, c.slow_disk
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dropped(n: u64) -> QualityCounts {
        QualityCounts { dropped_frames: n, ..Default::default() }
    }

    #[test]
    fn totals_count_from_the_start_and_recent_forgets() {
        let start = Instant::now();
        let at = |s: u64| start + Duration::from_secs(s);
        // 4 dropped before replay started don't count.
        let mut meter = Meter::new(start, dropped(4));
        let q = meter.report(at(5), dropped(16));
        assert_eq!((q.total.dropped_frames, q.recent.dropped_frames), (12, 12));
        for s in (10..=400).step_by(10) {
            meter.report(at(s), dropped(16));
        }
        let q = meter.report(at(400), dropped(16));
        assert_eq!(q.total.dropped_frames, 12);
        assert!(q.recent.is_clean(), "{:?}", q.recent);
        assert!(q.recent_seconds <= RECENT.as_secs_f32() && q.recent_seconds >= 290.0, "{}", q.recent_seconds);
        let q = meter.report(at(401), dropped(17));
        assert_eq!(q.recent.dropped_frames, 1);
    }

    #[test]
    fn a_reset_starts_over() {
        let start = Instant::now();
        let mut meter = Meter::new(start, dropped(0));
        meter.report(start + Duration::from_secs(20), dropped(9));
        meter.reset(start + Duration::from_secs(21), dropped(9));
        let q = meter.report(start + Duration::from_secs(22), dropped(9));
        assert!(q.total.is_clean() && q.recent.is_clean());
        assert!(q.seconds < 2.0);
    }

    #[test]
    fn the_log_gets_a_summary_every_ten_minutes() {
        let start = Instant::now();
        let mut meter = Meter::new(start, dropped(0));
        assert_eq!(meter.log_line(start + Duration::from_secs(599), dropped(3), 60.0), None);
        let line = meter.log_line(start + LOG_EVERY, dropped(3), 59.94).unwrap();
        assert!(line.starts_with("last 10 min: 3 dropped frames,") && line.ends_with("59.9 fps now"), "{line}");
        assert_eq!(meter.log_line(start + LOG_EVERY + Duration::from_secs(1), dropped(5), 60.0), None);
        let line = meter.log_line(start + LOG_EVERY * 2, dropped(5), 60.0).unwrap();
        assert!(line.starts_with("last 10 min: 2 dropped"), "{line}");
    }
}
