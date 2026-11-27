//! Virtual Xbox 360 and DualShock 4 pads with the kernel drivers' evdev layouts (xpad,
//! hid-playstation), and their rumble.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use libc::input_event;
use tracing::{debug, warn};
use ws_proto::{GamepadState, HostControl};

use super::uinput::*;
use crate::gamepad::{Ds4Report, xbutton};
use crate::handler::GamepadKind;

/// Axes and buttons, in the order [`values`] fills them.
const X360_SLOTS: [(u16, u16); 19] = [
    (EV_ABS, ABS_X),
    (EV_ABS, ABS_Y),
    (EV_ABS, ABS_RX),
    (EV_ABS, ABS_RY),
    (EV_ABS, ABS_Z),
    (EV_ABS, ABS_RZ),
    (EV_ABS, ABS_HAT0X),
    (EV_ABS, ABS_HAT0Y),
    (EV_KEY, BTN_SOUTH),
    (EV_KEY, BTN_EAST),
    (EV_KEY, BTN_NORTH),
    (EV_KEY, BTN_WEST),
    (EV_KEY, BTN_TL),
    (EV_KEY, BTN_TR),
    (EV_KEY, BTN_SELECT),
    (EV_KEY, BTN_START),
    (EV_KEY, BTN_MODE),
    (EV_KEY, BTN_THUMBL),
    (EV_KEY, BTN_THUMBR),
];

const DS4_SLOTS: [(u16, u16); 21] = [
    (EV_ABS, ABS_X),
    (EV_ABS, ABS_Y),
    (EV_ABS, ABS_RX),
    (EV_ABS, ABS_RY),
    (EV_ABS, ABS_Z),
    (EV_ABS, ABS_RZ),
    (EV_ABS, ABS_HAT0X),
    (EV_ABS, ABS_HAT0Y),
    (EV_KEY, BTN_WEST),
    (EV_KEY, BTN_SOUTH),
    (EV_KEY, BTN_EAST),
    (EV_KEY, BTN_NORTH),
    (EV_KEY, BTN_TL),
    (EV_KEY, BTN_TR),
    (EV_KEY, BTN_TL2),
    (EV_KEY, BTN_TR2),
    (EV_KEY, BTN_SELECT),
    (EV_KEY, BTN_START),
    (EV_KEY, BTN_MODE),
    (EV_KEY, BTN_THUMBL),
    (EV_KEY, BTN_THUMBR),
];

pub const MAX_SLOTS: usize = DS4_SLOTS.len();

const fn stick(code: u16) -> Abs {
    Abs {
        code,
        min: -32768,
        max: 32767,
        fuzz: 16,
        flat: 128,
    }
}

const fn range(code: u16, min: i32, max: i32) -> Abs {
    Abs {
        code,
        min,
        max,
        fuzz: 0,
        flat: 0,
    }
}

const X360_ABS: [Abs; 8] = [
    stick(ABS_X),
    stick(ABS_Y),
    stick(ABS_RX),
    stick(ABS_RY),
    range(ABS_Z, 0, 255),
    range(ABS_RZ, 0, 255),
    range(ABS_HAT0X, -1, 1),
    range(ABS_HAT0Y, -1, 1),
];

const DS4_ABS: [Abs; 8] = [
    range(ABS_X, 0, 255),
    range(ABS_Y, 0, 255),
    range(ABS_RX, 0, 255),
    range(ABS_RY, 0, 255),
    range(ABS_Z, 0, 255),
    range(ABS_RZ, 0, 255),
    range(ABS_HAT0X, -1, 1),
    range(ABS_HAT0Y, -1, 1),
];

/// Effects a game can upload at once.
const FF_EFFECTS: u32 = 16;

fn keys(slots: &[(u16, u16)]) -> Vec<u16> {
    slots
        .iter()
        .filter(|(t, _)| *t == EV_KEY)
        .map(|&(_, c)| c)
        .collect()
}

/// Device identity and layout of each kind.
pub fn create(kind: GamepadKind) -> io::Result<Device> {
    match kind {
        GamepadKind::Xbox360 => Device::create(&Spec {
            name: "Microsoft X-Box 360 pad",
            bustype: BUS_USB,
            vendor: 0x045e,
            product: 0x028e,
            version: 0x0114,
            keys: &keys(&X360_SLOTS),
            rels: &[],
            abs: &X360_ABS,
            ff_effects: FF_EFFECTS,
        }),
        GamepadKind::DualShock4 => Device::create(&Spec {
            name: "Sony Computer Entertainment Wireless Controller",
            bustype: BUS_USB,
            vendor: 0x054c,
            product: 0x05c4,
            version: 0x8100,
            keys: &keys(&DS4_SLOTS),
            rels: &[],
            abs: &DS4_ABS,
            ff_effects: FF_EFFECTS,
        }),
    }
}

pub fn slots(kind: GamepadKind) -> &'static [(u16, u16)] {
    match kind {
        GamepadKind::Xbox360 => &X360_SLOTS,
        GamepadKind::DualShock4 => &DS4_SLOTS,
    }
}

fn hat(buttons: u16) -> (i32, i32) {
    let on = |b| i32::from(buttons & b != 0);
    (
        on(xbutton::DPAD_RIGHT) - on(xbutton::DPAD_LEFT),
        on(xbutton::DPAD_DOWN) - on(xbutton::DPAD_UP),
    )
}

/// The value of each slot of `kind` for `s`.
pub fn values(kind: GamepadKind, s: &GamepadState, out: &mut [i32; MAX_SLOTS]) {
    use xbutton::*;
    let b = |bit| i32::from(s.buttons & bit != 0);
    let (hx, hy) = hat(s.buttons);
    match kind {
        GamepadKind::Xbox360 => {
            let v = [
                i32::from(s.lx),
                i32::from(!s.ly),
                i32::from(s.rx),
                i32::from(!s.ry),
                i32::from(s.lt),
                i32::from(s.rt),
                hx,
                hy,
                b(A),
                b(B),
                b(X),
                b(Y),
                b(LEFT_SHOULDER),
                b(RIGHT_SHOULDER),
                b(BACK),
                b(START),
                b(GUIDE),
                b(LEFT_THUMB),
                b(RIGHT_THUMB),
            ];
            out[..v.len()].copy_from_slice(&v);
        }
        GamepadKind::DualShock4 => {
            let r = Ds4Report::from(s);
            *out = [
                i32::from(r.thumb_lx),
                i32::from(r.thumb_ly),
                i32::from(r.thumb_rx),
                i32::from(r.thumb_ry),
                i32::from(r.trigger_l),
                i32::from(r.trigger_r),
                hx,
                hy,
                b(X),
                b(A),
                b(B),
                b(Y),
                b(LEFT_SHOULDER),
                b(RIGHT_SHOULDER),
                i32::from(s.lt > 0),
                i32::from(s.rt > 0),
                b(BACK),
                b(START),
                b(GUIDE),
                b(LEFT_THUMB),
                b(RIGHT_THUMB),
            ];
        }
    }
}

/// Appends the events that move a pad from `old` to `new`, and a report if anything changed.
pub fn diff(
    slots: &[(u16, u16)],
    old: Option<&[i32; MAX_SLOTS]>,
    new: &[i32; MAX_SLOTS],
    out: &mut Vec<input_event>,
) {
    let start = out.len();
    for (i, &(t, c)) in slots.iter().enumerate() {
        if old.is_none_or(|o| o[i] != new[i]) {
            out.push(ev(t, c, new[i]));
        }
    }
    if out.len() > start {
        out.push(syn());
    }
}

pub type RumbleFn = Arc<dyn Fn(HostControl) + Send + Sync>;

/// Answers a pad's force-feedback requests and reports rumble until dropped.
pub struct FfThread {
    wake: OwnedFd,
    thread: Option<JoinHandle<()>>,
}

impl FfThread {
    pub fn start(dev: Arc<Device>, index: u8, rumble: RumbleFn) -> io::Result<Self> {
        // SAFETY: plain syscall; the fd is owned below.
        let raw = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a fresh fd we own.
        let wake = unsafe { OwnedFd::from_raw_fd(raw) };
        let stop = wake.as_raw_fd();
        let thread = std::thread::Builder::new()
            .name(format!("ws-input-ff{index}"))
            .spawn(move || ff_loop(&dev, stop, index, &*rumble))?;
        Ok(Self {
            wake,
            thread: Some(thread),
        })
    }
}

impl Drop for FfThread {
    fn drop(&mut self) {
        let one = 1u64;
        // SAFETY: writes 8 bytes to our eventfd.
        unsafe { libc::write(self.wake.as_raw_fd(), (&one as *const u64).cast(), 8) };
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// What the game asked the motors to do.
#[derive(Debug, Default)]
pub struct Motors {
    effects: [Option<(u16, u16, u16)>; FF_EFFECTS as usize],
    gain: u32,
    /// The effect playing and when it ends.
    playing: Option<(usize, Option<Instant>)>,
}

impl Motors {
    pub fn new() -> Self {
        Self {
            gain: 0xffff,
            ..Default::default()
        }
    }

    fn level(&self, m: u16) -> f32 {
        (u32::from(m) * self.gain / 0xffff) as f32 / 65535.0
    }

    /// The motor levels to report after `event`, if they change.
    pub fn apply(&mut self, code: u16, value: i32, now: Instant) -> Option<(f32, f32)> {
        if code == FF_GAIN {
            self.gain = value.clamp(0, 0xffff) as u32;
            return None;
        }
        let id = usize::from(code);
        let (strong, weak, length) = (*self.effects.get(id)?)?;
        if value > 0 {
            let end = (length > 0).then(|| now + Duration::from_millis(length.into()));
            self.playing = Some((id, end));
            Some((self.level(strong), self.level(weak)))
        } else if self.playing.is_some_and(|(p, _)| p == id) {
            self.playing = None;
            Some((0.0, 0.0))
        } else {
            None
        }
    }

    pub fn upload(&mut self, id: i16, effect: Option<(u16, u16, u16)>) -> i32 {
        match (usize::try_from(id).ok(), effect) {
            (Some(i), Some(e)) if i < self.effects.len() => {
                self.effects[i] = Some(e);
                0
            }
            _ => -libc::EINVAL,
        }
    }

    /// Forgets effect `id`; stops the motors if it was playing.
    pub fn erase(&mut self, id: usize) -> Option<(f32, f32)> {
        if let Some(e) = self.effects.get_mut(id) {
            *e = None;
        }
        if self.playing.is_some_and(|(p, _)| p == id) {
            self.playing = None;
            return Some((0.0, 0.0));
        }
        None
    }

    /// When the playing effect runs out.
    pub fn deadline(&self) -> Option<Instant> {
        self.playing.and_then(|(_, end)| end)
    }

    /// Stops an effect that ran out by `now`.
    pub fn expire(&mut self, now: Instant) -> Option<(f32, f32)> {
        if self.deadline().is_some_and(|end| now >= end) {
            self.playing = None;
            return Some((0.0, 0.0));
        }
        None
    }
}

fn ff_loop(dev: &Device, stop: i32, index: u8, rumble: &(dyn Fn(HostControl) + Send + Sync)) {
    let mut motors = Motors::new();
    let send = |(strong, weak): (f32, f32)| {
        rumble(HostControl::Rumble {
            index,
            strong,
            weak,
        })
    };
    let mut events = [ev(0, 0, 0); 16];
    loop {
        let timeout = motors.deadline().map_or(-1, |end| {
            end.saturating_duration_since(Instant::now())
                .as_millis()
                .min(10_000) as i32
        });
        let mut fds = [
            libc::pollfd {
                fd: dev.fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: stop,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: `fds` is a valid array of two pollfds.
        let r = unsafe { libc::poll(fds.as_mut_ptr(), 2, timeout) };
        if r < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            warn!(index, "rumble poll: {e}");
            break;
        }
        if fds[1].revents != 0 {
            break;
        }
        if let Some(m) = motors.expire(Instant::now()) {
            send(m);
        }
        if fds[0].revents & libc::POLLIN == 0 {
            if fds[0].revents != 0 {
                break;
            }
            continue;
        }
        let n = match dev.read(&mut events) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                debug!(index, "rumble read: {e}");
                break;
            }
        };
        for e in &events[..n] {
            match (e.type_, e.code) {
                (EV_UINPUT, UI_FF_UPLOAD) => {
                    let r = dev.ff_upload(e.value as u32, |effect| {
                        motors.upload(effect.id, rumble_of(effect))
                    });
                    if let Err(err) = r {
                        debug!(index, "rumble upload: {err}");
                    }
                }
                (EV_UINPUT, UI_FF_ERASE) => match dev.ff_erase(e.value as u32) {
                    Ok(id) => {
                        if let Some(m) = motors.erase(id as usize) {
                            send(m);
                        }
                    }
                    Err(err) => debug!(index, "rumble erase: {err}"),
                },
                (EV_FF, code) => {
                    if let Some(m) = motors.apply(code, e.value, Instant::now()) {
                        send(m);
                    }
                }
                _ => {}
            }
        }
    }
    if motors.playing.is_some() {
        send((0.0, 0.0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vals(kind: GamepadKind, s: GamepadState) -> [i32; MAX_SLOTS] {
        let mut out = [0; MAX_SLOTS];
        values(kind, &s, &mut out);
        out
    }

    fn slot(kind: GamepadKind, code: (u16, u16), s: GamepadState) -> i32 {
        let i = slots(kind).iter().position(|&c| c == code).unwrap();
        vals(kind, s)[i]
    }

    #[test]
    fn xbox_layout_follows_xpad() {
        let k = GamepadKind::Xbox360;
        let up = GamepadState {
            ly: 32767,
            ..Default::default()
        };
        assert_eq!(
            slot(k, (EV_ABS, ABS_Y), up),
            -32768,
            "xpad reports up as negative"
        );
        assert_eq!(slot(k, (EV_ABS, ABS_Y), GamepadState::default()), -1);
        let s = GamepadState {
            buttons: xbutton::A | xbutton::Y | xbutton::DPAD_LEFT | xbutton::DPAD_DOWN,
            rt: 200,
            ..Default::default()
        };
        assert_eq!(slot(k, (EV_KEY, BTN_SOUTH), s), 1);
        assert_eq!(slot(k, (EV_KEY, BTN_WEST), s), 1, "xpad's BTN_Y");
        assert_eq!(slot(k, (EV_KEY, BTN_NORTH), s), 0);
        assert_eq!(slot(k, (EV_ABS, ABS_HAT0X), s), -1);
        assert_eq!(slot(k, (EV_ABS, ABS_HAT0Y), s), 1);
        assert_eq!(slot(k, (EV_ABS, ABS_RZ), s), 200);
    }

    #[test]
    fn ds4_layout_follows_hid_playstation() {
        let k = GamepadKind::DualShock4;
        let neutral = vals(k, GamepadState::default());
        assert_eq!(&neutral[..4], &[128, 128, 128, 128]);
        let s = GamepadState {
            buttons: xbutton::X | xbutton::GUIDE,
            lx: i16::MIN,
            ly: i16::MIN,
            lt: 1,
            ..Default::default()
        };
        assert_eq!(slot(k, (EV_ABS, ABS_X), s), 0);
        assert_eq!(slot(k, (EV_ABS, ABS_Y), s), 255, "stick down is DS4 bottom");
        assert_eq!(slot(k, (EV_KEY, BTN_WEST), s), 1, "square");
        assert_eq!(slot(k, (EV_KEY, BTN_MODE), s), 1);
        assert_eq!(slot(k, (EV_KEY, BTN_TL2), s), 1);
        assert_eq!(slot(k, (EV_KEY, BTN_TR2), s), 0);
    }

    #[test]
    fn diff_writes_only_changes_and_one_report() {
        let k = GamepadKind::Xbox360;
        let a = vals(k, GamepadState::default());
        let b = vals(
            k,
            GamepadState {
                buttons: xbutton::B,
                ..Default::default()
            },
        );
        let mut out = Vec::new();
        diff(slots(k), Some(&a), &a, &mut out);
        assert!(out.is_empty());
        diff(slots(k), Some(&a), &b, &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(
            (out[0].type_, out[0].code, out[0].value),
            (EV_KEY, BTN_EAST, 1)
        );
        assert_eq!(out[1].type_, EV_SYN);
        out.clear();
        diff(slots(k), None, &a, &mut out);
        assert_eq!(out.len(), X360_SLOTS.len() + 1);
    }

    #[test]
    fn rumble_plays_scales_and_runs_out() {
        let mut m = Motors::new();
        let t0 = Instant::now();
        assert_eq!(m.upload(0, Some((0xffff, 0x8000, 100))), 0);
        assert_eq!(m.upload(99, Some((1, 1, 1))), -libc::EINVAL);
        assert_eq!(m.upload(1, None), -libc::EINVAL);
        let (s, w) = m.apply(0, 1, t0).unwrap();
        assert_eq!(s, 1.0);
        assert!((w - 0.5).abs() < 0.001);
        assert_eq!(m.expire(t0 + Duration::from_millis(50)), None);
        assert_eq!(m.expire(t0 + Duration::from_millis(100)), Some((0.0, 0.0)));
        m.apply(FF_GAIN, 0x8000, t0);
        let (s, _) = m.apply(0, 1, t0).unwrap();
        assert!((s - 0.5).abs() < 0.001);
        assert_eq!(m.apply(0, 0, t0), Some((0.0, 0.0)));
        assert_eq!(m.apply(0, 0, t0), None);
        m.apply(0, 1, t0);
        assert_eq!(m.erase(0), Some((0.0, 0.0)));
        assert_eq!(m.apply(0, 1, t0), None, "erased");
    }

    #[test]
    fn zero_length_rumble_runs_until_stopped() {
        let mut m = Motors::new();
        m.upload(2, Some((100, 100, 0)));
        m.apply(2, 1, Instant::now());
        assert_eq!(m.deadline(), None);
    }
}
