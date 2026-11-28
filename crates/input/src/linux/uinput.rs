//! Virtual input devices on `/dev/uinput`.

use std::ffi::CString;
use std::fs::OpenOptions;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;

use libc::{
    _IO, _IOW, _IOWR, c_int, ff_effect, input_absinfo, input_event, input_id, uinput_abs_setup,
    uinput_ff_erase, uinput_ff_upload, uinput_setup,
};

pub const EV_SYN: u16 = 0x00;
pub const EV_KEY: u16 = 0x01;
pub const EV_REL: u16 = 0x02;
pub const EV_ABS: u16 = 0x03;
pub const EV_FF: u16 = 0x15;
pub const EV_UINPUT: u16 = 0x0101;
pub const SYN_REPORT: u16 = 0;

pub const REL_X: u16 = 0x00;
pub const REL_Y: u16 = 0x01;
pub const REL_HWHEEL: u16 = 0x06;
pub const REL_WHEEL: u16 = 0x08;
pub const REL_WHEEL_HI_RES: u16 = 0x0b;
pub const REL_HWHEEL_HI_RES: u16 = 0x0c;

pub const ABS_X: u16 = 0x00;
pub const ABS_Y: u16 = 0x01;
pub const ABS_Z: u16 = 0x02;
pub const ABS_RX: u16 = 0x03;
pub const ABS_RY: u16 = 0x04;
pub const ABS_RZ: u16 = 0x05;
pub const ABS_HAT0X: u16 = 0x10;
pub const ABS_HAT0Y: u16 = 0x11;

pub const BTN_LEFT: u16 = 0x110;
pub const BTN_RIGHT: u16 = 0x111;
pub const BTN_MIDDLE: u16 = 0x112;
pub const BTN_SIDE: u16 = 0x113;
pub const BTN_EXTRA: u16 = 0x114;
pub const BTN_SOUTH: u16 = 0x130;
pub const BTN_EAST: u16 = 0x131;
pub const BTN_NORTH: u16 = 0x133;
pub const BTN_WEST: u16 = 0x134;
pub const BTN_TL: u16 = 0x136;
pub const BTN_TR: u16 = 0x137;
pub const BTN_TL2: u16 = 0x138;
pub const BTN_TR2: u16 = 0x139;
pub const BTN_SELECT: u16 = 0x13a;
pub const BTN_START: u16 = 0x13b;
pub const BTN_MODE: u16 = 0x13c;
pub const BTN_THUMBL: u16 = 0x13d;
pub const BTN_THUMBR: u16 = 0x13e;

pub const FF_RUMBLE: u16 = 0x50;
pub const FF_GAIN: u16 = 0x60;
pub const UI_FF_UPLOAD: u16 = 1;
pub const UI_FF_ERASE: u16 = 2;

pub const BUS_USB: u16 = 0x03;
pub const BUS_VIRTUAL: u16 = 0x06;

const U: u32 = b'U' as u32;
const UI_DEV_CREATE: libc::Ioctl = _IO(U, 1);
const UI_DEV_DESTROY: libc::Ioctl = _IO(U, 2);
const UI_DEV_SETUP: libc::Ioctl = _IOW::<uinput_setup>(U, 3);
const UI_ABS_SETUP: libc::Ioctl = _IOW::<uinput_abs_setup>(U, 4);
const UI_SET_EVBIT: libc::Ioctl = _IOW::<c_int>(U, 100);
const UI_SET_KEYBIT: libc::Ioctl = _IOW::<c_int>(U, 101);
const UI_SET_RELBIT: libc::Ioctl = _IOW::<c_int>(U, 102);
const UI_SET_ABSBIT: libc::Ioctl = _IOW::<c_int>(U, 103);
const UI_SET_FFBIT: libc::Ioctl = _IOW::<c_int>(U, 107);
const UI_BEGIN_FF_UPLOAD: libc::Ioctl = _IOWR::<uinput_ff_upload>(U, 200);
const UI_END_FF_UPLOAD: libc::Ioctl = _IOW::<uinput_ff_upload>(U, 201);
const UI_BEGIN_FF_ERASE: libc::Ioctl = _IOWR::<uinput_ff_erase>(U, 202);
const UI_END_FF_ERASE: libc::Ioctl = _IOW::<uinput_ff_erase>(U, 203);

/// An absolute axis and its range.
#[derive(Debug, Clone, Copy)]
pub struct Abs {
    pub code: u16,
    pub min: i32,
    pub max: i32,
    pub fuzz: i32,
    pub flat: i32,
}

/// What a virtual device reports.
#[derive(Debug, Clone, Copy)]
pub struct Spec<'a> {
    pub name: &'a str,
    pub bustype: u16,
    pub vendor: u16,
    pub product: u16,
    pub version: u16,
    pub keys: &'a [u16],
    pub rels: &'a [u16],
    pub abs: &'a [Abs],
    /// Rumble effects a game may upload at once; 0 for none.
    pub ff_effects: u32,
}

/// One created uinput device; destroyed on drop.
#[derive(Debug)]
pub struct Device {
    fd: OwnedFd,
}

fn check(r: c_int) -> io::Result<c_int> {
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

/// An `input_event` with a zero timestamp, which the kernel fills in.
pub fn ev(type_: u16, code: u16, value: i32) -> input_event {
    // SAFETY: input_event is plain data; all zeroes is a valid value.
    let mut e: input_event = unsafe { std::mem::zeroed() };
    e.type_ = type_;
    e.code = code;
    e.value = value;
    e
}

pub fn syn() -> input_event {
    ev(EV_SYN, SYN_REPORT, 0)
}

impl Device {
    pub fn create(spec: &Spec) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_CLOEXEC)
            .open("/dev/uinput")
            .map_err(|e| match e.kind() {
                io::ErrorKind::NotFound => io::Error::new(
                    e.kind(),
                    "/dev/uinput is missing: load the uinput kernel module (modprobe uinput)",
                ),
                _ => io::Error::new(e.kind(), format!("/dev/uinput: {e}")),
            })?;
        let dev = Self { fd: file.into() };
        let fd = dev.fd.as_raw_fd();
        let set = |req, v: u16| -> io::Result<()> {
            // SAFETY: the bit ioctls take an int by value.
            check(unsafe { libc::ioctl(fd, req, c_int::from(v)) }).map(drop)
        };
        if !spec.keys.is_empty() {
            set(UI_SET_EVBIT, EV_KEY)?;
            for &k in spec.keys {
                set(UI_SET_KEYBIT, k)?;
            }
        }
        if !spec.rels.is_empty() {
            set(UI_SET_EVBIT, EV_REL)?;
            for &r in spec.rels {
                set(UI_SET_RELBIT, r)?;
            }
        }
        if !spec.abs.is_empty() {
            set(UI_SET_EVBIT, EV_ABS)?;
            for a in spec.abs {
                set(UI_SET_ABSBIT, a.code)?;
                let setup = uinput_abs_setup {
                    code: a.code,
                    absinfo: input_absinfo {
                        value: 0,
                        minimum: a.min,
                        maximum: a.max,
                        fuzz: a.fuzz,
                        flat: a.flat,
                        resolution: 0,
                    },
                };
                // SAFETY: `setup` is a valid uinput_abs_setup for the call.
                check(unsafe { libc::ioctl(fd, UI_ABS_SETUP, &setup) })?;
            }
        }
        if spec.ff_effects > 0 {
            set(UI_SET_EVBIT, EV_FF)?;
            set(UI_SET_FFBIT, FF_RUMBLE)?;
            set(UI_SET_FFBIT, FF_GAIN)?;
        }
        // SAFETY: uinput_setup is plain data.
        let mut setup: uinput_setup = unsafe { std::mem::zeroed() };
        setup.id = input_id {
            bustype: spec.bustype,
            vendor: spec.vendor,
            product: spec.product,
            version: spec.version,
        };
        let name = CString::new(spec.name).unwrap_or_default();
        for (d, s) in setup.name.iter_mut().zip(name.as_bytes().iter().take(79)) {
            *d = *s as libc::c_char;
        }
        setup.ff_effects_max = spec.ff_effects;
        // SAFETY: `setup` is a valid uinput_setup for the call.
        check(unsafe { libc::ioctl(fd, UI_DEV_SETUP, &setup) })?;
        // SAFETY: no arguments.
        check(unsafe { libc::ioctl(fd, UI_DEV_CREATE) })?;
        Ok(dev)
    }

    pub fn fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Writes `events` in one call.
    pub fn write(&self, events: &[input_event]) -> io::Result<()> {
        let len = std::mem::size_of_val(events);
        // SAFETY: `events` is a valid slice of `len` bytes.
        let n = unsafe { libc::write(self.fd(), events.as_ptr().cast(), len) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if n as usize != len {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "short uinput write",
            ));
        }
        Ok(())
    }

    /// Unregisters the device now; the fd stays open until drop.
    pub fn destroy(&self) {
        // SAFETY: no arguments; harmless on a destroyed device.
        unsafe { libc::ioctl(self.fd(), UI_DEV_DESTROY) };
    }

    /// Reads the events the kernel queued for this device (force feedback requests).
    pub fn read(&self, out: &mut [input_event]) -> io::Result<usize> {
        // SAFETY: `out` is writable for its whole length.
        let n = unsafe { libc::read(self.fd(), out.as_mut_ptr().cast(), size_of_val(out)) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(n as usize / size_of::<input_event>())
    }

    /// Takes the effect of upload request `id`, answers it with `retval` from `accept`.
    pub fn ff_upload(&self, id: u32, accept: impl FnOnce(&ff_effect) -> i32) -> io::Result<()> {
        // SAFETY: uinput_ff_upload is plain data.
        let mut up: uinput_ff_upload = unsafe { std::mem::zeroed() };
        up.request_id = id;
        // SAFETY: `up` is a valid uinput_ff_upload for both calls.
        unsafe {
            check(libc::ioctl(self.fd(), UI_BEGIN_FF_UPLOAD, &mut up))?;
            up.retval = accept(&up.effect);
            check(libc::ioctl(self.fd(), UI_END_FF_UPLOAD, &up))?;
        }
        Ok(())
    }

    /// Answers erase request `id`; returns the effect id erased.
    pub fn ff_erase(&self, id: u32) -> io::Result<u32> {
        // SAFETY: uinput_ff_erase is plain data.
        let mut er: uinput_ff_erase = unsafe { std::mem::zeroed() };
        er.request_id = id;
        // SAFETY: `er` is a valid uinput_ff_erase for both calls.
        unsafe {
            check(libc::ioctl(self.fd(), UI_BEGIN_FF_ERASE, &mut er))?;
            er.retval = 0;
            check(libc::ioctl(self.fd(), UI_END_FF_ERASE, &er))?;
        }
        Ok(er.effect_id)
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        self.destroy();
    }
}

/// Strong and weak magnitudes of a rumble effect.
pub fn rumble_of(effect: &ff_effect) -> Option<(u16, u16, u16)> {
    if effect.type_ != FF_RUMBLE {
        return None;
    }
    // SAFETY: for FF_RUMBLE the union starts with ff_rumble_effect.
    let r: libc::ff_rumble_effect = unsafe { std::ptr::read_unaligned(effect.u.as_ptr().cast()) };
    Some((r.strong_magnitude, r.weak_magnitude, effect.replay.length))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_numbers_match_the_kernel_headers() {
        assert_eq!(UI_DEV_CREATE as u32, 0x5501);
        assert_eq!(UI_DEV_DESTROY as u32, 0x5502);
        assert_eq!(UI_DEV_SETUP as u32, 0x405c_5503);
        assert_eq!(UI_ABS_SETUP as u32, 0x401c_5504);
        assert_eq!(UI_SET_EVBIT as u32, 0x4004_5564);
        assert_eq!(UI_SET_FFBIT as u32, 0x4004_556b);
        assert_eq!(UI_BEGIN_FF_ERASE as u32, 0xc00c_55ca);
        #[cfg(target_pointer_width = "64")]
        assert_eq!(UI_BEGIN_FF_UPLOAD as u32, 0xc068_55c8);
    }

    #[test]
    fn rumble_magnitudes_are_read_from_the_union() {
        // SAFETY: plain data.
        let mut e: ff_effect = unsafe { std::mem::zeroed() };
        e.type_ = FF_RUMBLE;
        e.replay.length = 250;
        let r = libc::ff_rumble_effect {
            strong_magnitude: 0x8000,
            weak_magnitude: 0x1234,
        };
        // SAFETY: the union is larger than ff_rumble_effect.
        unsafe { std::ptr::write_unaligned(e.u.as_mut_ptr().cast(), r) };
        assert_eq!(rumble_of(&e), Some((0x8000, 0x1234, 250)));
        e.type_ = 0x51;
        assert_eq!(rumble_of(&e), None);
    }
}
