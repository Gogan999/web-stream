//! The helper's Wayland side: the ext or wlr data-control protocol.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

use wayland_client::Connection;

use super::MAX_BYTES;
use super::linux::{
    SECRET_HINT, SET, TEXT_TYPES, WRITTEN, is_secret, read_frame, report, write_frame,
};

/// Why the Wayland side can't be used, so the helper falls back to X11.
pub struct Unavailable(pub String);

/// How long the program that copied gets to hand the text over.
const RECEIVE_TIMEOUT: Duration = Duration::from_secs(2);

/// Waits until `fd` is readable, or `timeout` passes; `Some(i)` for each ready fd index.
pub fn poll_in(fds: &[i32], timeout: Option<Duration>) -> io::Result<Vec<bool>> {
    let mut pfds: Vec<libc::pollfd> = fds
        .iter()
        .map(|&fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();
    let ms = timeout.map_or(-1, |t| t.as_millis().min(i32::MAX as u128) as i32);
    loop {
        // SAFETY: `pfds` is a valid array of pollfds.
        let r = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, ms) };
        if r >= 0 {
            return Ok(pfds.iter().map(|p| p.revents != 0).collect());
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// Reads what the source writes into `fd`, giving up after [`RECEIVE_TIMEOUT`]; `Ok(None)` when
/// over [`MAX_BYTES`].
/// A close-on-exec pipe as (read end, write end).
fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: `fds` has room for both ends, which we then own.
    unsafe {
        if libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])))
    }
}

fn read_pipe(fd: OwnedFd) -> io::Result<Option<Vec<u8>>> {
    let deadline = Instant::now() + RECEIVE_TIMEOUT;
    let mut file = std::fs::File::from(fd);
    let mut out = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || !poll_in(&[file.as_raw_fd()], Some(left))?[0] {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "clipboard owner too slow",
            ));
        }
        let n = file.read(&mut buf)?;
        if n == 0 {
            return Ok(Some(out));
        }
        out.extend_from_slice(&buf[..n]);
        if out.len() > MAX_BYTES {
            return Ok(None);
        }
    }
}

macro_rules! data_control {
    ($name:ident, $proto:path, $manager:ident::$M:ident, $device:ident::$D:ident, $offer:ident::$O:ident, $source:ident::$S:ident) => {
        mod $name {
            use std::sync::Arc;

            use wayland_client::backend::ObjectId;
            use wayland_client::globals::{GlobalListContents, registry_queue_init};
            use wayland_client::protocol::{wl_registry, wl_seat::WlSeat};
            use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
            use $proto::{$device, $manager, $offer, $source};

            use super::*;

            type Manager = $manager::$M;
            type Device = $device::$D;
            type Offer = $offer::$O;
            type Source = $source::$S;

            #[derive(Default)]
            pub struct State {
                /// MIME types of each live offer.
                offers: HashMap<ObjectId, Vec<String>>,
                selection: Option<Offer>,
                changed: bool,
                finished: bool,
                /// The source we put on the clipboard and its text.
                own: Option<(Source, Arc<Vec<u8>>)>,
            }

            impl State {
                fn drop_offer(&mut self, offer: Offer) {
                    self.offers.remove(&offer.id());
                    offer.destroy();
                }
            }

            impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
                fn event(
                    _: &mut Self,
                    _: &wl_registry::WlRegistry,
                    _: wl_registry::Event,
                    _: &GlobalListContents,
                    _: &Connection,
                    _: &QueueHandle<Self>,
                ) {
                }
            }

            impl Dispatch<WlSeat, ()> for State {
                fn event(
                    _: &mut Self,
                    _: &WlSeat,
                    _: <WlSeat as Proxy>::Event,
                    _: &(),
                    _: &Connection,
                    _: &QueueHandle<Self>,
                ) {
                }
            }

            impl Dispatch<Manager, ()> for State {
                fn event(
                    _: &mut Self,
                    _: &Manager,
                    _: <Manager as Proxy>::Event,
                    _: &(),
                    _: &Connection,
                    _: &QueueHandle<Self>,
                ) {
                }
            }

            impl Dispatch<Device, ()> for State {
                fn event(
                    state: &mut Self,
                    _: &Device,
                    event: <Device as Proxy>::Event,
                    _: &(),
                    _: &Connection,
                    _: &QueueHandle<Self>,
                ) {
                    match event {
                        $device::Event::DataOffer { id } => {
                            state.offers.insert(id.id(), Vec::new());
                        }
                        $device::Event::Selection { id } => {
                            if let Some(old) = state.selection.take() {
                                state.drop_offer(old);
                            }
                            state.selection = id;
                            state.changed = true;
                        }
                        $device::Event::PrimarySelection { id: Some(offer) } => {
                            state.drop_offer(offer)
                        }
                        $device::Event::Finished => state.finished = true,
                        _ => {}
                    }
                }

                wayland_client::event_created_child!(State, Device, [
                    $device::EVT_DATA_OFFER_OPCODE => (Offer, ()),
                ]);
            }

            impl Dispatch<Offer, ()> for State {
                fn event(
                    state: &mut Self,
                    offer: &Offer,
                    event: <Offer as Proxy>::Event,
                    _: &(),
                    _: &Connection,
                    _: &QueueHandle<Self>,
                ) {
                    if let $offer::Event::Offer { mime_type } = event {
                        if let Some(types) = state.offers.get_mut(&offer.id()) {
                            types.push(mime_type);
                        }
                    }
                }
            }

            impl Dispatch<Source, ()> for State {
                fn event(
                    state: &mut Self,
                    source: &Source,
                    event: <Source as Proxy>::Event,
                    _: &(),
                    _: &Connection,
                    _: &QueueHandle<Self>,
                ) {
                    match event {
                        $source::Event::Send { fd, .. } => {
                            let Some((_, text)) = state.own.as_ref().filter(|(s, _)| s == source)
                            else {
                                return;
                            };
                            let text = text.clone();
                            let _ = std::thread::Builder::new()
                                .name("ws-clipboard-send".into())
                                .spawn(move || {
                                    let _ = std::fs::File::from(fd).write_all(&text);
                                });
                        }
                        $source::Event::Cancelled => {
                            if state.own.as_ref().is_some_and(|(s, _)| s == source) {
                                state.own = None;
                            }
                            source.destroy();
                        }
                        _ => {}
                    }
                }
            }

            /// The selection's text, unless it is our own, secret or not text.
            fn read_selection(state: &State, conn: &Connection) -> io::Result<Option<Option<Vec<u8>>>> {
                if state.own.is_some() {
                    return Ok(None);
                }
                let Some(offer) = &state.selection else {
                    return Ok(None);
                };
                let types = state.offers.get(&offer.id()).cloned().unwrap_or_default();
                let receive = |mime: &str| -> io::Result<Option<Vec<u8>>> {
                    let (r, w) = pipe()?;
                    offer.receive(mime.into(), w.as_fd());
                    drop(w);
                    conn.flush().map_err(io::Error::other)?;
                    read_pipe(r)
                };
                if types.iter().any(|t| t == SECRET_HINT) {
                    if receive(SECRET_HINT)?.is_some_and(|h| is_secret(&h)) {
                        return Ok(None);
                    }
                }
                let Some(mime) = TEXT_TYPES.iter().find(|t| types.iter().any(|m| m == *t)) else {
                    return Ok(None);
                };
                receive(mime).map(Some)
            }

            /// Bridges the clipboard until stdin closes; `Err(Unavailable)` if the compositor lacks
            /// this protocol.
            pub fn run(conn: &Connection) -> Result<Result<(), String>, Unavailable> {
                let (globals, mut queue) = registry_queue_init::<State>(conn)
                    .map_err(|e| Unavailable(e.to_string()))?;
                let qh = queue.handle();
                let manager: Manager = globals
                    .bind(&qh, 1..=1, ())
                    .map_err(|e| Unavailable(format!("{}: {e}", stringify!($manager))))?;
                let seat: WlSeat = globals
                    .bind(&qh, 1..=1, ())
                    .map_err(|e| Unavailable(format!("seat: {e}")))?;
                let device = manager.get_data_device(&seat, &qh, ());
                let mut state = State::default();
                let mut stdin = std::io::stdin().lock();
                let mut stdout = std::io::stdout().lock();
                let stdin_fd = std::io::stdin().as_raw_fd();
                Ok((|| -> Result<(), String> {
                    loop {
                        queue.dispatch_pending(&mut state).map_err(|e| e.to_string())?;
                        if state.finished {
                            return Err("the compositor ended data control".into());
                        }
                        if std::mem::take(&mut state.changed) {
                            match read_selection(&state, conn) {
                                Ok(Some(text)) => report(&mut stdout, text).map_err(|e| e.to_string())?,
                                Ok(None) => {}
                                Err(e) => tracing::debug!("clipboard read: {e}"),
                            }
                        }
                        queue.flush().map_err(|e| e.to_string())?;
                        let Some(guard) = queue.prepare_read() else {
                            continue;
                        };
                        let ready = poll_in(&[guard.connection_fd().as_raw_fd(), stdin_fd], None)
                            .map_err(|e| e.to_string())?;
                        if ready[0] {
                            guard.read().map_err(|e| e.to_string())?;
                        } else {
                            drop(guard);
                        }
                        if ready[1] {
                            let (kind, payload) = match read_frame(&mut stdin) {
                                Ok(f) => f,
                                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
                                Err(e) => return Err(e.to_string()),
                            };
                            if kind == SET {
                                let source = manager.create_data_source(&qh, ());
                                for t in TEXT_TYPES {
                                    source.offer(t.into());
                                }
                                device.set_selection(Some(&source));
                                if let Some((old, _)) = state.own.replace((source, Arc::new(payload))) {
                                    old.destroy();
                                }
                                write_frame(&mut stdout, WRITTEN, &[1]).map_err(|e| e.to_string())?;
                            }
                        }
                    }
                })())
            }
        }
    };
}

data_control!(
    ext,
    wayland_protocols::ext::data_control::v1::client,
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_device_v1::ExtDataControlDeviceV1,
    ext_data_control_offer_v1::ExtDataControlOfferV1,
    ext_data_control_source_v1::ExtDataControlSourceV1
);
data_control!(
    wlr,
    wayland_protocols_wlr::data_control::v1::client,
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_device_v1::ZwlrDataControlDeviceV1,
    zwlr_data_control_offer_v1::ZwlrDataControlOfferV1,
    zwlr_data_control_source_v1::ZwlrDataControlSourceV1
);

/// Bridges the Wayland clipboard until stdin closes.
pub fn run() -> Result<Result<(), String>, Unavailable> {
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return Err(Unavailable("WAYLAND_DISPLAY is not set".into()));
    }
    let connect = || Connection::connect_to_env().map_err(|e| Unavailable(e.to_string()));
    match ext::run(&connect()?) {
        Err(Unavailable(why)) => {
            tracing::debug!("{why}");
            wlr::run(&connect()?)
        }
        done => done,
    }
}
