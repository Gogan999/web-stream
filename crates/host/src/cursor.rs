//! The host pointer, for the browser to draw (the video carries none).
//!
//! While a browser is connected a thread polls the pointer and keeps only
//! the newest reading in a `watch` channel, and a task of its own sends the
//! browser what changed. The media loop only tells that task where the
//! streamed display is and whether the `control` channel is open, so a
//! slow or full channel never holds up input or other control traffic, and
//! never builds a backlog of old positions.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::AbortHandle;
use ws_input::cursor::{CursorLook, CursorState};
use ws_proto::{CursorKind, CursorShape, HostControl, channels};
use ws_transport::{Error, Transport};

use crate::platform;

/// About two polls per 60 Hz frame; one `GetCursorInfo` call when the
/// pointer hasn't changed.
const POLL: Duration = Duration::from_millis(8);
/// After the channel refused a write, nothing more is tried for this long
/// (the newest state is sent then).
const RETRY: Duration = Duration::from_millis(100);

/// What the sender needs from the media loop.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CursorTarget {
    /// The streamed display's top-left in desktop pixels, while the
    /// `control` channel is open; `None` sends nothing, and everything is
    /// sent afresh when it is set again.
    pub origin: Option<(i32, i32)>,
    /// The browser draws the pointer itself (relative mouse mode) and wants
    /// positions. In absolute mode its own pointer is where the host's is.
    pub positions: bool,
}

/// The cursor thread and sender of one browser session; dropping it stops
/// both.
pub struct CursorSession {
    _thread: Option<CursorThread>,
    task: AbortHandle,
}

impl CursorSession {
    pub fn start(transport: Transport, target: watch::Receiver<CursorTarget>) -> Self {
        let (tx, rx) = watch::channel(None);
        let thread = CursorThread::start(tx);
        let task = tokio::spawn(send_loop(transport, rx, target)).abort_handle();
        Self {
            _thread: thread,
            task,
        }
    }
}

impl Drop for CursorSession {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct CursorThread {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl CursorThread {
    /// `None` if the thread can't be started.
    fn start(out: watch::Sender<Option<CursorState>>) -> Option<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::Builder::new()
            .name("ws-cursor".into())
            .spawn(move || {
                // None where the platform can't read the pointer.
                let Some(mut reader) = platform::cursor_reader() else {
                    return;
                };
                while !flag.load(Ordering::Relaxed) {
                    if let Some(state) = reader() {
                        out.send_if_modified(|cur| {
                            let changed = cur.as_ref() != Some(&state);
                            if changed {
                                *cur = Some(state);
                            }
                            changed
                        });
                    }
                    std::thread::sleep(POLL);
                }
            })
            .inspect_err(|e| tracing::warn!("cursor thread: {e}"))
            .ok()?;
        Some(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for CursorThread {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The pointer as the browser last heard it.
#[derive(Debug, Default)]
struct Sent {
    look: Option<CursorLook>,
    position: Option<(i32, i32)>,
    /// Image ids the browser has (it caches them by id).
    images: HashSet<u32>,
}

/// What to record once a message has gone out.
#[derive(Debug, PartialEq)]
enum Commit {
    Shape(CursorLook),
    Position((i32, i32)),
}

impl Sent {
    fn apply(&mut self, commit: Commit) {
        match commit {
            Commit::Shape(look) => {
                self.images.extend(look.image.as_ref().map(|(id, _)| *id));
                self.look = Some(look);
            }
            Commit::Position(p) => self.position = Some(p),
        }
    }
}

/// The messages that bring the browser up to date with `state`: the shape
/// if it changed (with its image if the browser lacks it), and the position
/// if it changed, the pointer shows and the browser wants positions. Each
/// stands alone, so a refused shape doesn't hold back the position.
fn updates(sent: &Sent, state: &CursorState, target: CursorTarget) -> Vec<(HostControl, Commit)> {
    let Some((ox, oy)) = target.origin else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if sent.look.as_ref() != Some(&state.look) {
        out.push((
            HostControl::CursorShape(shape_message(&state.look, &sent.images)),
            Commit::Shape(state.look.clone()),
        ));
    }
    let position = (state.x - ox, state.y - oy);
    if target.positions && state.look.kind != CursorKind::Hidden && sent.position != Some(position)
    {
        let (x, y) = position;
        out.push((
            HostControl::CursorPosition { x, y },
            Commit::Position(position),
        ));
    }
    out
}

/// The `cursorShape` message for `look`; the image goes along only if the
/// browser doesn't have it yet.
fn shape_message(look: &CursorLook, images: &HashSet<u32>) -> CursorShape {
    CursorShape {
        kind: look.kind,
        id: look.image.as_ref().map(|(id, _)| *id),
        image: look
            .image
            .as_ref()
            .filter(|(id, _)| !images.contains(id))
            .map(|(_, image)| (**image).clone()),
    }
}

async fn send_loop(
    transport: Transport,
    mut cursor: watch::Receiver<Option<CursorState>>,
    mut target: watch::Receiver<CursorTarget>,
) {
    let mut sent = Sent::default();
    loop {
        let t = *target.borrow_and_update();
        if t.origin.is_none() {
            // A browser that (re)opens the channel is told everything.
            sent = Sent::default();
        }
        let state = cursor.borrow_and_update().clone();
        let mut refused = false;
        for (msg, commit) in state.map(|s| updates(&sent, &s, t)).unwrap_or_default() {
            let json = match serde_json::to_vec(&msg) {
                Ok(j) => j,
                Err(e) => {
                    tracing::error!("encode cursor message: {e}");
                    continue;
                }
            };
            match transport.send_data(channels::CONTROL, false, json).await {
                Ok(true) => sent.apply(commit),
                Ok(false) | Err(Error::ChannelNotOpen(_)) => refused = true,
                Err(Error::Stopped) => return,
                Err(e) => {
                    tracing::debug!("cursor message not sent: {e}");
                    refused = true;
                }
            }
        }
        if refused {
            // Pointer changes meanwhile wait in the watch; only the newest
            // is sent.
            tokio::select! {
                _ = tokio::time::sleep(RETRY) => {}
                r = target.changed() => if r.is_err() { return },
            }
            continue;
        }
        tokio::select! {
            // An error: the thread is gone (or never ran), nothing more
            // to send.
            r = cursor.changed() => if r.is_err() { return },
            r = target.changed() => if r.is_err() { return },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ws_proto::CursorImage;

    fn look(kind: CursorKind, id: Option<u32>) -> CursorLook {
        CursorLook {
            kind,
            image: id.map(|id| {
                (
                    id,
                    Arc::new(CursorImage {
                        width: 1,
                        height: 1,
                        hot_x: 0,
                        hot_y: 0,
                        rgba: vec![0; 4],
                    }),
                )
            }),
        }
    }

    fn state(look: CursorLook, x: i32, y: i32) -> CursorState {
        CursorState { look, x, y }
    }

    const OPEN: CursorTarget = CursorTarget {
        origin: Some((1920, 0)),
        positions: true,
    };

    fn send_all(sent: &mut Sent, s: &CursorState, t: CursorTarget) -> Vec<HostControl> {
        updates(sent, s, t)
            .into_iter()
            .map(|(msg, commit)| {
                sent.apply(commit);
                msg
            })
            .collect()
    }

    #[test]
    fn image_goes_once_and_positions_are_relative_to_the_display() {
        let mut sent = Sent::default();
        let s = state(look(CursorKind::EwResize, Some(4)), 2000, 30);
        let msgs = send_all(&mut sent, &s, OPEN);
        let HostControl::CursorShape(shape) = &msgs[0] else {
            panic!("{msgs:?}")
        };
        assert_eq!((shape.kind, shape.id), (CursorKind::EwResize, Some(4)));
        assert!(shape.image.is_some());
        assert_eq!(msgs[1], HostControl::CursorPosition { x: 80, y: 30 });

        // Unchanged: nothing. Back to a known image: id only.
        assert!(send_all(&mut sent, &s, OPEN).is_empty());
        send_all(
            &mut sent,
            &state(look(CursorKind::Text, Some(5)), 2000, 30),
            OPEN,
        );
        let msgs = send_all(&mut sent, &s, OPEN);
        let HostControl::CursorShape(shape) = &msgs[0] else {
            panic!("{msgs:?}")
        };
        assert_eq!((shape.id, shape.image.is_none()), (Some(4), true));
        assert_eq!(msgs.len(), 1);

        // The display moved: the same desktop point is a new position.
        let moved = CursorTarget {
            origin: Some((0, 0)),
            ..OPEN
        };
        assert_eq!(
            send_all(&mut sent, &s, moved),
            vec![HostControl::CursorPosition { x: 2000, y: 30 }]
        );
    }

    #[test]
    fn positions_only_when_wanted_and_shown() {
        let mut sent = Sent::default();
        let absolute = CursorTarget {
            positions: false,
            ..OPEN
        };
        let s = state(look(CursorKind::Default, None), 2000, 30);
        assert_eq!(send_all(&mut sent, &s, absolute).len(), 1);
        let hidden = state(CursorLook::HIDDEN, 2010, 30);
        assert_eq!(send_all(&mut sent, &hidden, OPEN).len(), 1, "shape only");
        assert!(
            updates(&sent, &s, CursorTarget::default()).is_empty(),
            "closed"
        );
    }

    #[test]
    fn a_refused_shape_does_not_hold_back_the_position() {
        let sent = Sent::default();
        let s = state(look(CursorKind::Custom, Some(1)), 2000, 30);
        let out = updates(&sent, &s, OPEN);
        assert_eq!(out.len(), 2);
        // Only the position went out: the shape is still pending next time.
        let mut sent = Sent::default();
        let (_, commit) = out.into_iter().nth(1).unwrap();
        sent.apply(commit);
        let again = updates(&sent, &s, OPEN);
        assert_eq!(again.len(), 1);
        assert!(matches!(again[0].0, HostControl::CursorShape(_)));
    }
}
