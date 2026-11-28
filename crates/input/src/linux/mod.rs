//! Linux injection: uinput keyboard, relative mouse, absolute pointer and virtual pads.

pub mod cursor;
mod keys;
mod pad;
mod uinput;

use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use libc::input_event;
use tracing::{debug, info, warn};
use ws_proto::{GamepadState, HostControl, MAX_GAMEPADS, MouseButton};

use crate::handler::{GamepadKind, InputSink, SinkError};
use crate::keymap::KeyTarget;
use crate::mouse::{DisplayRect, to_virtual_desk};
use keys::{KEY_DELETE, KEY_LAST, KEY_LEFTALT, KEY_LEFTCTRL, KEY_LEFTSHIFT, Stroke};
use pad::{FfThread, MAX_SLOTS, RumbleFn};
use uinput::*;

pub use keys::{linux_key, us_key};

/// How long a failed `/dev/uinput` open is remembered before trying again.
const RETRY: Duration = Duration::from_secs(30);
/// Wheel units per notch, as on Windows.
const NOTCH: i32 = 120;
/// Most events written at once while typing text, under the evdev client buffer of 64.
const TEXT_CHUNK: usize = 32;
/// Pause between text chunks so the compositor drains its buffer.
const TEXT_PAUSE: Duration = Duration::from_millis(4);

/// The desktop pixel the absolute pointer was last moved to, and when.
static LAST_ABSOLUTE: Mutex<Option<((i32, i32), Instant)>> = Mutex::new(None);

/// Where and when the absolute pointer was last moved.
pub(crate) fn last_absolute() -> Option<((i32, i32), Instant)> {
    *LAST_ABSOLUTE.lock().unwrap_or_else(|e| e.into_inner())
}

/// The desktop pixel `x`, `y` (0..=65535 across `display`) lands on.
fn desktop_pixel(x: u16, y: u16, display: DisplayRect) -> (i32, i32) {
    let axis = |n: u16, origin: i32, size: i32| {
        (origin as i64 + (n as i64 * (size.max(1) as i64 - 1)) / 65535) as i32
    };
    (
        axis(x, display.left, display.width),
        axis(y, display.top, display.height),
    )
}

/// The keyboard that Ctrl+Alt+Del is typed on.
static SAS_KEYBOARD: Mutex<Weak<Device>> = Mutex::new(Weak::new());

/// The span both absolute axes cover when nothing else is known.
const UNKNOWN: DisplayRect = DisplayRect {
    left: 0,
    top: 0,
    width: 65536,
    height: 65536,
};

struct Devices {
    keyboard: Arc<Device>,
    mouse: Device,
    pointer: Device,
}

impl Devices {
    fn create() -> std::io::Result<Self> {
        let all_keys: Vec<u16> = (1..=KEY_LAST).collect();
        let buttons = [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_SIDE, BTN_EXTRA];
        let keyboard = Device::create(&Spec {
            name: "web-stream keyboard",
            bustype: BUS_VIRTUAL,
            vendor: 0,
            product: 1,
            version: 1,
            keys: &all_keys,
            rels: &[],
            abs: &[],
            ff_effects: 0,
        })?;
        let mouse = Device::create(&Spec {
            name: "web-stream mouse",
            bustype: BUS_VIRTUAL,
            vendor: 0,
            product: 2,
            version: 1,
            keys: &buttons,
            rels: &[
                REL_X,
                REL_Y,
                REL_WHEEL,
                REL_HWHEEL,
                REL_WHEEL_HI_RES,
                REL_HWHEEL_HI_RES,
            ],
            abs: &[],
            ff_effects: 0,
        })?;
        let axis = |code| Abs {
            code,
            min: 0,
            max: 65535,
            fuzz: 0,
            flat: 0,
        };
        let pointer = Device::create(&Spec {
            name: "web-stream pointer",
            bustype: BUS_VIRTUAL,
            vendor: 0,
            product: 3,
            version: 1,
            keys: &buttons,
            rels: &[],
            abs: &[axis(ABS_X), axis(ABS_Y)],
            ff_effects: 0,
        })?;
        Ok(Self {
            keyboard: Arc::new(keyboard),
            mouse,
            pointer,
        })
    }
}

struct Pad {
    kind: GamepadKind,
    dev: Arc<Device>,
    last: Option<[i32; MAX_SLOTS]>,
    _ff: Option<FfThread>,
}

impl Drop for Pad {
    fn drop(&mut self) {
        self.dev.destroy();
    }
}

pub struct LinuxInput {
    display: DisplayRect,
    desktop: DisplayRect,
    devices: Option<Devices>,
    error: Option<(Instant, String)>,
    /// Wheel units not yet worth a notch, per axis.
    wheel: (i32, i32),
    pads: [Option<Pad>; MAX_GAMEPADS as usize],
    rumble: RumbleFn,
    buf: Vec<input_event>,
}

fn sink_err(what: &str, e: std::io::Error) -> SinkError {
    SinkError(format!("{what}: {e}"))
}

fn button_code(button: MouseButton) -> u16 {
    match button {
        MouseButton::Left => BTN_LEFT,
        MouseButton::Right => BTN_RIGHT,
        MouseButton::Middle => BTN_MIDDLE,
        MouseButton::Back => BTN_SIDE,
        MouseButton::Forward => BTN_EXTRA,
    }
}

/// Splits `acc + delta` wheel units into whole notches and the remainder.
fn notches(acc: i32, delta: i32) -> (i32, i32) {
    let total = acc.saturating_add(delta);
    (total / NOTCH, total % NOTCH)
}

/// Appends a key press or release and a report.
fn key_events(out: &mut Vec<input_event>, code: u16, down: bool) {
    out.push(ev(EV_KEY, code, i32::from(down)));
    out.push(syn());
}

/// Appends the events that type one stroke.
fn stroke_events(out: &mut Vec<input_event>, s: Stroke) {
    let (key, mods): (u16, &[u16]) = match s {
        Stroke::Key(k, false) => (k, &[]),
        Stroke::Key(k, true) => (k, &[KEY_LEFTSHIFT]),
        Stroke::CtrlShift(k) => (k, &[KEY_LEFTCTRL, KEY_LEFTSHIFT]),
    };
    for &m in mods {
        key_events(out, m, true);
    }
    key_events(out, key, true);
    key_events(out, key, false);
    for &m in mods.iter().rev() {
        key_events(out, m, false);
    }
}

/// The strokes that type `text`; CR LF and lone CR are one Enter, other controls are dropped.
fn text_strokes(text: &str) -> Vec<Stroke> {
    let mut out = Vec::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        let c = match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                '\n'
            }
            '\n' | '\t' => c,
            c if c.is_control() => continue,
            c => c,
        };
        keys::strokes(c, &mut out);
    }
    out
}

/// Types Ctrl+Alt+Del on the session's virtual keyboard.
pub fn send_ctrl_alt_del() -> Result<(), String> {
    let keyboard = SAS_KEYBOARD
        .lock()
        .map_err(|_| "keyboard lock poisoned".to_string())?
        .upgrade()
        .ok_or("no browser is connected with input")?;
    let mut events = Vec::with_capacity(12);
    for k in [KEY_LEFTCTRL, KEY_LEFTALT, KEY_DELETE] {
        key_events(&mut events, k, true);
    }
    for k in [KEY_DELETE, KEY_LEFTALT, KEY_LEFTCTRL] {
        key_events(&mut events, k, false);
    }
    keyboard
        .write(&events)
        .map_err(|e| format!("Ctrl+Alt+Del: {e}"))
}

impl LinuxInput {
    /// `rumble` is called from a pad's force-feedback thread whenever a game sets its motors.
    pub fn new(rumble: impl Fn(HostControl) + Send + Sync + 'static) -> Self {
        let mut me = Self {
            display: UNKNOWN,
            desktop: UNKNOWN,
            devices: None,
            error: None,
            wheel: (0, 0),
            pads: Default::default(),
            rumble: Arc::new(rumble),
            buf: Vec::with_capacity(64),
        };
        if let Err(e) = me.devices() {
            warn!("input injection unavailable: {e}");
        }
        me
    }

    /// Call when the streamed display changes; `desktop` is the box around every display, which
    /// the absolute pointer spans.
    pub fn set_display_rect(&mut self, display: DisplayRect, desktop: DisplayRect) {
        self.display = display;
        self.desktop = desktop;
    }

    fn open_failed(&self) -> Option<SinkError> {
        match &self.error {
            Some((at, msg)) if at.elapsed() < RETRY => Some(SinkError(msg.clone())),
            _ => None,
        }
    }

    fn devices(&mut self) -> Result<&Devices, SinkError> {
        self.ensure_devices()?;
        Ok(self.devices.as_ref().expect("just created"))
    }

    fn ensure_devices(&mut self) -> Result<(), SinkError> {
        if self.devices.is_none() {
            if let Some(e) = self.open_failed() {
                return Err(e);
            }
            match Devices::create() {
                Ok(d) => {
                    if let Ok(mut k) = SAS_KEYBOARD.lock() {
                        *k = Arc::downgrade(&d.keyboard);
                    }
                    info!("virtual keyboard and mouse created");
                    self.error = None;
                    self.devices = Some(d);
                }
                Err(e) => {
                    let msg = format!("cannot create virtual input devices: {e}");
                    self.error = Some((Instant::now(), msg.clone()));
                    return Err(SinkError(msg));
                }
            }
        }
        Ok(())
    }

    /// Writes `self.buf` to the device `pick` chooses, then clears it.
    fn flush(&mut self, pick: fn(&Devices) -> &Device) -> Result<(), SinkError> {
        let result = self.ensure_devices().and_then(|()| {
            let d = self.devices.as_ref().expect("just created");
            pick(d).write(&self.buf).map_err(|e| sink_err("uinput", e))
        });
        self.buf.clear();
        result
    }
}

impl InputSink for LinuxInput {
    fn text(&mut self, text: &str) -> Result<(), SinkError> {
        let mut events = Vec::new();
        for s in text_strokes(text) {
            stroke_events(&mut events, s);
        }
        let keyboard = self.devices()?.keyboard.clone();
        let mut chunks = events.chunks(TEXT_CHUNK).peekable();
        while let Some(chunk) = chunks.next() {
            keyboard.write(chunk).map_err(|e| sink_err("uinput", e))?;
            if chunks.peek().is_some() {
                std::thread::sleep(TEXT_PAUSE);
            }
        }
        Ok(())
    }

    fn key(&mut self, key: KeyTarget, down: bool) -> Result<(), SinkError> {
        let Some(code) = linux_key(key) else {
            debug!(?key, "no Linux key for this target");
            return Ok(());
        };
        key_events(&mut self.buf, code, down);
        self.flush(|d| &d.keyboard)
    }

    fn mouse_move(&mut self, dx: i32, dy: i32) -> Result<(), SinkError> {
        if dx != 0 {
            self.buf.push(ev(EV_REL, REL_X, dx));
        }
        if dy != 0 {
            self.buf.push(ev(EV_REL, REL_Y, dy));
        }
        if self.buf.is_empty() {
            return Ok(());
        }
        self.buf.push(syn());
        self.flush(|d| &d.mouse)
    }

    fn mouse_move_abs(&mut self, x: u16, y: u16) -> Result<(), SinkError> {
        let (nx, ny) = to_virtual_desk(x, y, self.display, self.desktop);
        self.buf.push(ev(EV_ABS, ABS_X, nx));
        self.buf.push(ev(EV_ABS, ABS_Y, ny));
        self.buf.push(syn());
        self.flush(|d| &d.pointer)?;
        if self.display != UNKNOWN {
            *LAST_ABSOLUTE.lock().unwrap_or_else(|e| e.into_inner()) =
                Some((desktop_pixel(x, y, self.display), Instant::now()));
        }
        Ok(())
    }

    fn mouse_button(&mut self, button: MouseButton, down: bool) -> Result<(), SinkError> {
        key_events(&mut self.buf, button_code(button), down);
        self.flush(|d| &d.mouse)
    }

    fn mouse_wheel(&mut self, dx: i32, dy: i32) -> Result<(), SinkError> {
        if dy != 0 {
            let (n, rest) = notches(self.wheel.1, dy);
            self.wheel.1 = rest;
            self.buf.push(ev(EV_REL, REL_WHEEL_HI_RES, dy));
            if n != 0 {
                self.buf.push(ev(EV_REL, REL_WHEEL, n));
            }
        }
        if dx != 0 {
            let (n, rest) = notches(self.wheel.0, dx);
            self.wheel.0 = rest;
            self.buf.push(ev(EV_REL, REL_HWHEEL_HI_RES, dx));
            if n != 0 {
                self.buf.push(ev(EV_REL, REL_HWHEEL, n));
            }
        }
        if self.buf.is_empty() {
            return Ok(());
        }
        self.buf.push(syn());
        self.flush(|d| &d.mouse)
    }

    fn gamepad_connect(&mut self, index: u8, kind: GamepadKind) -> Result<(), SinkError> {
        if let Some(e) = self.open_failed() {
            return Err(e);
        }
        let dev = match pad::create(kind) {
            Ok(d) => Arc::new(d),
            Err(e) => {
                let msg = format!("gamepad {index}: {e}");
                warn!("{msg}");
                self.error = Some((Instant::now(), msg.clone()));
                return Err(SinkError(msg));
            }
        };
        let ff = FfThread::start(dev.clone(), index, self.rumble.clone())
            .inspect_err(|e| warn!(index, "no rumble for this gamepad: {e}"))
            .ok();
        self.pads[index as usize] = Some(Pad {
            kind,
            dev,
            last: None,
            _ff: ff,
        });
        info!(index, ?kind, "virtual gamepad plugged in");
        Ok(())
    }

    fn gamepad_update(&mut self, index: u8, s: &GamepadState) -> Result<(), SinkError> {
        let Some(p) = &mut self.pads[index as usize] else {
            return Err(SinkError(format!("gamepad {index} is not plugged in")));
        };
        let mut now = [0; MAX_SLOTS];
        pad::values(p.kind, s, &mut now);
        pad::diff(pad::slots(p.kind), p.last.as_ref(), &now, &mut self.buf);
        p.last = Some(now);
        if self.buf.is_empty() {
            return Ok(());
        }
        let result = p
            .dev
            .write(&self.buf)
            .map_err(|e| SinkError(format!("gamepad {index}: {e}")));
        self.buf.clear();
        result
    }

    fn gamepad_disconnect(&mut self, index: u8) -> Result<(), SinkError> {
        if self.pads[index as usize].take().is_some() {
            info!(index, "virtual gamepad unplugged");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wheel_keeps_the_remainder_for_the_next_notch() {
        assert_eq!(notches(0, 120), (1, 0));
        assert_eq!(notches(0, 60), (0, 60));
        assert_eq!(notches(60, 60), (1, 0));
        assert_eq!(notches(0, -240), (-2, 0));
        assert_eq!(notches(30, -60), (0, -30));
    }

    #[test]
    fn text_line_breaks_and_controls() {
        let s = text_strokes("a\r\nb\rc\u{7}");
        let a = Stroke::Key(us_key('a').unwrap().0, false);
        let enter = Stroke::Key(keys::KEY_ENTER, false);
        assert_eq!(s[0], a);
        assert_eq!(s[1], enter);
        assert_eq!(s[3], enter);
        assert_eq!(s.len(), 5);
    }

    #[test]
    fn shifted_strokes_wrap_the_key_in_shift() {
        let mut out = Vec::new();
        stroke_events(&mut out, Stroke::Key(30, true));
        let keys: Vec<(u16, i32)> = out
            .iter()
            .filter(|e| e.type_ == EV_KEY)
            .map(|e| (e.code, e.value))
            .collect();
        assert_eq!(
            keys,
            [(KEY_LEFTSHIFT, 1), (30, 1), (30, 0), (KEY_LEFTSHIFT, 0)]
        );
        assert_eq!(out.len(), 8, "a report after every key");
    }

    #[test]
    fn back_and_forward_are_side_and_extra() {
        assert_eq!(button_code(MouseButton::Back), BTN_SIDE);
        assert_eq!(button_code(MouseButton::Forward), BTN_EXTRA);
    }

    #[test]
    fn unknown_display_maps_the_wire_range_straight_through() {
        assert_eq!(to_virtual_desk(0, 65535, UNKNOWN, UNKNOWN), (0, 65535));
        assert_eq!(
            to_virtual_desk(1234, 40000, UNKNOWN, UNKNOWN),
            (1234, 40000)
        );
    }
}
