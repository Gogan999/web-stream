//! Creates the real virtual devices and reads their events back through evdev; skipped without
//! `/dev/uinput`.

#![cfg(target_os = "linux")]

use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ws_input::{DisplayRect, GamepadKind, InputSink, LinuxInput, keymap};
use ws_proto::{GamepadState, HostControl, MouseButton};

const EV_KEY: u16 = 1;
const EV_REL: u16 = 2;
const EV_ABS: u16 = 3;
const EV_FF: u16 = 0x15;

fn uinput_available() -> bool {
    OpenOptions::new().write(true).open("/dev/uinput").is_ok()
}

/// The event node of the input device called `name`.
fn find(name: &str) -> Option<PathBuf> {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        for entry in std::fs::read_dir("/sys/class/input").ok()?.flatten() {
            let file = entry.file_name().to_string_lossy().into_owned();
            if !file.starts_with("event") {
                continue;
            }
            let dev_name =
                std::fs::read_to_string(entry.path().join("device/name")).unwrap_or_default();
            let node = PathBuf::from("/dev/input").join(&file);
            if dev_name.trim() == name && node.exists() {
                return Some(node);
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

fn open(name: &str) -> File {
    let node = find(name).unwrap_or_else(|| panic!("no event node for {name}"));
    OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(node)
        .expect("open event node")
}

/// Reads non-sync events until `want` of them arrived or a second passed.
fn read_events(f: &mut File, want: usize) -> Vec<(u16, u16, i32)> {
    let mut out = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(1);
    let size = size_of::<libc::input_event>();
    let mut buf = vec![0u8; size * 64];
    while out.len() < want && Instant::now() < deadline {
        match f.read(&mut buf) {
            Ok(n) => {
                for chunk in buf[..n].chunks_exact(size) {
                    // SAFETY: the kernel wrote whole input_events.
                    let e: libc::input_event =
                        unsafe { std::ptr::read_unaligned(chunk.as_ptr().cast()) };
                    if e.type_ != 0 {
                        out.push((e.type_, e.code, e.value));
                    }
                }
            }
            Err(_) => std::thread::sleep(Duration::from_millis(5)),
        }
    }
    out
}

fn upload_rumble(f: &File, strong: u16, weak: u16) -> i16 {
    // SAFETY: plain data.
    let mut effect: libc::ff_effect = unsafe { std::mem::zeroed() };
    effect.type_ = 0x50;
    effect.id = -1;
    effect.replay.length = 0;
    let r = libc::ff_rumble_effect {
        strong_magnitude: strong,
        weak_magnitude: weak,
    };
    // SAFETY: the union is larger than ff_rumble_effect.
    unsafe { std::ptr::write_unaligned(effect.u.as_mut_ptr().cast(), r) };
    let req = libc::_IOW::<libc::ff_effect>(b'E' as u32, 0x80);
    // SAFETY: EVIOCSFF takes a valid ff_effect.
    let rc = unsafe { libc::ioctl(f.as_raw_fd(), req, &mut effect) };
    assert_eq!(rc, 0, "EVIOCSFF: {}", std::io::Error::last_os_error());
    effect.id
}

fn play(f: &File, id: i16, value: i32) {
    // SAFETY: plain data.
    let mut e: libc::input_event = unsafe { std::mem::zeroed() };
    e.type_ = EV_FF;
    e.code = id as u16;
    e.value = value;
    // SAFETY: writes one input_event.
    let n = unsafe {
        libc::write(
            f.as_raw_fd(),
            (&e as *const libc::input_event).cast(),
            size_of::<libc::input_event>(),
        )
    };
    assert_eq!(n as usize, size_of::<libc::input_event>());
}

fn wait_for(rumbles: &Mutex<Vec<HostControl>>, n: usize) -> Vec<HostControl> {
    let deadline = Instant::now() + Duration::from_secs(2);
    while rumbles.lock().unwrap().len() < n && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    rumbles.lock().unwrap().clone()
}

#[test]
fn virtual_devices_deliver_what_the_sink_injects() {
    if !uinput_available() {
        eprintln!("skipped: /dev/uinput is not available");
        return;
    }
    let rumbles = Arc::new(Mutex::new(Vec::new()));
    let sink_rumbles = rumbles.clone();
    let mut sink = LinuxInput::new(move |m| sink_rumbles.lock().unwrap().push(m));
    let mut keyboard = open("web-stream keyboard");
    let mut mouse = open("web-stream mouse");
    let mut pointer = open("web-stream pointer");

    let a = keymap::lookup(0x04).unwrap();
    sink.key(a, true).unwrap();
    sink.key(a, false).unwrap();
    assert_eq!(
        read_events(&mut keyboard, 2),
        [(EV_KEY, 30, 1), (EV_KEY, 30, 0)]
    );
    sink.text("A").unwrap();
    assert_eq!(
        read_events(&mut keyboard, 4),
        [
            (EV_KEY, 42, 1),
            (EV_KEY, 30, 1),
            (EV_KEY, 30, 0),
            (EV_KEY, 42, 0)
        ]
    );

    sink.mouse_move(3, -4).unwrap();
    sink.mouse_button(MouseButton::Back, true).unwrap();
    sink.mouse_button(MouseButton::Back, false).unwrap();
    sink.mouse_wheel(0, 60).unwrap();
    sink.mouse_wheel(-120, 60).unwrap();
    assert_eq!(
        read_events(&mut mouse, 9),
        [
            (EV_REL, 0, 3),
            (EV_REL, 1, -4),
            (EV_KEY, 0x113, 1),
            (EV_KEY, 0x113, 0),
            (EV_REL, 0x0b, 60),
            (EV_REL, 0x0b, 60),
            (EV_REL, 0x08, 1),
            (EV_REL, 0x0c, -120),
            (EV_REL, 0x06, -1),
        ]
    );

    let right = DisplayRect {
        left: 1920,
        top: 0,
        width: 1920,
        height: 1080,
    };
    let desktop = DisplayRect {
        left: 0,
        top: 0,
        width: 3840,
        height: 1080,
    };
    sink.set_display_rect(right, desktop);
    sink.mouse_move_abs(0, 65535).unwrap();
    let got = read_events(&mut pointer, 2);
    assert_eq!(got, [(EV_ABS, 0, 1920 * 65535 / 3839), (EV_ABS, 1, 65535)]);

    sink.gamepad_connect(0, GamepadKind::Xbox360).unwrap();
    let mut pad = open("Microsoft X-Box 360 pad");
    sink.gamepad_update(
        0,
        &GamepadState {
            buttons: 0x1000,
            lx: 1000,
            ..Default::default()
        },
    )
    .unwrap();
    let got = read_events(&mut pad, 2);
    assert!(got.contains(&(EV_KEY, 0x130, 1)), "{got:?}");
    assert!(got.contains(&(EV_ABS, 0, 1000)), "{got:?}");

    let id = upload_rumble(&pad, 0xffff, 0x8000);
    play(&pad, id, 1);
    let seen = wait_for(&rumbles, 1);
    assert!(
        matches!(seen.first(), Some(HostControl::Rumble { index: 0, strong, weak })
            if *strong == 1.0 && (*weak - 0.5).abs() < 0.01),
        "{seen:?}"
    );
    play(&pad, id, 0);
    let seen = wait_for(&rumbles, 2);
    assert!(
        matches!(seen.get(1), Some(HostControl::Rumble { strong, weak, .. }) if *strong == 0.0 && *weak == 0.0),
        "{seen:?}"
    );

    sink.gamepad_connect(1, GamepadKind::DualShock4).unwrap();
    let mut ds4 = open("Sony Computer Entertainment Wireless Controller");
    sink.gamepad_update(
        1,
        &GamepadState {
            buttons: 0x4000,
            ..Default::default()
        },
    )
    .unwrap();
    let got = read_events(&mut ds4, 1);
    assert!(got.contains(&(EV_KEY, 0x134, 1)), "square: {got:?}");

    sink.gamepad_disconnect(0).unwrap();
    sink.gamepad_disconnect(1).unwrap();
    ws_input::send_ctrl_alt_del().unwrap();
    assert_eq!(read_events(&mut keyboard, 6).len(), 6);
}
