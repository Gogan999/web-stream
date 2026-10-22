//! Decides when a captured desktop image gets encoded.
//!
//! Desktop Duplication hands us a frame whenever the desktop changes, which
//! can be faster than the stream frame rate (a 144 Hz monitor streamed at
//! 60 fps) or never (a static desktop). The pacer caps the encode rate with
//! a deadline that advances by one frame interval per encode, so the average
//! rate matches the target without bursts (a source barely faster than the
//! target passes through, see [`NEAR_TARGET`]), and re-encodes the last
//! image at a low rate while the desktop is idle so the stream stays alive
//! and quality converges.

use std::time::{Duration, Instant};

/// A source whose frames come at least this fraction of the target interval
/// apart (one up to about 5% faster than the target) is not capped: each of
/// its frames is encoded as it arrives. A display's real refresh rate is
/// rarely exactly its nominal one, and one running at 60.05 Hz, streamed at
/// 60 fps, gained on a deadline at exactly 60 until every frame was held for
/// most of a frame time before it was encoded (up to 16 ms of latency), with
/// a frame skipped each time the drift wrapped round. Sources well above the
/// target (120 or 240 Hz at 60 fps) keep the exact target rate.
const NEAR_TARGET: f64 = 0.95;
/// Weight of the newest interval in the source interval estimate.
const SOURCE_SMOOTHING: f64 = 1.0 / 8.0;

#[derive(Debug)]
pub(crate) struct Pacer {
    /// One frame at the target rate.
    interval: Duration,
    /// A new frame up to this early is encoded immediately instead of held,
    /// absorbing jitter between the display's refresh and our deadline.
    slack: Duration,
    idle_repeat: Duration,
    next_due: Option<Instant>,
    last_encode: Option<Instant>,
    /// When the source last delivered a frame, and the smoothed time between
    /// its frames.
    last_source: Option<Instant>,
    source_interval: Option<Duration>,
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
            last_source: None,
            source_interval: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn interval(&self) -> Duration {
        self.interval
    }

    /// The source delivered a frame at `at`.
    pub(crate) fn source_frame(&mut self, at: Instant) {
        if let Some(last) = self.last_source.replace(at) {
            // A pause (a still desktop) says nothing about the source's rate.
            let gap = at.saturating_duration_since(last);
            if gap <= self.interval * 2 {
                self.source_interval = Some(match self.source_interval {
                    Some(avg) => {
                        avg.mul_f64(1.0 - SOURCE_SMOOTHING) + gap.mul_f64(SOURCE_SMOOTHING)
                    }
                    None => gap,
                });
            }
        }
    }

    pub(crate) fn forget_source(&mut self) {
        self.last_source = None;
        self.source_interval = None;
    }

    /// How far apart encodes are kept: the target interval, or just under
    /// the source's own when that is close to it.
    fn pace(&self) -> Duration {
        match self.source_interval {
            Some(src) if src >= self.interval.mul_f64(NEAR_TARGET) => {
                self.interval.min(src.mul_f64(0.99))
            }
            _ => self.interval,
        }
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
        self.next_due = Some(base + self.pace());
        self.last_encode = Some(now);
    }
}

/// Send queue age past which a due frame is held back instead of encoded:
/// under half the transport's late cutoff (50 ms), so a frame encoded now
/// still goes out in time, and well above the zero a healthy link shows.
pub(crate) const HOLD_BACKLOG: Duration = Duration::from_millis(20);
/// Longest a frame is held back while the send queue stays behind. Past
/// this one frame is encoded anyway, so a queue that never drains still
/// gets a picture now and then (and the transport's late drop decides).
pub(crate) const MAX_HOLD: Duration = Duration::from_millis(100);

/// Holds frames back while the transport's send queue is behind, so the
/// encoder sends fewer frames rather than frames that would be dropped as
/// late. A late drop costs more than a skipped encode: the encoder has to
/// repair it with a frame built on an older reference (several times the
/// bits), and if the repair is late too, with a keyframe, which backs the
/// queue up further. Skipping before the encoder leaves no gap to repair.
#[derive(Debug, Default)]
pub(crate) struct Backpressure {
    /// When the current hold began.
    since: Option<Instant>,
}

impl Backpressure {
    /// Whether a frame due now should wait, given the send queue's age.
    pub(crate) fn hold(&mut self, now: Instant, backlog: Duration) -> bool {
        if backlog <= HOLD_BACKLOG {
            self.since = None;
            return false;
        }
        let since = *self.since.get_or_insert(now);
        if now.saturating_duration_since(since) < MAX_HOLD {
            return true;
        }
        // Let one through and start a new hold.
        self.since = Some(now);
        false
    }

    /// When the current hold began, if one is on.
    pub(crate) fn holding_since(&self) -> Option<Instant> {
        self.since
    }
}

/// Least time between two bitrate changes to the encoder. The bandwidth
/// estimate moves several times a second, and every change is an
/// `nvEncReconfigureEncoder` on the capture thread (342 in one logged run).
pub(crate) const BITRATE_MIN_GAP: Duration = Duration::from_millis(250);
/// A cut at least this deep goes through at once: an encoder above the send
/// rate backs the queue up, and frames then go out late.
pub(crate) const BITRATE_URGENT_CUT: f64 = 0.85;

/// Coalesces bitrate changes so the encoder is reconfigured at most every
/// [`BITRATE_MIN_GAP`], except for a deep cut. Only the newest request
/// counts.
#[derive(Debug)]
pub(crate) struct BitrateGate {
    current: u32,
    pending: Option<u32>,
    last_change: Option<Instant>,
}

impl BitrateGate {
    pub(crate) fn new(kbps: u32) -> Self {
        Self {
            current: kbps,
            pending: None,
            last_change: None,
        }
    }

    pub(crate) fn request(&mut self, kbps: u32) {
        self.pending = (kbps != self.current).then_some(kbps);
    }

    /// The bitrate to change to with the next frame, if one is due.
    pub(crate) fn take_due(&mut self, now: Instant) -> Option<u32> {
        let kbps = self.pending?;
        let urgent = f64::from(kbps) <= f64::from(self.current) * BITRATE_URGENT_CUT;
        let rested = self
            .last_change
            .is_none_or(|t| now.saturating_duration_since(t) >= BITRATE_MIN_GAP);
        (urgent || rested).then(|| self.pending.take()).flatten()
    }

    /// Any waiting change, due or not: the encoder is being reconfigured
    /// anyway.
    pub(crate) fn take_now(&mut self) -> Option<u32> {
        self.pending.take()
    }

    /// The encoder now runs at `kbps`.
    pub(crate) fn applied(&mut self, now: Instant, kbps: u32) {
        self.current = kbps;
        self.last_change = Some(now);
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

    /// Why the next encode will be an IDR, if it will (diagnostics).
    pub(crate) fn idr_cause(&self) -> Option<&'static str> {
        if self.keyframe_requested {
            Some("requested")
        } else if self.idr_next {
            Some("start_or_handoff_drop")
        } else {
            None
        }
    }

    /// Whether the next encode is a repair after invalidated frames
    /// (diagnostics).
    pub(crate) fn repair_requested(&self) -> bool {
        self.repair_requested
    }

    /// A new desktop image was converted and is waiting to be encoded.
    pub(crate) fn new_image(&mut self, captured: Instant) {
        self.pacer.source_frame(captured);
        self.has_image = true;
        self.pending = Some(captured);
    }

    /// The image source changed (display switch, encoder rebuilt): wait for
    /// a fresh image and start it with an IDR.
    pub(crate) fn reset(&mut self) {
        self.has_image = false;
        self.pending = None;
        self.idr_next = true;
        // Another display may refresh at another rate.
        self.pacer.forget_source();
    }

    /// Whether [`Self::poll`] would return a job now.
    pub(crate) fn due(&self, now: Instant) -> bool {
        if !self.has_image {
            return false;
        }
        let paced = match self.pending {
            Some(_) => self.pacer.ready(now),
            None => self.pacer.idle_repeat_due(now),
        };
        paced || self.keyframe_requested || self.repair_requested
    }

    pub(crate) fn poll(&mut self, now: Instant) -> Option<EncodeJob> {
        if !self.due(now) {
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

    struct Run {
        encodes: u32,
        mean_hold: Duration,
        max_hold: Duration,
    }

    /// Feeds frames at `source_hz` for `secs`, encoding when the pacer
    /// allows (a newer frame replaces a held one), and reports how often it
    /// encoded and how long encoded frames were held.
    fn simulate(source_hz: f64, target_fps: u32, secs: u64) -> Run {
        let start = Instant::now();
        let mut pacer = Pacer::new(target_fps);
        let step = Duration::from_micros(50);
        let source_interval = Duration::from_secs_f64(1.0 / source_hz);
        let mut next_source = start;
        let mut held: Option<Instant> = None;
        let (mut encodes, mut total, mut max_hold) = (0, Duration::ZERO, Duration::ZERO);
        let mut now = start;
        while now < start + Duration::from_secs(secs) {
            if now >= next_source {
                next_source += source_interval;
                pacer.source_frame(now);
                held = Some(now);
            }
            if let Some(arrived) = held {
                if pacer.ready(now) {
                    pacer.encoded(now);
                    encodes += 1;
                    total += now - arrived;
                    max_hold = max_hold.max(now - arrived);
                    held = None;
                }
            }
            now += step;
        }
        Run {
            encodes,
            mean_hold: total / encodes.max(1),
            max_hold,
        }
    }

    fn run(source_hz: u32, target_fps: u32) -> u32 {
        simulate(f64::from(source_hz), target_fps, 1).encodes
    }

    #[test]
    fn a_display_a_hair_faster_than_the_target_is_not_held() {
        // A "60 Hz" monitor that really runs at 60.05 Hz, streamed at 60
        // fps: frames must go out as they arrive, not drift into a hold
        // of up to most of a frame time.
        for (hz, fps) in [
            (60.0, 60),
            (60.05, 60),
            (60.5, 60),
            (62.0, 60),
            (144.2, 144),
        ] {
            let r = simulate(hz, fps, 30);
            assert!(
                r.max_hold < Duration::from_millis(1),
                "{hz} Hz at {fps} fps: held {:?}",
                r.max_hold
            );
        }
    }

    #[test]
    fn a_much_faster_display_keeps_the_target_rate() {
        // Holds no longer than a plain deadline at exactly the target rate
        // gives (measured with this simulation before sources near the
        // target were let through).
        for (hz, mean_ms, max_ms) in [(120.0, 4.2, 4.25), (144.0, 2.8, 5.6), (240.0, 1.4, 4.2)] {
            let r = simulate(hz, 60, 30);
            let fps = f64::from(r.encodes) / 30.0;
            assert!((59.5..=60.5).contains(&fps), "{hz} Hz: {fps} fps");
            assert!(
                r.mean_hold.as_secs_f64() * 1000.0 <= mean_ms
                    && r.max_hold.as_secs_f64() * 1000.0 <= max_ms,
                "{hz} Hz: mean hold {:?}, max {:?}",
                r.mean_hold,
                r.max_hold
            );
        }
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
    fn backpressure_holds_only_while_the_queue_is_behind() {
        let t = Instant::now();
        let mut b = Backpressure::default();
        let behind = HOLD_BACKLOG + Duration::from_millis(1);
        assert!(!b.hold(t, Duration::ZERO));
        assert!(!b.hold(t, HOLD_BACKLOG));
        assert!(b.hold(t, behind));
        assert_eq!(b.holding_since(), Some(t));
        // Caught up: encode at once.
        assert!(!b.hold(t + Duration::from_millis(5), Duration::ZERO));
        assert_eq!(b.holding_since(), None);
    }

    #[test]
    fn backpressure_lets_a_frame_through_every_max_hold() {
        let t = Instant::now();
        let mut b = Backpressure::default();
        let behind = Duration::from_secs(1);
        let mut passed = Vec::new();
        let mut now = t;
        while now < t + MAX_HOLD * 3 + Duration::from_millis(1) {
            if !b.hold(now, behind) {
                passed.push(now - t);
            }
            now += Duration::from_millis(1);
        }
        assert_eq!(passed, [MAX_HOLD, MAX_HOLD * 2, MAX_HOLD * 3]);
    }

    #[test]
    fn due_matches_poll() {
        let t = Instant::now();
        let mut s = Scheduler::new(60);
        assert!(!s.due(t));
        s.new_image(t);
        assert!(s.due(t));
        let job = s.poll(t).unwrap();
        s.encoded(t, job, true);
        s.new_image(t);
        assert!(!s.due(t));
        assert!(s.poll(t).is_none());
        s.request_repair();
        assert!(s.due(t));
    }

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

    #[test]
    fn bitrate_changes_are_spaced_but_deep_cuts_are_not() {
        let t0 = Instant::now();
        let mut g = BitrateGate::new(10_000);
        g.request(11_000);
        assert_eq!(g.take_due(t0), Some(11_000));
        g.applied(t0, 11_000);
        // Too soon, and only the newest request counts.
        g.request(12_000);
        g.request(12_500);
        assert_eq!(g.take_due(t0 + Duration::from_millis(100)), None);
        assert_eq!(g.take_due(t0 + BITRATE_MIN_GAP), Some(12_500));
        g.applied(t0 + BITRATE_MIN_GAP, 12_500);
        // A deep cut goes at once.
        let t1 = t0 + BITRATE_MIN_GAP + Duration::from_millis(10);
        g.request(9_000);
        assert_eq!(g.take_due(t1), Some(9_000));
        g.applied(t1, 9_000);
        // Asking for what it already runs at is nothing to do.
        g.request(9_000);
        assert_eq!(g.take_due(t1 + Duration::from_secs(1)), None);
        // A keyframe takes whatever waits.
        g.request(9_500);
        assert_eq!(g.take_now(), Some(9_500));
    }
}
