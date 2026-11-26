//! Decides when a captured desktop image gets encoded.

use std::time::{Duration, Instant};

/// A source whose frames come at least this fraction of the target interval apart is not capped:
/// each of its frames is encoded as it arrives.
const NEAR_TARGET: f64 = 0.95;
/// Weight of the newest interval in the source interval estimate.
const SOURCE_SMOOTHING: f64 = 1.0 / 8.0;

#[derive(Debug)]
pub(crate) struct Pacer {
    /// One frame at the target rate.
    interval: Duration,
    /// A new frame up to this early is encoded immediately instead of held, absorbing jitter
    /// between the display's refresh and our deadline.
    slack: Duration,
    idle_repeat: Duration,
    next_due: Option<Instant>,
    last_encode: Option<Instant>,
    /// When the source last delivered a frame, and the smoothed time between its frames.
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

    /// How far apart encodes are kept: the target interval, or just under the source's own when
    /// that is close to it.
    fn pace(&self) -> Duration {
        match self.source_interval {
            Some(src) if src >= self.interval.mul_f64(NEAR_TARGET) => {
                self.interval.min(src.mul_f64(0.99))
            }
            _ => self.interval,
        }
    }

    /// Whether a freshly captured frame may be encoded now; if not it should be held and encoded
    /// once [`Self::wait`] elapses.
    pub(crate) fn ready(&self, now: Instant) -> bool {
        self.next_due.is_none_or(|due| now + self.slack >= due)
    }

    /// Whether the last image should be re-encoded.
    pub(crate) fn idle_repeat_due(&self, now: Instant) -> bool {
        self.last_encode
            .is_some_and(|t| now.saturating_duration_since(t) >= self.idle_repeat)
    }

    /// How long to wait for the next desktop update.
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
        let base = match self.next_due {
            Some(due) => due.max(now.checked_sub(self.slack).unwrap_or(now)),
            None => now,
        };
        self.next_due = Some(base + self.pace());
        self.last_encode = Some(now);
    }
}

/// Send queue age past which a due frame is held back instead of encoded: under half the
/// transport's late cutoff (50 ms), so a frame encoded now still goes out in time, and well above
/// the zero a healthy link shows.
pub(crate) const HOLD_BACKLOG: Duration = Duration::from_millis(20);
/// Longest a frame is held back while the send queue stays behind.
pub(crate) const MAX_HOLD: Duration = Duration::from_millis(100);

/// Holds frames back while the transport's send queue is behind.
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

/// Least time between two bitrate changes to the encoder.
pub(crate) const BITRATE_MIN_GAP: Duration = Duration::from_millis(250);
/// A cut at least this deep goes through at once: an encoder above the send rate backs the queue
/// up, and frames then go out late.
pub(crate) const BITRATE_URGENT_CUT: f64 = 0.85;

/// Coalesces bitrate changes.
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

    /// Any waiting change, due or not: the encoder is being reconfigured anyway.
    pub(crate) fn take_now(&mut self) -> Option<u32> {
        self.pending.take()
    }

    /// The encoder now runs at `kbps`.
    pub(crate) fn applied(&mut self, now: Instant, kbps: u32) {
        self.current = kbps;
        self.last_change = Some(now);
    }
}

/// Most that may be owed, in time at the target bitrate, when the next frame starts: one frame's
/// burst, as the other qualities' VBV allows.
const BUDGET_SLACK: Duration = Duration::from_millis(20);

/// Spends the bitrate by delaying frames, for the Source quality, where every frame costs what the
/// picture needs and the frame rate gives.
#[derive(Debug)]
pub(crate) struct RateBudget {
    bits_per_sec: f64,
    /// Bits owed as of `at`.
    owed: f64,
    at: Option<Instant>,
    in_flight: u32,
    /// When the newest frame in the encoder went in.
    submitted_at: Option<Instant>,
}

/// A frame still in the encoder after this long is taken as lost, so a missing completion can't
/// stall the stream.
const IN_FLIGHT_MAX: Duration = Duration::from_millis(250);

impl RateBudget {
    pub(crate) fn new(kbps: u32) -> Self {
        Self {
            bits_per_sec: f64::from(kbps.max(1)) * 1000.0,
            owed: 0.0,
            at: None,
            in_flight: 0,
            submitted_at: None,
        }
    }

    pub(crate) fn set_rate(&mut self, kbps: u32) {
        self.bits_per_sec = f64::from(kbps.max(1)) * 1000.0;
    }

    fn owed_at(&self, now: Instant) -> f64 {
        let drained = self
            .at
            .map_or(0.0, |at| now.saturating_duration_since(at).as_secs_f64())
            * self.bits_per_sec;
        (self.owed - drained).max(0.0)
    }

    /// Time until the next frame may start; zero if it may now.
    pub(crate) fn wait(&self, now: Instant) -> Duration {
        let waiting = self.in_flight > 0
            && self
                .submitted_at
                .is_some_and(|t| now.saturating_duration_since(t) < IN_FLIGHT_MAX);
        if waiting {
            return Duration::from_millis(1);
        }
        let over = self.owed_at(now) - BUDGET_SLACK.as_secs_f64() * self.bits_per_sec;
        if over <= 0.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(over / self.bits_per_sec)
        }
    }

    pub(crate) fn ready(&self, now: Instant) -> bool {
        self.wait(now).is_zero()
    }

    /// A frame went into the encoder.
    pub(crate) fn submitted(&mut self, now: Instant) {
        self.in_flight += 1;
        self.submitted_at = Some(now);
    }

    /// A frame came out of the encoder at `now` and is `bytes` long.
    pub(crate) fn completed(&mut self, now: Instant, bytes: usize) {
        self.in_flight = self.in_flight.saturating_sub(1);
        self.owed = self.owed_at(now) + bytes as f64 * 8.0;
        self.at = Some(now);
    }

    /// The encoder was rebuilt: nothing is in it any more.
    pub(crate) fn reset(&mut self) {
        self.in_flight = 0;
    }
}

/// What the capture thread should encode next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EncodeJob {
    /// When the image being encoded was captured.
    pub captured: Instant,
    pub idr: bool,
}

/// The capture thread's encode decisions, kept free of GPU calls so they can be tested.
#[derive(Debug)]
pub(crate) struct Scheduler {
    pacer: Pacer,
    /// Set for the Source quality.
    budget: Option<RateBudget>,
    has_image: bool,
    pending: Option<Instant>,
    keyframe_requested: bool,
    /// Frames were invalidated: encode the frame that skips them now.
    repair_requested: bool,
    idr_next: bool,
}

impl Scheduler {
    pub(crate) fn new(fps: u32) -> Self {
        Self {
            pacer: Pacer::new(fps),
            budget: None,
            has_image: false,
            pending: None,
            keyframe_requested: false,
            repair_requested: false,
            idr_next: true,
        }
    }

    /// Spends `kbps` by delaying frames instead of leaving it to the encoder (see [`RateBudget`]).
    pub(crate) fn pace_by_bitrate(mut self, kbps: u32) -> Self {
        self.budget = Some(RateBudget::new(kbps));
        self
    }

    /// The bitrate to spend changed.
    pub(crate) fn set_bitrate(&mut self, kbps: u32) {
        if let Some(b) = &mut self.budget {
            b.set_rate(kbps);
        }
    }

    /// A frame left the encoder at `now`, `bytes` long (zero if lost).
    pub(crate) fn frame_done(&mut self, now: Instant, bytes: usize) {
        if let Some(b) = &mut self.budget {
            b.completed(now, bytes);
        }
    }

    /// The client asked for a keyframe: encode one as soon as there is an image, ignoring the
    /// frame-rate cap.
    pub(crate) fn request_keyframe(&mut self) {
        self.keyframe_requested = true;
    }

    /// Frames the browser never got were invalidated: encode the next frame (not a keyframe) as
    /// soon as there is an image, ignoring the frame-rate cap.
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

    /// Whether the next encode is a repair after invalidated frames (diagnostics).
    pub(crate) fn repair_requested(&self) -> bool {
        self.repair_requested
    }

    /// A new desktop image was converted and is waiting to be encoded.
    pub(crate) fn new_image(&mut self, captured: Instant) {
        self.pacer.source_frame(captured);
        self.has_image = true;
        self.pending = Some(captured);
    }

    /// The image source changed: wait for a fresh image and start it with an IDR.
    pub(crate) fn reset(&mut self) {
        self.has_image = false;
        self.pending = None;
        self.idr_next = true;
        self.pacer.forget_source();
        if let Some(b) = &mut self.budget {
            b.reset();
        }
    }

    /// Whether [`Self::poll`] would return a job now.
    pub(crate) fn due(&self, now: Instant) -> bool {
        if !self.has_image {
            return false;
        }
        let paced = match self.pending {
            Some(_) => self.pacer.ready(now),
            None => self.pacer.idle_repeat_due(now),
        } && self.budget.as_ref().is_none_or(|b| b.ready(now));
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

    /// A frame already encoded was dropped and can't be skipped: the next paced encode is an IDR.
    pub(crate) fn dropped(&mut self) {
        self.idr_next = true;
    }

    /// Records the outcome of `job`.
    pub(crate) fn encoded(&mut self, now: Instant, job: EncodeJob, delivered: bool) {
        self.pacer.encoded(now);
        if let Some(b) = &mut self.budget {
            b.submitted(now);
        }
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
        let wait = self.pacer.wait(now, self.pending.is_some());
        match &self.budget {
            Some(b) if !b.ready(now) => wait.max(b.wait(now)).max(Duration::from_millis(1)),
            _ => wait,
        }
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

    /// Feeds frames at `source_hz` for `secs`, encoding when the pacer allows, and reports how
    /// often it encoded and how long encoded frames were held.
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
    fn a_lost_frame_makes_the_next_encode_an_idr() {
        let t0 = Instant::now();
        let mut s = Scheduler::new(60);
        s.new_image(t0);
        let job = s.poll(t0).unwrap();
        s.encoded(t0, job, true);
        s.dropped();
        let later = t0 + Duration::from_millis(17);
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
        g.request(12_000);
        g.request(12_500);
        assert_eq!(g.take_due(t0 + Duration::from_millis(100)), None);
        assert_eq!(g.take_due(t0 + BITRATE_MIN_GAP), Some(12_500));
        g.applied(t0 + BITRATE_MIN_GAP, 12_500);
        let t1 = t0 + BITRATE_MIN_GAP + Duration::from_millis(10);
        g.request(9_000);
        assert_eq!(g.take_due(t1), Some(9_000));
        g.applied(t1, 9_000);
        g.request(9_000);
        assert_eq!(g.take_due(t1 + Duration::from_secs(1)), None);
        g.request(9_500);
        assert_eq!(g.take_now(), Some(9_500));
    }

    #[test]
    fn a_cheap_frame_leaves_the_budget_open() {
        let now = Instant::now();
        let mut b = RateBudget::new(20_000);
        assert!(b.ready(now));
        b.submitted(now);
        assert!(!b.ready(now));
        // 2 KB at 20 Mb/s is under a millisecond of the budget.
        b.completed(now + Duration::from_millis(5), 2_000);
        assert!(b.ready(now + Duration::from_millis(5)));
    }

    #[test]
    fn a_costly_frame_holds_the_next_ones_for_what_it_cost() {
        let now = Instant::now();
        let mut b = RateBudget::new(20_000);
        b.submitted(now);
        // 500 KB = 4 Mbit = 200 ms at 20 Mb/s.
        b.completed(now, 500_000);
        let wait = b.wait(now);
        // 200 ms less the 20 ms of slack.
        assert!(
            (wait.as_secs_f64() - 0.180).abs() < 0.001,
            "waited {wait:?}"
        );
        assert!(!b.ready(now + Duration::from_millis(100)));
        assert!(b.ready(now + Duration::from_millis(181)));
    }

    #[test]
    fn the_average_rate_matches_the_bitrate() {
        // Frames of 100 KB (0.8 Mbit) at 20 Mb/s: 25 frames a second.
        let start = Instant::now();
        let mut b = RateBudget::new(20_000);
        let mut now = start;
        let mut frames = 0;
        while now < start + Duration::from_secs(10) {
            if b.ready(now) {
                b.submitted(now);
                b.completed(now, 100_000);
                frames += 1;
            }
            now += Duration::from_millis(1);
        }
        let fps = f64::from(frames) / 10.0;
        assert!((fps - 25.0).abs() < 1.0, "{fps} fps");
    }

    #[test]
    fn a_frame_that_never_comes_out_cannot_stall_the_stream() {
        let now = Instant::now();
        let mut b = RateBudget::new(20_000);
        b.submitted(now);
        assert!(!b.ready(now + Duration::from_millis(100)));
        assert!(b.ready(now + IN_FLIGHT_MAX + Duration::from_millis(1)));
    }

    #[test]
    fn the_source_scheduler_waits_for_the_budget_without_spinning() {
        let now = Instant::now();
        let mut s = Scheduler::new(60).pace_by_bitrate(20_000);
        s.new_image(now);
        let job = s.poll(now).expect("the first frame goes at once");
        s.encoded(now, job, true);
        s.frame_done(now, 500_000);
        s.new_image(now + Duration::from_millis(17));
        let later = now + Duration::from_millis(17);
        assert!(!s.due(later));
        assert!(s.wait(later) >= Duration::from_millis(100));
        assert!(s.due(now + Duration::from_millis(190)));
    }
}
