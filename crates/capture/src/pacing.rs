//! Decides when a captured desktop image gets encoded.
//!
//! Desktop Duplication hands us a frame whenever the desktop changes, which
//! can be faster than the stream frame rate (a 144 Hz monitor streamed at
//! 60 fps) or never (a static desktop). The pacer caps the encode rate with
//! a deadline that advances by one frame interval per encode, so the average
//! rate matches the target without bursts, and re-encodes the last image at
//! a low rate while the desktop is idle so the stream stays alive and quality
//! converges.

use std::time::{Duration, Instant};

#[derive(Debug)]
pub(crate) struct Pacer {
    interval: Duration,
    /// A new frame up to this early is encoded immediately instead of held,
    /// absorbing jitter between the display's refresh and our deadline.
    slack: Duration,
    idle_repeat: Duration,
    next_due: Option<Instant>,
    last_encode: Option<Instant>,
}

impl Pacer {
    pub(crate) fn new(fps: u32) -> Self {
        let interval = Duration::from_secs(1) / fps.max(1);
        Self {
            interval,
            slack: interval / 4,
            idle_repeat: Duration::from_millis(100).max(interval),
            next_due: None,
            last_encode: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn interval(&self) -> Duration {
        self.interval
    }

    /// Whether a freshly captured frame may be encoded now; if not it should
    /// be held and encoded once [`Self::wait`] elapses.
    pub(crate) fn ready(&self, now: Instant) -> bool {
        self.next_due.is_none_or(|due| now + self.slack >= due)
    }

    /// Whether the last image should be re-encoded because nothing changed
    /// for a while.
    pub(crate) fn idle_repeat_due(&self, now: Instant) -> bool {
        self.last_encode
            .is_some_and(|t| now.saturating_duration_since(t) >= self.idle_repeat)
    }

    /// How long to wait for the next desktop update. Never longer than one
    /// frame interval so control commands are handled promptly.
    pub(crate) fn wait(&self, now: Instant, holding: bool) -> Duration {
        if holding {
            return self
                .next_due
                .map_or(Duration::ZERO, |due| due.saturating_duration_since(now));
        }
        match self.last_encode {
            Some(t) => (t + self.idle_repeat)
                .saturating_duration_since(now)
                .min(self.interval),
            None => self.interval,
        }
    }

    pub(crate) fn encoded(&mut self, now: Instant) {
        // After an idle period the old deadline is far in the past; restart
        // from now so we don't burst to catch up.
        let base = match self.next_due {
            Some(due) => due.max(now.checked_sub(self.slack).unwrap_or(now)),
            None => now,
        };
        self.next_due = Some(base + self.interval);
        self.last_encode = Some(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds frames at `source_hz` for one second, encoding when the pacer
    /// allows (holding otherwise), and returns the encode count.
    fn run(source_hz: u32, target_fps: u32) -> u32 {
        let start = Instant::now();
        let mut pacer = Pacer::new(target_fps);
        let mut encodes = 0;
        let mut holding = false;
        let step = Duration::from_micros(100);
        let source_interval = Duration::from_secs(1) / source_hz;
        let mut next_source = start;
        let mut now = start;
        while now < start + Duration::from_secs(1) {
            if now >= next_source {
                next_source += source_interval;
                holding = true;
            }
            if holding && pacer.ready(now) {
                pacer.encoded(now);
                holding = false;
                encodes += 1;
            }
            now += step;
        }
        encodes
    }

    #[test]
    fn caps_fast_source_to_target_rate() {
        let n = run(144, 60);
        assert!((59..=61).contains(&n), "{n} encodes");
    }

    #[test]
    fn passes_slow_source_through() {
        assert_eq!(run(30, 60), 30);
    }

    #[test]
    fn matching_rates_encode_every_frame() {
        let n = run(60, 60);
        assert!((59..=61).contains(&n), "{n} encodes");
    }

    #[test]
    fn idle_repeat_after_quiet_period() {
        let t0 = Instant::now();
        let mut pacer = Pacer::new(60);
        assert!(!pacer.idle_repeat_due(t0));
        pacer.encoded(t0);
        assert!(!pacer.idle_repeat_due(t0 + Duration::from_millis(50)));
        assert!(pacer.idle_repeat_due(t0 + Duration::from_millis(100)));
        assert!(pacer.wait(t0, false) <= pacer.interval());
    }

    #[test]
    fn no_burst_after_idle() {
        let t0 = Instant::now();
        let mut pacer = Pacer::new(60);
        pacer.encoded(t0);
        let later = t0 + Duration::from_secs(5);
        assert!(pacer.ready(later));
        pacer.encoded(later);
        assert!(!pacer.ready(later + Duration::from_millis(5)));
    }
}
