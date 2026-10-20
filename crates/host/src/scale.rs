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
/// How long the bitrate must stay comfortably above the next size up's
/// need before stepping up.
const UP_AFTER: Duration = Duration::from_secs(10);
/// "Comfortably": this many times what the next size up needs.
const UP_MARGIN: f64 = 1.5;
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

/// The smallest step allowed for a configured size.
fn last_step(width: u32, height: u32) -> usize {
    (0..STEPS.len())
        .rev()
        .find(|&s| size(width, height, s).1 >= MIN_HEIGHT.min(height))
        .unwrap_or(0)
}

#[derive(Debug, Default)]
pub(crate) struct AutoScale {
    step: usize,
    /// The first update since the last reset or size change.
    since_change: Option<Instant>,
    /// Since when the bitrate has been too low for the current step.
    low_since: Option<Instant>,
    /// Since when it has been enough for the step above.
    high_since: Option<Instant>,
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

    /// Takes the encoder's target bitrate. Returns the new step when the
    /// encode size should change.
    pub(crate) fn update(
        &mut self,
        now: Instant,
        kbps: u32,
        width: u32,
        height: u32,
        fps: u32,
    ) -> Option<usize> {
        let kbps = f64::from(kbps);
        let need = |step: usize| {
            let (w, h) = size(width, height, step);
            min_kbps(w, h, fps)
        };
        let last = last_step(width, height);
        let settled =
            now.saturating_duration_since(*self.since_change.get_or_insert(now)) >= SETTLE;
        if settled && self.step < last && kbps < need(self.step) {
            self.high_since = None;
            let since = *self.low_since.get_or_insert(now);
            if now.saturating_duration_since(since) >= DOWN_AFTER {
                // Straight to the largest size the bitrate holds up.
                self.step = (self.step + 1..=last)
                    .find(|&s| kbps >= need(s))
                    .unwrap_or(last);
                self.low_since = None;
                self.since_change = Some(now);
                return Some(self.step);
            }
            return None;
        }
        self.low_since = None;
        if self.step > 0 && kbps >= need(self.step - 1) * UP_MARGIN {
            let since = *self.high_since.get_or_insert(now);
            if now.saturating_duration_since(since) >= UP_AFTER {
                self.step -= 1;
                self.high_since = None;
                self.since_change = Some(now);
                return Some(self.step);
            }
        } else {
            self.high_since = None;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Duration = Duration::from_secs(1);

    /// A scaler whose settling time after connecting is over by `t`.
    fn settled(t: Instant) -> AutoScale {
        let mut a = AutoScale::default();
        a.update(t - SETTLE, 1_000_000, 2560, 1440, 60);
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
        assert_eq!(last_step(2560, 1440), 3);
        assert_eq!(last_step(1280, 720), 2);
        assert_eq!(last_step(640, 360), 0);
    }

    #[test]
    fn steps_down_only_after_the_bitrate_stays_low() {
        let t = Instant::now();
        let mut a = settled(t);
        // 1440p60 needs about 8.8 Mb/s.
        assert_eq!(a.update(t, 3000, 2560, 1440, 60), None);
        assert_eq!(a.update(t + S, 3000, 2560, 1440, 60), None);
        // A blip back up restarts the wait.
        assert_eq!(a.update(t + S, 20_000, 2560, 1440, 60), None);
        assert_eq!(a.update(t + S * 2, 3000, 2560, 1440, 60), None);
        // 3 Mb/s holds up 720p60 (about 2.2 Mb/s) but not 1080p60.
        assert_eq!(a.update(t + S * 4, 3000, 2560, 1440, 60), Some(2));
        assert_eq!(a.step(), 2);
        assert_eq!(a.update(t + S * 10, 3000, 2560, 1440, 60), None);
    }

    #[test]
    fn goes_no_lower_than_the_last_step() {
        let t = Instant::now();
        let mut a = settled(t);
        a.update(t, 200, 2560, 1440, 60);
        assert_eq!(a.update(t + S * 2, 200, 2560, 1440, 60), Some(3));
        assert_eq!(a.update(t + S * 10, 200, 2560, 1440, 60), None);
    }

    #[test]
    fn steps_up_one_at_a_time_after_a_long_stretch_of_headroom() {
        let t = Instant::now();
        let mut a = settled(t);
        a.update(t, 1000, 2560, 1440, 60);
        assert_eq!(a.update(t + S * 2, 1000, 2560, 1440, 60), Some(3));
        // Enough for 720p, but not with margin: stay.
        assert_eq!(a.update(t + S * 3, 2500, 2560, 1440, 60), None);
        assert_eq!(a.update(t + S * 20, 2500, 2560, 1440, 60), None);
        // Plenty: up one step after ten seconds.
        assert_eq!(a.update(t + S * 21, 30_000, 2560, 1440, 60), None);
        assert_eq!(a.update(t + S * 30, 30_000, 2560, 1440, 60), None);
        assert_eq!(a.update(t + S * 31, 30_000, 2560, 1440, 60), Some(2));
        assert_eq!(a.update(t + S * 41, 30_000, 2560, 1440, 60), None);
        assert_eq!(a.update(t + S * 50, 30_000, 2560, 1440, 60), None);
        assert_eq!(a.update(t + S * 51, 30_000, 2560, 1440, 60), Some(1));
    }

    #[test]
    fn the_starting_estimate_does_not_step_down() {
        // A new connection's 7.5 Mb/s target, under 1440p60's need, while
        // the estimate climbs.
        let t = Instant::now();
        let mut a = AutoScale::default();
        assert_eq!(a.update(t, 7500, 2560, 1440, 60), None);
        assert_eq!(a.update(t + S * 4, 7500, 2560, 1440, 60), None);
        assert_eq!(a.update(t + S * 5, 30_000, 2560, 1440, 60), None);
        assert_eq!(a.update(t + S * 30, 30_000, 2560, 1440, 60), None);
        assert_eq!(a.step(), 0);
        // Still low once settled: step down after the usual wait.
        assert_eq!(a.update(t + S * 31, 3000, 2560, 1440, 60), None);
        assert_eq!(a.update(t + S * 33, 3000, 2560, 1440, 60), Some(2));
    }

    #[test]
    fn no_second_step_down_right_after_a_change() {
        let t = Instant::now();
        let mut a = settled(t);
        a.update(t, 3000, 2560, 1440, 60);
        assert_eq!(a.update(t + S * 2, 3000, 2560, 1440, 60), Some(2));
        // The new size needs a moment too.
        a.update(t + S * 3, 200, 2560, 1440, 60);
        assert_eq!(a.update(t + S * 6, 200, 2560, 1440, 60), None);
        assert_eq!(a.update(t + S * 7, 200, 2560, 1440, 60), None);
        assert_eq!(a.update(t + S * 9, 200, 2560, 1440, 60), Some(3));
    }

    #[test]
    fn a_small_configured_size_never_scales() {
        let t = Instant::now();
        let mut a = AutoScale::default();
        a.update(t, 100, 640, 360, 60);
        assert_eq!(a.update(t + S * 5, 100, 640, 360, 60), None);
        assert_eq!(a.step(), 0);
    }
}
