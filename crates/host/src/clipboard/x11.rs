//! The helper's X11 side: the CLIPBOARD selection, with XFixes change notifications.

use std::io;
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xfixes::{ConnectionExt as _, SelectionEventMask};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, PropMode, Property,
    SELECTION_NOTIFY_EVENT, SelectionNotifyEvent, SelectionRequestEvent, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{COPY_DEPTH_FROM_PARENT, CURRENT_TIME, NONE};

use super::MAX_BYTES;
use super::linux::{
    SECRET_HINT, SET, TEXT_TYPES, WRITTEN, is_secret, read_frame, report, write_frame,
};
use super::wayland::poll_in;

/// How long the owner gets to answer a conversion.
const CONVERT_TIMEOUT: Duration = Duration::from_secs(2);

type Error = Box<dyn std::error::Error>;

struct Atoms {
    clipboard: Atom,
    targets: Atom,
    incr: Atom,
    property: Atom,
    hint: Atom,
    /// [`TEXT_TYPES`] in order.
    text: Vec<Atom>,
}

struct X {
    conn: RustConnection,
    window: Window,
    atoms: Atoms,
    /// Text we own the selection with.
    own: Option<Vec<u8>>,
    changed: bool,
}

fn intern(conn: &RustConnection, name: &str) -> Result<Atom, Error> {
    Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
}

impl X {
    fn connect() -> Result<Self, Error> {
        let (conn, screen) = x11rb::connect(None)?;
        let root = conn.setup().roots[screen].root;
        let window = conn.generate_id()?;
        conn.create_window(
            COPY_DEPTH_FROM_PARENT,
            window,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            x11rb::COPY_FROM_PARENT,
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )?;
        let atoms = Atoms {
            clipboard: intern(&conn, "CLIPBOARD")?,
            targets: intern(&conn, "TARGETS")?,
            incr: intern(&conn, "INCR")?,
            property: intern(&conn, "WEB_STREAM_CLIP")?,
            hint: intern(&conn, SECRET_HINT)?,
            text: TEXT_TYPES
                .iter()
                .map(|t| intern(&conn, t))
                .collect::<Result<_, _>>()?,
        };
        conn.xfixes_query_version(5, 0)?.reply()?;
        conn.xfixes_select_selection_input(
            window,
            atoms.clipboard,
            SelectionEventMask::SET_SELECTION_OWNER
                | SelectionEventMask::SELECTION_WINDOW_DESTROY
                | SelectionEventMask::SELECTION_CLIENT_CLOSE,
        )?;
        conn.flush()?;
        Ok(Self {
            conn,
            window,
            atoms,
            own: None,
            changed: false,
        })
    }

    /// Handles an event that is not part of a conversion we wait for.
    fn event(&mut self, event: Event) -> Result<(), Error> {
        match event {
            Event::XfixesSelectionNotify(e) if e.selection == self.atoms.clipboard => {
                if e.owner != self.window {
                    self.own = None;
                    self.changed = true;
                }
            }
            Event::SelectionClear(e) if e.selection == self.atoms.clipboard => self.own = None,
            Event::SelectionRequest(e) => self.answer(&e)?,
            _ => {}
        }
        Ok(())
    }

    /// Serves our text to a program that pastes it.
    fn answer(&self, e: &SelectionRequestEvent) -> Result<(), Error> {
        let property = if e.property == NONE {
            e.target
        } else {
            e.property
        };
        let served = match &self.own {
            Some(text) if e.selection == self.atoms.clipboard => {
                if e.target == self.atoms.targets {
                    let mut list = vec![self.atoms.targets];
                    list.extend(&self.atoms.text);
                    self.conn.change_property32(
                        PropMode::REPLACE,
                        e.requestor,
                        property,
                        AtomEnum::ATOM,
                        &list,
                    )?;
                    true
                } else if self.atoms.text.contains(&e.target) {
                    self.conn.change_property8(
                        PropMode::REPLACE,
                        e.requestor,
                        property,
                        e.target,
                        text,
                    )?;
                    true
                } else {
                    false
                }
            }
            _ => false,
        };
        let reply = SelectionNotifyEvent {
            response_type: SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: e.time,
            requestor: e.requestor,
            selection: e.selection,
            target: e.target,
            property: if served { property } else { NONE },
        };
        self.conn
            .send_event(false, e.requestor, EventMask::NO_EVENT, reply)?;
        self.conn.flush()?;
        Ok(())
    }

    /// Waits for the next event, up to `deadline`.
    fn next_event(&mut self, deadline: Instant) -> Result<Option<Event>, Error> {
        loop {
            if let Some(e) = self.conn.poll_for_event()? {
                return Ok(Some(e));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            self.conn.flush()?;
            poll_in(&[self.conn.stream().as_raw_fd()], Some(left))?;
        }
    }

    /// The clipboard converted to `target`; `Ok(None)` if the owner refused or was too slow, and
    /// `Some(None)` inside when the data is over [`MAX_BYTES`].
    fn convert(&mut self, target: Atom) -> Result<Option<Option<Vec<u8>>>, Error> {
        let a = &self.atoms;
        self.conn
            .convert_selection(self.window, a.clipboard, target, a.property, CURRENT_TIME)?;
        self.conn.flush()?;
        let deadline = Instant::now() + CONVERT_TIMEOUT;
        loop {
            match self.next_event(deadline)? {
                None => return Ok(None),
                Some(Event::SelectionNotify(e)) if e.requestor == self.window => {
                    if e.property == NONE {
                        return Ok(None);
                    }
                    return self.take_property().map(Some);
                }
                Some(e) => self.event(e)?,
            }
        }
    }

    /// Reads and deletes our property, following an INCR transfer.
    fn take_property(&mut self) -> Result<Option<Vec<u8>>, Error> {
        let max = (MAX_BYTES as u32 / 4) + 1;
        let reply = self
            .conn
            .get_property(
                true,
                self.window,
                self.atoms.property,
                AtomEnum::ANY,
                0,
                max,
            )?
            .reply()?;
        if reply.type_ != self.atoms.incr {
            if reply.bytes_after > 0 || reply.value.len() > MAX_BYTES {
                return Ok(None);
            }
            return Ok(Some(reply.value));
        }
        let mut out = Vec::new();
        let deadline = Instant::now() + CONVERT_TIMEOUT;
        loop {
            match self.next_event(deadline)? {
                None => return Err("INCR transfer timed out".into()),
                Some(Event::PropertyNotify(e))
                    if e.window == self.window
                        && e.atom == self.atoms.property
                        && e.state == Property::NEW_VALUE =>
                {
                    let chunk = self
                        .conn
                        .get_property(
                            true,
                            self.window,
                            self.atoms.property,
                            AtomEnum::ANY,
                            0,
                            max,
                        )?
                        .reply()?;
                    if chunk.value.is_empty() {
                        return Ok(Some(out));
                    }
                    out.extend_from_slice(&chunk.value);
                    if out.len() > MAX_BYTES {
                        return Ok(None);
                    }
                }
                Some(e) => self.event(e)?,
            }
        }
    }

    /// The clipboard's text, unless it is secret or not text.
    fn read(&mut self) -> Result<Option<Option<Vec<u8>>>, Error> {
        let Some(Some(raw)) = self.convert(self.atoms.targets)? else {
            return Ok(None);
        };
        let offered: Vec<Atom> = raw
            .chunks_exact(4)
            .map(|b| u32::from_ne_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        if offered.contains(&self.atoms.hint)
            && self
                .convert(self.atoms.hint)?
                .flatten()
                .is_some_and(|h| is_secret(&h))
        {
            return Ok(None);
        }
        let Some(&target) = self.atoms.text.iter().find(|t| offered.contains(t)) else {
            return Ok(None);
        };
        self.convert(target)
    }
}

/// Bridges the X11 clipboard until stdin closes.
pub fn run() -> Result<(), String> {
    let mut x = X::connect().map_err(|e| format!("X11: {e}"))?;
    let mut stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();
    let stdin_fd = io::stdin().as_raw_fd();
    let err = |e: Error| e.to_string();
    loop {
        while let Some(e) = x.conn.poll_for_event().map_err(|e| e.to_string())? {
            x.event(e).map_err(err)?;
        }
        if std::mem::take(&mut x.changed) {
            match x.read() {
                Ok(Some(text)) => report(&mut stdout, text).map_err(|e| e.to_string())?,
                Ok(None) => {}
                Err(e) => tracing::debug!("clipboard read: {e}"),
            }
            continue;
        }
        x.conn.flush().map_err(|e| e.to_string())?;
        let ready =
            poll_in(&[x.conn.stream().as_raw_fd(), stdin_fd], None).map_err(|e| e.to_string())?;
        if ready[1] {
            let (kind, payload) = match read_frame(&mut stdin) {
                Ok(f) => f,
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(e) => return Err(e.to_string()),
            };
            if kind == SET {
                x.conn
                    .set_selection_owner(x.window, x.atoms.clipboard, CURRENT_TIME)
                    .map_err(|e| e.to_string())?;
                let owner = x
                    .conn
                    .get_selection_owner(x.atoms.clipboard)
                    .map_err(|e| e.to_string())?
                    .reply()
                    .map_err(|e| e.to_string())?
                    .owner;
                let ok = owner == x.window;
                x.own = ok.then_some(payload);
                write_frame(&mut stdout, WRITTEN, &[u8::from(ok)]).map_err(|e| e.to_string())?;
            }
        }
    }
}
