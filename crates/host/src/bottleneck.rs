//! Tells which end of the stream is the bottleneck, and relieves it.

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
/// Browser reports older than this are ignored.
pub(crate) const CLIENT_STATS_TTL: Duration = Duration::from_secs(3);
/// Encode (capture to encoded) over this many frame times is too slow.
const ENCODE_LIMIT: f32 = 1.5;
/// Decode over this share of a frame time is too slow.
const DECODE_LIMIT: f32 = 0.8;
/// A step up is tried only if the figure, scaled by the pixels the larger size adds, stays under
/// this share of its limit.
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
    /// Into the encoder to encoded, averaged: not the time the pacer held the image first.
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
    quiet_since: Option<Instant>,
    last_relief: Option<Instant>,
    relief_after: Option<Duration>,
    current: Limiter,
    /// What the last step down was for.
    relief_cause: Limiter,
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
        let behind = host.fps >= 15.0 && c.fps >= 1.0 && c.fps < 0.7 * host.fps;
        let stalled = c.fps < 1.0;
        if !stalled && (c.dropped > 0 || c.freezes > 0 || behind) {
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

    /// Takes the last second's figures.
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
                    if self
                        .last_relief
                        .is_some_and(|t| now.saturating_duration_since(t) < FLAP_WINDOW)
                    {
                        let wait = self.relief_after.unwrap_or(RELIEF_AFTER) * 2;
                        self.relief_after = Some(wait.min(MAX_RELIEF_AFTER));
                    }
                    self.step = at + 1;
                    self.relief_cause = current;
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
                    let grow = (f64::from(w1) * f64::from(h1) / (f64::from(w0) * f64::from(h0)))
                        .sqrt() as f32;
                    let budget = 1000.0 / host.fps_target.max(1) as f32;
                    let holds = self.last_encode_ms * grow < ENCODE_LIMIT * RELIEF_MARGIN * budget
                        && self.last_decode_ms * grow < DECODE_LIMIT * RELIEF_MARGIN * budget;
                    if holds {
                        self.step -= 1;
                        if self.step == 0 {
                            self.relief_cause = Limiter::None;
                        }
                        self.last_relief = Some(now);
                        changed = Some(self.step);
                        self.stepped(now);
                    }
                }
            }
        }
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
            relieving_for: if self.step > 0 {
                self.relief_cause
            } else {
                Limiter::None
            },
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
        let o = r.seconds(35, host(10.0), Some(client(3.0)));
        assert!(steps(&o).is_empty());
        let o = r.seconds(40, host(10.0), Some(client(3.0)));
        assert_eq!(steps(&o), vec![0]);
    }

    #[test]
    fn software_decode_that_scales_slowly_comes_back_to_native() {
        let mut r = Run::new();
        r.seconds(30, host(8.0), Some(client(14.0)));
        assert!(r.b.step() >= 2);
        for _ in 0..400 {
            let decode = match r.b.step() {
                0 => 8.0,
                1 => 7.0,
                _ => 5.5,
            };
            r.second(host(8.0), Some(client(decode)));
        }
        assert_eq!(r.b.step(), 0);
    }

    #[test]
    fn the_report_says_what_the_size_is_held_down_for() {
        let mut r = Run::new();
        let mut reports: Vec<BottleneckReport> = Vec::new();
        for _ in 0..4 {
            reports.extend(r.second(host(8.0), Some(client(15.0))).report);
        }
        assert_eq!(reports[0].limiter, Limiter::ClientDecode);
        assert_eq!(reports[0].relieving_for, Limiter::ClientDecode);
        for _ in 0..8 {
            reports.extend(r.second(host(8.0), Some(client(3.0))).report);
        }
        let rep = reports.last().unwrap();
        assert_eq!(rep.limiter, Limiter::None);
        assert!(rep.relieving);
        assert_eq!(rep.relieving_for, Limiter::ClientDecode);
    }

    #[test]
    fn a_browser_decoding_nothing_is_not_a_slow_decoder() {
        let mut r = Run::new();
        let mut c = client(0.0);
        c.fps = 0.0;
        c.decode_ms = None;
        let o = r.seconds(30, host(8.0), Some(c));
        assert!(steps(&o).is_empty());
        assert!(o.iter().all(|o| o.report.is_none()));
    }
}
