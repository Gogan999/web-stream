//! Windows injection: `SendInput` for keyboard and mouse, ViGEmBus for
//! virtual Xbox 360 and DualShock 4 controllers.

use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Once};
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};
use vigem_client::{
    Client, DS4Report, DualShock4Wired, TargetId, XButtons, XGamepad, Xbox360Wired,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MAPVK_VK_TO_VSC, MOUSE_EVENT_FLAGS,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
    MOUSEEVENTF_XUP, MOUSEINPUT, MapVirtualKeyW, SendInput, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXSCREEN, SM_CXVIRTUALSCREEN, SM_CYSCREEN, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, XBUTTON1, XBUTTON2,
};

use ws_proto::{GamepadState, HostControl, MAX_GAMEPADS, MouseButton};

use crate::gamepad::Ds4Report;
use crate::handler::{GamepadKind, InputSink, SinkError};
use crate::keymap::KeyTarget;
use crate::mouse::{DisplayRect, to_virtual_desk};

impl DisplayRect {
    pub fn primary() -> Self {
        // SAFETY: GetSystemMetrics has no preconditions.
        let (width, height) =
            unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
        Self {
            left: 0,
            top: 0,
            width,
            height,
        }
    }

    fn virtual_desktop() -> Self {
        // SAFETY: GetSystemMetrics has no preconditions.
        unsafe {
            Self {
                left: GetSystemMetrics(SM_XVIRTUALSCREEN),
                top: GetSystemMetrics(SM_YVIRTUALSCREEN),
                width: GetSystemMetrics(SM_CXVIRTUALSCREEN),
                height: GetSystemMetrics(SM_CYVIRTUALSCREEN),
            }
        }
    }
}

/// Makes the process per-monitor DPI aware (v2), so system metrics are in
/// the same physical pixels as DXGI's display coordinates. Fails harmlessly
/// when awareness was already set, e.g. by the manifest or another crate.
pub(crate) fn ensure_dpi_aware() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: no pointer arguments.
        if let Err(e) =
            unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }
        {
            debug!("DPI awareness not changed: {e}");
        }
    });
}

type RumbleFn = Arc<dyn Fn(HostControl) + Send + Sync>;

/// How long a failed ViGEmBus open is remembered before trying again.
const VIGEM_RETRY: Duration = Duration::from_secs(30);

enum Pad {
    X360(Xbox360Wired<Arc<Client>>),
    Ds4(DualShock4Wired<Arc<Client>>),
}

enum Slot {
    /// Plug-in runs on its own thread because `wait_ready` blocks. Updates
    /// before it is ready are dropped; the client resends state every 100 ms.
    Pending {
        ready: Receiver<Result<Pad, SinkError>>,
    },
    Ready(Pad),
}

pub struct WindowsInput {
    display: DisplayRect,
    vigem: Option<Arc<Client>>,
    vigem_error: Option<(Instant, String)>,
    pads: [Option<Slot>; MAX_GAMEPADS as usize],
    rumble: RumbleFn,
}

fn send(inputs: &[INPUT]) -> Result<(), SinkError> {
    let mut sent = send_once(inputs);
    // The lock screen or a UAC prompt came up, or went away: input goes to
    // the desktop showing now. Only works when running as SYSTEM. Retried
    // only when nothing went in, so a partial send isn't doubled.
    if sent == 0 && ws_desktop::follow_input_desktop() {
        sent = send_once(inputs);
    }
    if sent as usize != inputs.len() {
        // Usually UIPI: the foreground window belongs to an elevated process,
        // or the secure desktop (UAC, lock screen) is showing. The host
        // service (running as SYSTEM) gets past both.
        return Err(SinkError(format!(
            "SendInput blocked: {}",
            windows::core::Error::from_thread()
        )));
    }
    Ok(())
}

fn send_once(inputs: &[INPUT]) -> u32 {
    // SAFETY: `inputs` is a valid slice of fully initialised INPUT structs.
    unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) }
}

fn mouse(dx: i32, dy: i32, data: u32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn keyboard(vk: u16, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

impl WindowsInput {
    /// `rumble` is called from a ViGEm notification thread whenever a game
    /// sets an Xbox controller's motors, with a [`HostControl::Rumble`] to
    /// send on the `control` channel.
    pub fn new(rumble: impl Fn(HostControl) + Send + Sync + 'static) -> Self {
        ensure_dpi_aware();
        Self {
            display: DisplayRect::primary(),
            vigem: None,
            vigem_error: None,
            pads: Default::default(),
            rumble: Arc::new(rumble),
        }
    }

    /// Call when the streamed display changes so absolute mouse positions land
    /// on the display the client is looking at.
    pub fn set_display_rect(&mut self, display: DisplayRect) {
        self.display = display;
    }

    fn vigem(&mut self) -> Result<Arc<Client>, SinkError> {
        if let Some(c) = &self.vigem {
            return Ok(c.clone());
        }
        if let Some((at, msg)) = &self.vigem_error {
            if at.elapsed() < VIGEM_RETRY {
                return Err(SinkError(msg.clone()));
            }
        }
        match Client::connect() {
            Ok(client) => {
                let client = Arc::new(client);
                self.vigem = Some(client.clone());
                self.vigem_error = None;
                Ok(client)
            }
            Err(e) => {
                let msg = match e {
                    vigem_client::Error::BusNotFound => "ViGEmBus driver is not installed, gamepads are unavailable (see docs/host-setup.md)".to_string(),
                    e => format!("cannot open ViGEmBus: {e}"),
                };
                warn!("{msg}");
                self.vigem_error = Some((Instant::now(), msg.clone()));
                Err(SinkError(msg))
            }
        }
    }
}

fn plug_in(
    client: Arc<Client>,
    index: u8,
    kind: GamepadKind,
    rumble: RumbleFn,
) -> Result<Pad, SinkError> {
    let err = |e: vigem_client::Error| SinkError(format!("gamepad {index}: {e}"));
    let pad = match kind {
        GamepadKind::Xbox360 => {
            let mut t = Xbox360Wired::new(client, TargetId::XBOX360_WIRED);
            t.plugin().map_err(err)?;
            t.wait_ready().map_err(err)?;
            match t.request_notification() {
                Ok(req) => {
                    // The thread ends by itself when the target is unplugged.
                    req.spawn_thread(move |_, n| {
                        rumble(HostControl::Rumble {
                            index,
                            strong: n.large_motor as f32 / 255.0,
                            weak: n.small_motor as f32 / 255.0,
                        })
                    });
                }
                Err(e) => warn!(index, "no rumble for this gamepad: {e}"),
            }
            Pad::X360(t)
        }
        GamepadKind::DualShock4 => {
            let mut t = DualShock4Wired::new(client, TargetId::DUALSHOCK4_WIRED);
            t.plugin().map_err(err)?;
            t.wait_ready().map_err(err)?;
            Pad::Ds4(t)
        }
    };
    info!(index, ?kind, "virtual gamepad plugged in");
    Ok(pad)
}

fn update_pad(pad: &mut Pad, index: u8, s: &GamepadState) -> Result<(), SinkError> {
    let result = match pad {
        Pad::X360(t) => t.update(&XGamepad {
            buttons: XButtons(s.buttons),
            left_trigger: s.lt,
            right_trigger: s.rt,
            thumb_lx: s.lx,
            thumb_ly: s.ly,
            thumb_rx: s.rx,
            thumb_ry: s.ry,
        }),
        Pad::Ds4(t) => {
            let r = Ds4Report::from(s);
            t.update(&DS4Report {
                thumb_lx: r.thumb_lx,
                thumb_ly: r.thumb_ly,
                thumb_rx: r.thumb_rx,
                thumb_ry: r.thumb_ry,
                buttons: r.buttons,
                special: r.special,
                trigger_l: r.trigger_l,
                trigger_r: r.trigger_r,
            })
        }
    };
    result.map_err(|e| SinkError(format!("gamepad {index}: {e}")))
}

impl InputSink for WindowsInput {
    fn key(&mut self, key: KeyTarget, down: bool) -> Result<(), SinkError> {
        let up = if down {
            KEYBD_EVENT_FLAGS(0)
        } else {
            KEYEVENTF_KEYUP
        };
        let input = match key {
            KeyTarget::Scan { code, extended } => {
                let ext = if extended {
                    KEYEVENTF_EXTENDEDKEY
                } else {
                    KEYBD_EVENT_FLAGS(0)
                };
                keyboard(0, code, KEYEVENTF_SCANCODE | ext | up)
            }
            KeyTarget::Vk(vk) => {
                // SAFETY: MapVirtualKeyW has no preconditions.
                let scan = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) } as u16;
                // Media and browser keys are E0-prefixed on real keyboards.
                let ext = if (0xa6..=0xb7).contains(&vk) {
                    KEYEVENTF_EXTENDEDKEY
                } else {
                    KEYBD_EVENT_FLAGS(0)
                };
                keyboard(vk, scan, ext | up)
            }
        };
        send(&[input])
    }

    fn mouse_move(&mut self, dx: i32, dy: i32) -> Result<(), SinkError> {
        send(&[mouse(dx, dy, 0, MOUSEEVENTF_MOVE)])
    }

    fn mouse_move_abs(&mut self, x: u16, y: u16) -> Result<(), SinkError> {
        let (nx, ny) = to_virtual_desk(x, y, self.display, DisplayRect::virtual_desktop());
        send(&[mouse(
            nx,
            ny,
            0,
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        )])
    }

    fn mouse_button(&mut self, button: MouseButton, down: bool) -> Result<(), SinkError> {
        let (flags, data) = match (button, down) {
            (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
            (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
            (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
            (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
            (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
            (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
            (MouseButton::Back, true) => (MOUSEEVENTF_XDOWN, XBUTTON1),
            (MouseButton::Back, false) => (MOUSEEVENTF_XUP, XBUTTON1),
            (MouseButton::Forward, true) => (MOUSEEVENTF_XDOWN, XBUTTON2),
            (MouseButton::Forward, false) => (MOUSEEVENTF_XUP, XBUTTON2),
        };
        send(&[mouse(0, 0, data as u32, flags)])
    }

    fn mouse_wheel(&mut self, dx: i32, dy: i32) -> Result<(), SinkError> {
        let mut inputs = Vec::with_capacity(2);
        if dy != 0 {
            inputs.push(mouse(0, 0, dy as u32, MOUSEEVENTF_WHEEL));
        }
        if dx != 0 {
            inputs.push(mouse(0, 0, dx as u32, MOUSEEVENTF_HWHEEL));
        }
        if inputs.is_empty() {
            return Ok(());
        }
        send(&inputs)
    }

    fn gamepad_connect(&mut self, index: u8, kind: GamepadKind) -> Result<(), SinkError> {
        let client = self.vigem()?;
        let rumble = self.rumble.clone();
        let (tx, ready) = mpsc::channel();
        std::thread::Builder::new()
            .name(format!("ws-input-pad{index}"))
            .spawn(move || {
                // If the slot was dropped meanwhile, the returned pad is dropped
                // here, which unplugs it.
                let _ = tx.send(plug_in(client, index, kind, rumble));
            })
            .map_err(|e| SinkError(format!("gamepad {index}: {e}")))?;
        self.pads[index as usize] = Some(Slot::Pending { ready });
        Ok(())
    }

    fn gamepad_update(&mut self, index: u8, s: &GamepadState) -> Result<(), SinkError> {
        let slot = &mut self.pads[index as usize];
        if let Some(Slot::Pending { ready }) = slot {
            match ready.try_recv() {
                Err(TryRecvError::Empty) => return Ok(()),
                Ok(Ok(pad)) => *slot = Some(Slot::Ready(pad)),
                Ok(Err(e)) => {
                    *slot = None;
                    return Err(e);
                }
                Err(TryRecvError::Disconnected) => {
                    *slot = None;
                    return Err(SinkError(format!("gamepad {index}: plug-in thread died")));
                }
            }
        }
        match slot {
            Some(Slot::Ready(pad)) => update_pad(pad, index, s),
            _ => Err(SinkError(format!("gamepad {index} is not plugged in"))),
        }
    }

    fn gamepad_disconnect(&mut self, index: u8) -> Result<(), SinkError> {
        let result = match self.pads[index as usize].take() {
            None | Some(Slot::Pending { .. }) => return Ok(()),
            Some(Slot::Ready(Pad::X360(mut t))) => t.unplug(),
            Some(Slot::Ready(Pad::Ds4(mut t))) => t.unplug(),
        };
        info!(index, "virtual gamepad unplugged");
        result.map_err(|e| SinkError(format!("gamepad {index}: {e}")))
    }
}
