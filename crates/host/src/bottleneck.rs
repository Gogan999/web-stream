//! Tells which end of the stream is the bottleneck, and relieves it.
//!
//! The host knows its own side (how long a frame takes to encode, whether the
//! send queue fell behind, how much was lost) and the browser reports its
//! own (decode time, frames dropped or frozen) once a second. Together they
//! say what is limiting the picture:
//!
//! * the **network**: frames are late or dropped, packets are lost. The
//!   bandwidth estimate already moves the bitrate and [`crate::scale`]
//!   already shrinks the picture for it, so this only reports it;
//! * the **host encoder**: capture to encoded takes longer than a frame. A
//!   smaller picture is cheaper to encode, so the picture is shrunk;
//! * the **browser's decoder**: it takes most of a frame to decode, or drops
//!   or skips frames while the network is clean. A smaller picture is cheaper
//!   to decode too, so the picture is shrunk.
//!
//! Limits: with two monitors only the main stream's figures are counted (the
//! browser's report comes from the first video, whose decode load is doubled
//! by the second, so decode time is underestimated). A tab that is visible but
//! occluded or on another screen reports `hidden: false` while throttled; the
//! "showing too few frames" test could misfire there, which the 3-of-5 rule
//! only limits.
//!
//! Each second is judged on its own, then a limiter has to show in 3 of the
//! last 5 seconds before anything changes, so a single hitch does nothing.
//! Every change restarts the wait, so a step has time to show its effect.
//! The size comes back up after a quiet stretch, and only if the figures
//! measured at the current size say the larger one would hold; one that
//! doesn't hold doubles the wait next time, as [`crate::scale`] does.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use ws_proto::{BottleneckReport, ClientStats, Limiter};

use crate::scale;

/// Seconds judged together.
const WINDOW: usize = 5;
/// This many of them must agree on a limiter.
const NEEDED: usize = 3;
/// Least time between two steps down.
const COOLDOWN: Duration = Duration::from_secs(6);
/// Quiet for this long before a step back up.
const RELIEF_AFTER: Duration = Duration::from_secs(25);
/// The longest that doubling makes it.
const MAX_RELIEF_AFTER: Duration = Duration::from_secs(200);
/// A limiter back this soon after a step up means the size didn't hold.
const FLAP_WINDOW: Duration = Duration::from_secs(60);
/// Browser reports older than this are ignored (it stopped sending, or the
/// page is not running).
pub(crate) const CLIENT_STATS_TTL: Duration = Duration::from_secs(3);
/// Encode (capture to encoded) over this many frame times is too slow. More
/// than one, because a frame waits up to a frame time for its slot.
const ENCODE_LIMIT: f32 = 1.5;
/// Decode over this share of a frame time is too slow.
const DECODE_LIMIT: f32 = 0.8;
/// A step up is tried only if the figure, scaled by the pixels the larger
/// size adds, stays under this share of its limit.
const RELIEF_MARGIN: f32 = 0.8;
/// Packet loss above this is the network's.
const LOSS_LIMIT: f32 = 0.02;

/// The host's side over the last second.
#[derive(Debug, Clone, Copy)]
pub(crate) struct HostSample {
    /// The frame rate asked for.
    pub fps_target: u32,
    /// Frames handed to the network per second.
    pub fps: f32,
    /// Into the encoder to encoded, averaged: not the time the pacer held the
    /// image first (Source quality holds one up to ~200 ms).
    pub encode_ms: f32,
    /// Frames dropped for lateness or the queue, or packets the socket refused.
    pub dropped: u32,
    pub loss: Option<f32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Ok,
    Limited(Limiter),
}

/// What [`Bottleneck::update`] decided.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Outcome {
    /// Set when the encode size should change, to the new extra step.
    pub step: Option<usize>,
    /// Set when what the browser is told changed.
    pub report: Option<BottleneckReport>,
}

#[derive(Debug, Default)]
pub(crate) struct Bottleneck {
    /// Extra scale steps on top of the bitrate's ([`scale::AutoScale`]).
    step: usize,
    history: VecDeque<Verdict>,
    /// The last second's figures, for judging whether a larger size would hold.
    last_encode_ms: f32,
    last_decode_ms: f32,
    last_step_change: Option<Instant>,
    /// Since when nothing limited, at least.
    quiet_since: Option<Instant>,
    last_relief: Option<Instant>,
    relief_after: Option<Duration>,
    current: Limiter,
    /// Last report sent, to send changes only.
    sent: Option<(Limiter, usize)>,
    detail: String,
}

fn judge(host: &HostSample, client: Option<&ClientStats>) -> (Verdict, String) {
    let budget = 1000.0 / host.fps_target.max(1) as f32;
    let network = host.dropped > 0 || host.loss.is_some_and(|l| l > LOSS_LIMIT);
    if network {
        let what = match host.loss {
            Some(l) if l > LOSS_LIMIT => format!("{:.1}% of packets lost", l * 100.0),
            _ => format!("{} frame(s) dropped, the link fell behind", host.dropped),
        };
        return (Verdict::Limited(Limiter::Network), what);
    }
    if host.encode_ms > ENCODE_LIMIT * budget {
        return (
            Verdict::Limited(Limiter::HostEncode),
            format!(
                "host encode takes {:.1} ms (a frame is {:.1} ms)",
                host.encode_ms, budget
            ),
        );
    }
    if let Some(c) = client.filter(|c| !c.hidden) {
        if let Some(d) = c.decode_ms.filter(|d| *d > DECODE_LIMIT * budget) {
            return (
                Verdict::Limited(Limiter::ClientDecode),
                format!("browser decode takes {d:.1} ms (a frame is {budget:.1} ms)"),
            );
        }
        // The host sends frames the browser neither shows nor even decodes,
        // with the link clean: the decoder or renderer can't keep up.
        let behind = host.fps >= 15.0 && c.fps < 0.7 * host.fps;
        if c.dropped > 0 || c.freezes > 0 || behind {
            return (
                Verdict::Limited(Limiter::ClientDecode),
                format!(
                    "browser shows {:.0} of {:.0} fps ({} dropped)",
                    c.fps, host.fps, c.dropped
                ),
            );
        }
    }
    (Verdict::Ok, String::new())
}

impl Bottleneck {
    pub(crate) fn step(&self) -> usize {
        self.step
    }

    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    /// Takes the last second's figures. `client` is the browser's latest
    /// report if it is recent. `width`, `height` and `min_percent` are the
    /// browser's asked size and scale floor; `auto_step` the bitrate-driven
    /// scale step, which counts toward the floor.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn update(
        &mut self,
        now: Instant,
        host: HostSample,
        client: Option<&ClientStats>,
        width: u32,
        height: u32,
        min_percent: u8,
        auto_step: usize,
    ) -> Outcome {
        let (verdict, detail) = judge(&host, client);
        // A background tab says nothing: neither a limit nor quiet.
        let silent = client.is_some_and(|c| c.hidden) && verdict == Verdict::Ok;
        if !silent {
            self.history.push_back(verdict);
            while self.history.len() > WINDOW {
                self.history.pop_front();
            }
        }
        if verdict == Verdict::Ok {
            if let Some(c) = client.filter(|c| !c.hidden) {
                self.last_decode_ms = c.decode_ms.unwrap_or(0.0);
            }
            self.last_encode_ms = host.encode_ms;
        }
        let limited = |l: Limiter| {
            self.history
                .iter()
                .filter(|v| **v == Verdict::Limited(l))
                .count()
        };
        let current = [Limiter::HostEncode, Limiter::ClientDecode, Limiter::Network]
            .into_iter()
            .find(|l| limited(*l) >= NEEDED)
            .unwrap_or(Limiter::None);
        if current != Limiter::None {
            self.detail = detail;
        } else {
            self.detail.clear();
        }
        self.current = current;

        let last = scale::clamp_step(usize::MAX, width, height, min_percent);
        let mut changed = None;
        let cooled = self
            .last_step_change
            .is_none_or(|t| now.saturating_duration_since(t) >= COOLDOWN);
        match current {
            Limiter::HostEncode | Limiter::ClientDecode => {
                self.quiet_since = None;
                let at = self.step.max(auto_step);
                if cooled && at < last {
                    // The size didn't hold if this follows a recent step up.
                    if self
                        .last_relief
                        .is_some_and(|t| now.saturating_duration_since(t) < FLAP_WINDOW)
                    {
                        let wait = self.relief_after.unwrap_or(RELIEF_AFTER) * 2;
                        self.relief_after = Some(wait.min(MAX_RELIEF_AFTER));
                    }
                    self.step = at + 1;
                    changed = Some(self.step);
                    self.stepped(now);
                }
            }
            Limiter::Network => self.quiet_since = None,
            Limiter::None => {
                let quiet = *self.quiet_since.get_or_insert(now);
                let after = self.relief_after.unwrap_or(RELIEF_AFTER);
                if self.step > 0 && cooled && now.saturating_duration_since(quiet) >= after {
                    let (w0, h0) = scale::size(width, height, self.step);
                    let (w1, h1) = scale::size(width, height, self.step - 1);
                    let grow =
                        (f64::from(w1) * f64::from(h1) / (f64::from(w0) * f64::from(h0))) as f32;
                    let budget = 1000.0 / host.fps_target.max(1) as f32;
                    let holds = self.last_encode_ms * grow < ENCODE_LIMIT * RELIEF_MARGIN * budget
                        && self.last_decode_ms * grow < DECODE_LIMIT * RELIEF_MARGIN * budget;
                    if holds {
                        self.step -= 1;
                        self.last_relief = Some(now);
                        changed = Some(self.step);
                        self.stepped(now);
                    }
                }
            }
        }
        // The size in force counts the bitrate's step too; the report says
        // what the browser sees.
        let report = self.report(width, height, auto_step);
        Outcome {
            step: changed,
            report,
        }
    }

    fn stepped(&mut self, now: Instant) {
        self.last_step_change = Some(now);
        self.history.clear();
        self.quiet_since = None;
    }

    fn report(&mut self, width: u32, height: u32, auto_step: usize) -> Option<BottleneckReport> {
        let key = (self.current, self.step.max(auto_step));
        if self.sent == Some(key) || (self.sent.is_none() && key == (Limiter::None, 0)) {
            return None;
        }
        self.sent = Some(key);
        let (w, _) = scale::size(width, height, key.1);
        Some(BottleneckReport {
            limiter: self.current,
            detail: self.detail.clone(),
            scale_percent: (u64::from(w) * 100 / u64::from(width.max(1))) as u32,
            relieving: self.step > 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: u32 = 2560;
    const H: u32 = 1440;

    fn host(encode_ms: f32) -> HostSample {
        HostSample {
            fps_target: 60,
            fps: 60.0,
            encode_ms,
            dropped: 0,
            loss: None,
        }
    }

    fn client(decode_ms: f32) -> ClientStats {
        ClientStats {
            fps: 60.0,
            decode_ms: Some(decode_ms),
            dropped: 0,
            freezes: 0,
            hidden: false,
        }
    }

    struct Run {
        b: Bottleneck,
        t: Instant,
    }

    impl Run {
        fn new() -> Self {
            Self {
                b: Bottleneck::default(),
                t: Instant::now(),
            }
        }

        fn second(&mut self, h: HostSample, c: Option<ClientStats>) -> Outcome {
            self.t += Duration::from_secs(1);
            self.b.update(self.t, h, c.as_ref(), W, H, 0, 0)
        }

        fn seconds(&mut self, n: usize, h: HostSample, c: Option<ClientStats>) -> Vec<Outcome> {
            (0..n).map(|_| self.second(h, c.clone())).collect()
        }
    }

    fn steps(o: &[Outcome]) -> Vec<usize> {
        o.iter().filter_map(|o| o.step).collect()
    }

    #[test]
    fn a_healthy_stream_changes_nothing() {
        let mut r = Run::new();
        let o = r.seconds(60, host(8.0), Some(client(4.0)));
        assert!(steps(&o).is_empty());
        assert!(o.iter().all(|o| o.report.is_none()));
    }

    #[test]
    fn a_slow_host_encoder_shrinks_the_picture() {
        let mut r = Run::new();
        let o = r.seconds(4, host(40.0), Some(client(4.0)));
        assert_eq!(steps(&o), vec![1]);
        let rep = o.iter().find_map(|o| o.report.clone()).unwrap();
        assert_eq!(rep.limiter, Limiter::HostEncode);
        assert!(rep.relieving);
        assert_eq!(rep.scale_percent, 75);
    }

    #[test]
    fn a_slow_browser_decoder_shrinks_the_picture() {
        let mut r = Run::new();
        let o = r.seconds(4, host(8.0), Some(client(15.0)));
        assert_eq!(steps(&o), vec![1]);
        let rep = o.iter().find_map(|o| o.report.clone()).unwrap();
        assert_eq!(rep.limiter, Limiter::ClientDecode);
    }

    #[test]
    fn a_browser_showing_too_few_frames_is_its_own_limit() {
        let mut r = Run::new();
        let mut c = client(6.0);
        c.fps = 30.0;
        let o = r.seconds(4, host(8.0), Some(c));
        assert_eq!(steps(&o), vec![1]);
    }

    #[test]
    fn network_trouble_is_reported_but_not_acted_on() {
        let mut r = Run::new();
        let mut h = host(8.0);
        h.dropped = 3;
        let o = r.seconds(8, h, Some(client(4.0)));
        assert!(steps(&o).is_empty());
        let rep = o.iter().find_map(|o| o.report.clone()).unwrap();
        assert_eq!(rep.limiter, Limiter::Network);
        assert!(!rep.relieving);
    }

    #[test]
    fn dropped_frames_do_not_blame_the_decoder() {
        // The browser's low fps with the link losing frames is the link's.
        let mut r = Run::new();
        let mut h = host(8.0);
        h.loss = Some(0.05);
        let mut c = client(4.0);
        c.fps = 20.0;
        c.dropped = 2;
        let o = r.seconds(8, h, Some(c));
        assert!(steps(&o).is_empty());
    }

    #[test]
    fn one_hitch_does_nothing() {
        let mut r = Run::new();
        r.seconds(10, host(8.0), Some(client(4.0)));
        let o = r.second(host(50.0), Some(client(4.0)));
        assert_eq!(o.step, None);
        let o = r.seconds(10, host(8.0), Some(client(4.0)));
        assert!(steps(&o).is_empty());
    }

    #[test]
    fn a_background_tab_is_ignored() {
        let mut r = Run::new();
        let mut c = client(30.0);
        c.hidden = true;
        c.fps = 1.0;
        let o = r.seconds(20, host(8.0), Some(c));
        assert!(steps(&o).is_empty());
    }

    #[test]
    fn steps_down_again_only_after_the_cooldown() {
        let mut r = Run::new();
        let o = r.seconds(30, host(40.0), Some(client(4.0)));
        let n = steps(&o);
        assert_eq!(n[0], 1);
        assert!(n.len() >= 2 && n.len() <= 4, "{n:?}");
        assert_eq!(*n.last().unwrap(), n.len());
    }

    #[test]
    fn never_below_the_browsers_floor() {
        let mut r = Run::new();
        r.t += Duration::from_secs(1);
        let mut max = 0;
        for _ in 0..60 {
            r.t += Duration::from_secs(1);
            let o =
                r.b.update(r.t, host(40.0), Some(&client(4.0)), W, H, 100, 0);
            max = max.max(o.step.unwrap_or(0));
        }
        assert_eq!(max, 0, "floor of 100% is automatic scaling off");
    }

    #[test]
    fn steps_back_up_after_a_quiet_stretch_when_it_would_hold() {
        let mut r = Run::new();
        r.seconds(4, host(40.0), Some(client(4.0)));
        assert_eq!(r.b.step(), 1);
        // At the smaller size the stream is comfortable.
        let o = r.seconds(40, host(10.0), Some(client(3.0)));
        assert_eq!(steps(&o), vec![0]);
        assert_eq!(r.b.step(), 0);
    }

    #[test]
    fn does_not_step_back_up_when_the_larger_size_would_not_hold() {
        let mut r = Run::new();
        r.seconds(4, host(40.0), Some(client(4.0)));
        // Under the limit now (22 ms < 25 ms), but 1.78x the pixels would not be.
        let o = r.seconds(120, host(22.0), Some(client(4.0)));
        assert!(steps(&o).is_empty());
        assert_eq!(r.b.step(), 1);
    }

    #[test]
    fn a_size_that_did_not_hold_waits_longer_next_time() {
        let mut r = Run::new();
        r.seconds(4, host(40.0), Some(client(4.0)));
        let o = r.seconds(40, host(10.0), Some(client(3.0)));
        assert_eq!(steps(&o), vec![0]);
        // Trouble returns right away.
        let o = r.seconds(3, host(40.0), Some(client(4.0)));
        assert_eq!(steps(&o), vec![1]);
        // The wait is now twice as long: nothing at 30 s of quiet.
        let o = r.seconds(35, host(10.0), Some(client(3.0)));
        assert!(steps(&o).is_empty());
        let o = r.seconds(40, host(10.0), Some(client(3.0)));
        assert_eq!(steps(&o), vec![0]);
    }
}
