//! Lowers the encode resolution while the bitrate is too low for it.
//!
//! An encoder can only make a frame so small: in a real run over cellular
//! (1 to 3 Mb/s) a still 2560x1440 desktop still came out at about 16 KB a
//! frame, many times its budget, so the send queue backed up whatever the
//! bitrate was set to. Encoding fewer pixels lets the frames fit; the
//! browser scales the picture back up. The step goes down soon after the
//! bitrate falls and back up only once it has stayed well above what the
//! larger size needs, since every change restarts the encoder (a keyframe
//! and a short pause).
//!
//! A low bandwidth estimate alone is not enough to step down. The estimate
//! only grows to about 1.5 times what the stream actually sends, and on a
//! loss or delay spike it drops to 0.85 times that. A still desktop sends
//! almost nothing (a tiny repeat frame ten times a second), so after a
//! minute of it the estimate sits far below the link's real capacity, and
//! stepping down on it shrank the picture again and again with nothing on
//! screen. So a step down also needs the stream to be using its bitrate, or
//! frames to be dropping for lack of it: then the bitrate really is too
//! small for the size.
//!
//! Nor is a short dip. Over Wi-Fi the estimate saws up and down every 5 to
//! 10 seconds even on a healthy link: a probe finds 50 Mb/s or more, a
//! delay spike puts it back to 0.85 times what was sent, and it climbs
//! again (a real run, 2026-10-22: 49 -> 11 -> 57 -> 17 -> 10 -> 49 -> 3 Mb/s,
//! with no frame dropped or packet lost). Stepping down on a 2 second dip
//! shrank the picture for nothing. So a step down needs either frames
//! dropping, or the estimate to have stayed under what the size needs for
//! the whole [`LOOKBACK`].

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Scale factors of the configured size, largest first.
const STEPS: [f64; 4] = [1.0, 0.75, 0.5, 0.375];
/// Never encode fewer lines than this.
const MIN_HEIGHT: u32 = 360;
/// Bits per pixel per frame below which a size is too big for the bitrate.
/// From the run above: about 0.035 was the least the encoder managed at
/// 1440p, so frames under this mostly overrun their budget.
const MIN_BITS_PER_PIXEL: f64 = 0.04;
/// How long the bitrate must stay too low before stepping down.
const DOWN_AFTER: Duration = Duration::from_secs(2);
/// Without dropped frames, a step down also needs the bitrate to have
/// been too low for all of this long: longer than the estimate's sawtooth.
const LOOKBACK: Duration = Duration::from_secs(15);
/// How long the bitrate must stay comfortably above the next size up's
/// need before stepping up.
const UP_AFTER: Duration = Duration::from_secs(10);
/// "Comfortably": this many times what the next size up needs.
const UP_MARGIN: f64 = 1.5;
/// A second in which the stream sent at least this share of the encoder's
/// target counts as using it. A still desktop sends a few percent; moving
/// content near 100%, and more when its frames overrun.
const BUSY_SHARE: f64 = 0.5;
/// A step down this soon after a step up means the larger size didn't hold:
/// wait twice as long before trying it again.
const FLAP_WINDOW: Duration = Duration::from_secs(30);
/// The longest that doubling makes the wait for a step up.
const MAX_UP_AFTER: Duration = Duration::from_secs(80);
/// No step down this soon after a connection starts or the size changes:
/// the bandwidth estimate starts at 10 Mb/s (a 7.5 Mb/s target, under what
/// 1440p60 needs) and takes a moment to climb.
const SETTLE: Duration = Duration::from_secs(5);

/// The size to encode at `step` of `width` x `height`: scaled, rounded down
/// to a multiple of 8 (the encoder needs even sizes).
pub(crate) fn size(width: u32, height: u32, step: usize) -> (u32, u32) {
    let f = STEPS[step.min(STEPS.len() - 1)];
    let round = |v: u32| (((f64::from(v) * f) as u32) / 8 * 8).max(8);
    if step == 0 {
        (width, height)
    } else {
        (round(width), round(height))
    }
}

/// The least bitrate, in kb/s, that `width` x `height` at `fps` holds up at.
pub(crate) fn min_kbps(width: u32, height: u32, fps: u32) -> f64 {
    f64::from(width) * f64::from(height) * f64::from(fps) * MIN_BITS_PER_PIXEL / 1000.0
}

/// The smallest step allowed for a configured size, and for the browser's
/// floor (see [`ws_proto::VideoConfig::min_scale_percent`]).
fn last_step(width: u32, height: u32, min_percent: u8) -> usize {
    let floor = f64::from(min_percent.min(100)) / 100.0;
    (0..STEPS.len())
        .rev()
        .find(|&s| STEPS[s] >= floor && size(width, height, s).1 >= MIN_HEIGHT.min(height))
        .unwrap_or(0)
}

/// What the stream did over the last second.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Load {
    /// The encoder's target, from the bandwidth estimate.
    pub kbps: u32,
    /// What the encoder actually produced.
    pub sent_kbps: u32,
    /// Frames were dropped or refused for lack of bandwidth.
    pub dropped: bool,
}

impl Load {
    /// Whether the stream used its bitrate, so a low estimate is real.
    fn busy(&self) -> bool {
        self.dropped || f64::from(self.sent_kbps) >= f64::from(self.kbps) * BUSY_SHARE
    }
}

#[derive(Debug, Default)]
pub(crate) struct AutoScale {
    step: usize,
    /// The first update since the last reset or size change.
    since_change: Option<Instant>,
    /// Since when the bitrate has been too low for the current step.
    low_since: Option<Instant>,
    /// Frames were dropped since `low_since`.
    dropped_while_low: bool,
    /// The bitrate each second over the last [`LOOKBACK`].
    recent: VecDeque<(Instant, u32)>,
    /// Since when it has been enough for the step above.
    high_since: Option<Instant>,
    /// The last step up, to spot a size that doesn't hold.
    last_up: Option<Instant>,
    /// How long the headroom must last before a step up; [`UP_AFTER`]
    /// until a step up fails.
    up_after: Option<Duration>,
}

impl AutoScale {
    pub(crate) fn step(&self) -> usize {
        self.step
    }

    /// Back to the configured size (a new connection, or the browser chose
    /// another size).
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    /// Steps back up to the browser's floor if the current step is below
    /// it. Returns the new step when the encode size should change.
    pub(crate) fn limit(
        &mut self,
        now: Instant,
        width: u32,
        height: u32,
        min_percent: u8,
    ) -> Option<usize> {
        let last = last_step(width, height, min_percent);
        if self.step <= last {
            return None;
        }
        self.step = last;
        self.changed(now);
        Some(last)
    }

    /// Takes the last second's [`Load`], once a second. Returns the new
    /// step when the encode size should change.
    pub(crate) fn update(
        &mut self,
        now: Instant,
        load: Load,
        width: u32,
        height: u32,
        fps: u32,
        min_percent: u8,
    ) -> Option<usize> {
        if let Some(step) = self.limit(now, width, height, min_percent) {
            return Some(step);
        }
        while self
            .recent
            .front()
            .is_some_and(|&(t, _)| now.saturating_duration_since(t) > LOOKBACK)
        {
            self.recent.pop_front();
        }
        self.recent.push_back((now, load.kbps));
        let kbps = f64::from(load.kbps);
        let need = |step: usize| {
            let (w, h) = size(width, height, step);
            min_kbps(w, h, fps)
        };
        let last = last_step(width, height, min_percent);
        let settled =
            now.saturating_duration_since(*self.since_change.get_or_insert(now)) >= SETTLE;
        if self.step < last && kbps < need(self.step) {
            self.high_since = None;
            // Too low, but the stream isn't using what it has: the estimate
            // says nothing about the link, so wait for a busy stretch.
            if !settled || !load.busy() {
                self.low_since = None;
                self.dropped_while_low = false;
                return None;
            }
            let since = *self.low_since.get_or_insert(now);
            self.dropped_while_low |= load.dropped;
            // The most the bitrate reached lately: a dip that the estimate
            // climbs back out of says nothing about the link.
            let peak = f64::from(self.recent.iter().map(|&(_, k)| k).max().unwrap_or(0));
            let real = self.dropped_while_low || peak < need(self.step);
            if real && now.saturating_duration_since(since) >= DOWN_AFTER {
                // Straight to the largest size the bitrate holds up: the
                // current one if frames are dropping, else the recent best.
                let basis = if self.dropped_while_low { kbps } else { peak };
                self.step = (self.step + 1..=last)
                    .find(|&s| basis >= need(s))
                    .unwrap_or(last);
                if self
                    .last_up
                    .is_some_and(|t| now.saturating_duration_since(t) < FLAP_WINDOW)
                {
                    self.up_after = Some((self.up_after() * 2).min(MAX_UP_AFTER));
                }
                self.changed(now);
                return Some(self.step);
            }
            return None;
        }
        self.low_since = None;
        self.dropped_while_low = false;
        if self.step > 0 && kbps >= need(self.step - 1) * UP_MARGIN {
            let since = *self.high_since.get_or_insert(now);
            if now.saturating_duration_since(since) >= self.up_after() {
                self.step -= 1;
                self.last_up = Some(now);
                self.changed(now);
                return Some(self.step);
            }
        } else {
            self.high_since = None;
        }
        None
    }

    fn up_after(&self) -> Duration {
        self.up_after.unwrap_or(UP_AFTER)
    }

    fn changed(&mut self, now: Instant) {
        self.low_since = None;
        self.dropped_while_low = false;
        self.high_since = None;
        self.since_change = Some(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Duration = Duration::from_secs(1);

    /// A second of moving content using all of `kbps`.
    fn busy(kbps: u32) -> Load {
        Load {
            kbps,
            sent_kbps: kbps,
            dropped: false,
        }
    }

    /// A second of still desktop: a target of `kbps` and almost nothing sent.
    fn idle(kbps: u32) -> Load {
        Load {
            kbps,
            sent_kbps: 20,
            dropped: false,
        }
    }

    /// A second of frames dropping for lack of bandwidth at `kbps`.
    fn dropping(kbps: u32) -> Load {
        Load {
            kbps,
            sent_kbps: kbps,
            dropped: true,
        }
    }

    /// 1440p60 with no floor.
    fn up(a: &mut AutoScale, t: Instant, load: Load) -> Option<usize> {
        a.update(t, load, 2560, 1440, 60, 0)
    }

    /// A scaler that has streamed with plenty of bitrate until `t`.
    fn settled(t: Instant) -> AutoScale {
        let mut a = AutoScale::default();
        for i in (1..=LOOKBACK.as_secs() + 1).rev() {
            up(&mut a, t - S * i as u32, busy(1_000_000));
        }
        a
    }

    #[test]
    fn sizes_scale_to_multiples_of_8() {
        assert_eq!(size(2560, 1440, 0), (2560, 1440));
        assert_eq!(size(2560, 1440, 1), (1920, 1080));
        assert_eq!(size(2560, 1440, 2), (1280, 720));
        assert_eq!(size(2560, 1440, 3), (960, 536));
        assert_eq!(size(1366, 768, 1), (1024, 576));
        assert_eq!(size(1366, 768, 0), (1366, 768));
    }

    #[test]
    fn stops_at_360_lines() {
        assert_eq!(last_step(2560, 1440, 0), 3);
        assert_eq!(last_step(1280, 720, 0), 2);
        assert_eq!(last_step(640, 360, 0), 0);
    }

    #[test]
    fn the_browser_floor_limits_the_last_step() {
        assert_eq!(last_step(2560, 1440, 100), 0);
        assert_eq!(last_step(2560, 1440, 75), 1);
        assert_eq!(last_step(2560, 1440, 60), 1);
        assert_eq!(last_step(2560, 1440, 50), 2);
        assert_eq!(last_step(2560, 1440, 1), 3);
        // The 360-line limit still applies under a low floor.
        assert_eq!(last_step(1280, 720, 1), 2);
    }

    #[test]
    fn steps_down_once_the_bitrate_stays_low_for_the_lookback() {
        let t = Instant::now();
        let mut a = settled(t);
        // 1440p60 needs about 8.8 Mb/s.
        for i in 0..15 {
            assert_eq!(up(&mut a, t + S * i, busy(3000)), None, "second {i}");
        }
        // 3 Mb/s holds up 720p60 (about 2.2 Mb/s) but not 1080p60.
        assert_eq!(up(&mut a, t + S * 15, busy(3000)), Some(2));
        assert_eq!(a.step(), 2);
        assert_eq!(up(&mut a, t + S * 16, busy(3000)), None);
    }

    #[test]
    fn a_sawtooth_estimate_does_not_step_down() {
        // The Wi-Fi run: a probe finds 50 Mb/s, a delay spike drops the
        // estimate under what 1440p needs, it climbs, and again.
        let t = Instant::now();
        let mut a = settled(t);
        for i in 0..120 {
            let kbps = if i % 10 == 0 { 37_000 } else { 6000 };
            assert_eq!(up(&mut a, t + S * i, busy(kbps)), None, "second {i}");
        }
        assert_eq!(a.step(), 0);
    }

    #[test]
    fn steps_down_from_the_recent_best_bitrate() {
        let t = Instant::now();
        let mut a = settled(t);
        // 6 Mb/s a while ago holds up 1080p (about 5 Mb/s) even though the
        // estimate is lower now.
        up(&mut a, t, busy(6000));
        for i in 1..15 {
            up(&mut a, t + S * i, busy(2000));
        }
        assert_eq!(up(&mut a, t + S * 15, busy(2000)), Some(1));
    }

    #[test]
    fn a_still_desktop_never_steps_down() {
        // The estimate collapses to a fraction of the tiny repeat frames'
        // rate; nothing moves, so it says nothing about the link.
        let t = Instant::now();
        let mut a = settled(t);
        for i in 0..120 {
            assert_eq!(up(&mut a, t + S * i, idle(300)), None);
        }
        assert_eq!(a.step(), 0);
    }

    #[test]
    fn dropped_frames_step_down_soon() {
        let t = Instant::now();
        let mut a = settled(t);
        assert_eq!(up(&mut a, t, dropping(3000)), None);
        assert_eq!(up(&mut a, t + S, busy(3000)), None);
        // A blip back up restarts the wait.
        assert_eq!(up(&mut a, t + S, busy(20_000)), None);
        assert_eq!(up(&mut a, t + S * 2, dropping(3000)), None);
        assert_eq!(up(&mut a, t + S * 4, busy(3000)), Some(2));
    }

    #[test]
    fn idle_seconds_restart_the_wait() {
        let t = Instant::now();
        let mut a = settled(t);
        assert_eq!(up(&mut a, t, dropping(3000)), None);
        assert_eq!(up(&mut a, t + S, dropping(3000)), None);
        assert_eq!(up(&mut a, t + S * 2, idle(3000)), None);
        assert_eq!(up(&mut a, t + S * 3, dropping(3000)), None);
        assert_eq!(up(&mut a, t + S * 4, dropping(3000)), None);
        assert_eq!(up(&mut a, t + S * 5, dropping(3000)), Some(2));
    }

    #[test]
    fn dropped_frames_count_as_busy() {
        // Frames held back or dropped: the queue is full even if what got
        // out was under half the target.
        let t = Instant::now();
        let mut a = settled(t);
        let held = Load {
            kbps: 3000,
            sent_kbps: 900,
            dropped: true,
        };
        assert_eq!(up(&mut a, t, held), None);
        assert_eq!(up(&mut a, t + S * 2, held), Some(2));
    }

    #[test]
    fn goes_no_lower_than_the_last_step() {
        let t = Instant::now();
        let mut a = settled(t);
        up(&mut a, t, dropping(200));
        assert_eq!(up(&mut a, t + S * 2, dropping(200)), Some(3));
        assert_eq!(up(&mut a, t + S * 10, dropping(200)), None);
    }

    #[test]
    fn goes_no_lower_than_the_browser_floor() {
        let t = Instant::now();
        let mut a = settled(t);
        a.update(t, dropping(200), 2560, 1440, 60, 75);
        assert_eq!(
            a.update(t + S * 2, dropping(200), 2560, 1440, 60, 75),
            Some(1)
        );
        assert_eq!(
            a.update(t + S * 10, dropping(200), 2560, 1440, 60, 75),
            None
        );
        // Off: never scales.
        let mut a = settled(t);
        a.update(t, dropping(200), 2560, 1440, 60, 100);
        assert_eq!(
            a.update(t + S * 2, dropping(200), 2560, 1440, 60, 100),
            None
        );
        assert_eq!(a.step(), 0);
    }

    #[test]
    fn raising_the_floor_steps_straight_back_up() {
        let t = Instant::now();
        let mut a = settled(t);
        up(&mut a, t, dropping(200));
        assert_eq!(up(&mut a, t + S * 2, dropping(200)), Some(3));
        assert_eq!(a.limit(t + S * 3, 2560, 1440, 50), Some(2));
        assert_eq!(a.limit(t + S * 3, 2560, 1440, 50), None);
        assert_eq!(a.update(t + S * 4, busy(200), 2560, 1440, 60, 100), Some(0));
    }

    #[test]
    fn steps_up_one_at_a_time_after_a_long_stretch_of_headroom() {
        let t = Instant::now();
        let mut a = settled(t);
        up(&mut a, t, dropping(1000));
        assert_eq!(up(&mut a, t + S * 2, dropping(1000)), Some(3));
        // Enough for 720p, but not with margin: stay.
        assert_eq!(up(&mut a, t + S * 3, busy(2500)), None);
        assert_eq!(up(&mut a, t + S * 20, busy(2500)), None);
        // Plenty: up one step after ten seconds.
        assert_eq!(up(&mut a, t + S * 21, busy(30_000)), None);
        assert_eq!(up(&mut a, t + S * 30, busy(30_000)), None);
        assert_eq!(up(&mut a, t + S * 31, busy(30_000)), Some(2));
        assert_eq!(up(&mut a, t + S * 41, busy(30_000)), None);
        assert_eq!(up(&mut a, t + S * 50, busy(30_000)), None);
        assert_eq!(up(&mut a, t + S * 51, busy(30_000)), Some(1));
    }

    #[test]
    fn a_size_that_does_not_hold_waits_longer_next_time() {
        let t = Instant::now();
        let mut a = settled(t);
        up(&mut a, t, dropping(3000));
        assert_eq!(up(&mut a, t + S * 2, dropping(3000)), Some(2));
        up(&mut a, t + S * 3, busy(30_000));
        assert_eq!(up(&mut a, t + S * 13, busy(30_000)), Some(1));
        // Down again within half a minute of going up.
        up(&mut a, t + S * 18, dropping(3000));
        assert_eq!(up(&mut a, t + S * 20, dropping(3000)), Some(2));
        // Now twenty seconds of headroom before the next try.
        up(&mut a, t + S * 21, busy(30_000));
        assert_eq!(up(&mut a, t + S * 31, busy(30_000)), None);
        assert_eq!(up(&mut a, t + S * 40, busy(30_000)), None);
        assert_eq!(up(&mut a, t + S * 41, busy(30_000)), Some(1));
    }

    #[test]
    fn the_starting_estimate_does_not_step_down() {
        // A new connection's 7.5 Mb/s target, under 1440p60's need, while
        // the estimate climbs.
        let t = Instant::now();
        let mut a = AutoScale::default();
        assert_eq!(up(&mut a, t, busy(7500)), None);
        assert_eq!(up(&mut a, t + S * 4, busy(7500)), None);
        assert_eq!(up(&mut a, t + S * 5, busy(30_000)), None);
        assert_eq!(up(&mut a, t + S * 30, busy(30_000)), None);
        assert_eq!(a.step(), 0);
        // Low from then on: step down once 30 Mb/s is out of the lookback.
        for i in 31..46 {
            assert_eq!(up(&mut a, t + S * i, busy(3000)), None, "second {i}");
        }
        assert_eq!(up(&mut a, t + S * 46, busy(3000)), Some(2));
    }

    #[test]
    fn no_second_step_down_right_after_a_change() {
        let t = Instant::now();
        let mut a = settled(t);
        up(&mut a, t, dropping(3000));
        assert_eq!(up(&mut a, t + S * 2, dropping(3000)), Some(2));
        // The new size needs a moment too.
        up(&mut a, t + S * 3, dropping(200));
        assert_eq!(up(&mut a, t + S * 6, dropping(200)), None);
        assert_eq!(up(&mut a, t + S * 7, dropping(200)), None);
        assert_eq!(up(&mut a, t + S * 9, dropping(200)), Some(3));
    }

    #[test]
    fn a_small_configured_size_never_scales() {
        let t = Instant::now();
        let mut a = AutoScale::default();
        a.update(t, busy(100), 640, 360, 60, 0);
        assert_eq!(a.update(t + S * 5, busy(100), 640, 360, 60, 0), None);
        assert_eq!(a.step(), 0);
    }
}
