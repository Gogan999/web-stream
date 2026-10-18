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

/// What the capture thread should encode next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EncodeJob {
    /// When the image being encoded was captured (now, for a repeat).
    pub captured: Instant,
    pub idr: bool,
}

/// The capture thread's encode decisions, kept free of GPU calls so they can
/// be tested.
///
/// Only an explicit keyframe or repair request bypasses pacing. A frame dropped
/// because the consumer's channel was full makes the *next paced* encode an
/// IDR; it never triggers an immediate re-encode, so a stalled consumer
/// can't make the thread spin on the same image.
#[derive(Debug)]
pub(crate) struct Scheduler {
    pacer: Pacer,
    has_image: bool,
    pending: Option<Instant>,
    keyframe_requested: bool,
    /// Frames were invalidated: encode the frame that skips them now, so
    /// the picture moves again without waiting for a frame slot.
    repair_requested: bool,
    idr_next: bool,
}

impl Scheduler {
    pub(crate) fn new(fps: u32) -> Self {
        Self {
            pacer: Pacer::new(fps),
            has_image: false,
            pending: None,
            keyframe_requested: false,
            repair_requested: false,
            idr_next: true,
        }
    }

    /// The client asked for a keyframe: encode one as soon as there is an
    /// image, ignoring the frame-rate cap.
    pub(crate) fn request_keyframe(&mut self) {
        self.keyframe_requested = true;
    }

    /// Frames the browser never got were invalidated: encode the next frame
    /// (not a keyframe) as soon as there is an image, ignoring the
    /// frame-rate cap.
    pub(crate) fn request_repair(&mut self) {
        self.repair_requested = true;
    }

    /// A new desktop image was converted and is waiting to be encoded.
    pub(crate) fn new_image(&mut self, captured: Instant) {
        self.has_image = true;
        self.pending = Some(captured);
    }

    /// The image source changed (display switch, encoder rebuilt): wait for
    /// a fresh image and start it with an IDR.
    pub(crate) fn reset(&mut self) {
        self.has_image = false;
        self.pending = None;
        self.idr_next = true;
    }

    pub(crate) fn poll(&mut self, now: Instant) -> Option<EncodeJob> {
        if !self.has_image {
            return None;
        }
        let due = match self.pending {
            Some(_) => self.pacer.ready(now),
            None => self.pacer.idle_repeat_due(now),
        };
        if !due && !self.keyframe_requested && !self.repair_requested {
            return None;
        }
        Some(EncodeJob {
            captured: self.pending.take().unwrap_or(now),
            idr: self.keyframe_requested || self.idr_next,
        })
    }

    /// Records the outcome of `job`. `delivered` is false when the frame was
    /// dropped because the consumer's channel was full.
    pub(crate) fn encoded(&mut self, now: Instant, job: EncodeJob, delivered: bool) {
        self.pacer.encoded(now);
        self.repair_requested = false;
        if job.idr {
            self.keyframe_requested = false;
            self.idr_next = false;
        }
        if !delivered {
            self.idr_next = true;
        }
    }

    /// How long to wait for the next desktop update.
    pub(crate) fn wait(&self, now: Instant) -> Duration {
        if self.has_image && (self.keyframe_requested || self.repair_requested) {
            return Duration::ZERO;
        }
        self.pacer.wait(now, self.pending.is_some())
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

#[cfg(test)]
mod scheduler_tests {
    use super::*;

    #[test]
    fn first_frame_is_idr() {
        let t0 = Instant::now();
        let mut s = Scheduler::new(60);
        assert_eq!(s.poll(t0), None);
        s.new_image(t0);
        let job = s.poll(t0).unwrap();
        assert!(job.idr);
        s.encoded(t0, job, true);
        s.new_image(t0 + Duration::from_millis(17));
        assert!(!s.poll(t0 + Duration::from_millis(17)).unwrap().idr);
    }

    #[test]
    fn full_channel_does_not_spin() {
        let t0 = Instant::now();
        let mut s = Scheduler::new(60);
        s.new_image(t0);
        let job = s.poll(t0).unwrap();
        s.encoded(t0, job, false);
        // No new image: nothing to do until the idle repeat, however often
        // we ask.
        for ms in [0, 1, 5, 16, 50, 99] {
            assert_eq!(s.poll(t0 + Duration::from_millis(ms)), None, "at {ms} ms");
        }
        assert!(s.wait(t0) > Duration::ZERO);
        // The next paced encode carries the IDR.
        let later = t0 + Duration::from_millis(20);
        s.new_image(later);
        let job = s.poll(later).unwrap();
        assert!(job.idr);
        s.encoded(later, job, true);
        let after = later + Duration::from_millis(17);
        s.new_image(after);
        assert!(!s.poll(after).unwrap().idr);
    }

    #[test]
    fn keyframe_request_bypasses_pacing_once() {
        let t0 = Instant::now();
        let mut s = Scheduler::new(60);
        s.new_image(t0);
        let job = s.poll(t0).unwrap();
        s.encoded(t0, job, true);
        let t1 = t0 + Duration::from_millis(2);
        assert_eq!(s.poll(t1), None);
        s.request_keyframe();
        assert_eq!(s.wait(t1), Duration::ZERO);
        let job = s.poll(t1).unwrap();
        assert!(job.idr);
        s.encoded(t1, job, true);
        assert_eq!(s.poll(t1), None);
    }

    #[test]
    fn repair_request_bypasses_pacing_once_without_an_idr() {
        let t0 = Instant::now();
        let mut s = Scheduler::new(60);
        s.new_image(t0);
        let job = s.poll(t0).unwrap();
        s.encoded(t0, job, true);
        let t1 = t0 + Duration::from_millis(2);
        assert_eq!(s.poll(t1), None);
        s.request_repair();
        assert_eq!(s.wait(t1), Duration::ZERO);
        let job = s.poll(t1).unwrap();
        assert!(!job.idr);
        s.encoded(t1, job, true);
        assert_eq!(s.poll(t1), None);
    }

    #[test]
    fn reset_waits_for_new_image_then_idr() {
        let t0 = Instant::now();
        let mut s = Scheduler::new(60);
        s.new_image(t0);
        let job = s.poll(t0).unwrap();
        s.encoded(t0, job, true);
        s.reset();
        assert_eq!(s.poll(t0 + Duration::from_secs(1)), None);
        let t1 = t0 + Duration::from_secs(1);
        s.new_image(t1);
        assert!(s.poll(t1).unwrap().idr);
    }
}
