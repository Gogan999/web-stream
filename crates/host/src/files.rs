//! Files the host user shares with the connected browser.
//!
//! The host user picks files (tray app or console); each is opened right
//! then and offered to the browser under a random id. The browser can only
//! ask for those ids, never for a path. The handle opened at pick time is
//! the one read from, so a file renamed or replaced afterwards can't
//! redirect a transfer, and on Windows it is opened with read sharing only,
//! so nobody can change it while it is on offer.
//!
//! One file is sent at a time on the `file` channel, in [`file_chunk`]s.
//! The browser acks what it has stored and the host keeps at most
//! [`FILE_WINDOW`] past that in flight, so a slow disk in the browser never
//! piles up memory. File data is not part of the video's bandwidth
//! estimate, so while video streams it is paced to a share of that
//! estimate to leave the game stream its room.
//!
//! Everything lives in one task per browser session; dropping the
//! [`FileSession`] ends it, and with it every offer.

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use ws_proto::{FILE_CHUNK_MAX, FILE_WINDOW, FileId, HostControl, channels, file_chunk, names};
use ws_transport::Transport;

use crate::serve::Event;

/// Bytes read from disk at a time.
const READ_BLOCK: usize = 256 * 1024;
/// Share of the bandwidth estimate a file may use.
const BWE_SHARE: f64 = 0.25;
/// Least a file is paced to, in bits per second: enough to finish a small
/// file on a link too weak to spare a quarter of itself.
const MIN_RATE_BPS: f64 = 100_000.0;
/// Bytes the pacer lets out in one burst.
const BURST: f64 = 64.0 * 1024.0;
/// After the channel refused a write, the next try waits for
/// `ChannelLow` or this long, whichever is first.
const RETRY: Duration = Duration::from_millis(50);
/// Progress reaches the front end at most this often per file.
const PROGRESS_EVERY: Duration = Duration::from_millis(250);
const QUEUE: usize = 256;
/// Longest file name offered, in characters.
const NAME_MAX: usize = 200;

/// What the host user sees happen to shared files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileEvent {
    /// Offered to the browser.
    Shared { id: FileId, name: String, size: u64 },
    /// The browser has stored `received` bytes of it.
    Progress {
        id: FileId,
        name: String,
        received: u64,
    },
    /// The browser has all of it.
    Sent { id: FileId, name: String },
    /// The browser stopped the transfer. Still on offer.
    Cancelled { id: FileId, name: String },
    /// A transfer failed. Still on offer.
    Failed {
        id: FileId,
        name: String,
        message: String,
    },
    /// No longer on offer, as the host user asked.
    Withdrawn { id: FileId, name: String },
    /// The browser left: nothing is on offer any more.
    Cleared,
    /// The browser is sending a file, saved as `name` (see `uploads.rs`).
    Receiving { name: String, size: u64 },
    /// A file from the browser is complete.
    Received { name: String, path: PathBuf },
    /// A file from the browser was refused or broke off; nothing was kept.
    ReceiveFailed { name: String, message: String },
}

/// What the host user asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileCommand {
    Share(Vec<PathBuf>),
    Stop(FileId),
}

/// What the media loop passes on.
#[derive(Debug)]
pub enum FileMsg {
    Command(FileCommand),
    Request(FileId),
    Ack {
        id: FileId,
        received: u64,
    },
    Cancel(FileId),
    /// The `file` channel drained.
    ChannelLow,
    /// Bandwidth estimate, bits per second, while video streams; `None`
    /// without video, when there is nothing to leave room for.
    Bitrate(Option<u64>),
}

/// The file sender of one browser session; dropping it ends every offer.
pub struct FileSession {
    tx: mpsc::Sender<FileMsg>,
    task: AbortHandle,
    events: mpsc::UnboundedSender<Event>,
}

impl FileSession {
    pub fn start(
        transport: Transport,
        control: mpsc::Sender<HostControl>,
        events: mpsc::UnboundedSender<Event>,
    ) -> Self {
        let (tx, rx) = mpsc::channel(QUEUE);
        let sender = Sender {
            out: TransportOut { transport },
            control,
            events: events.clone(),
            offers: HashMap::new(),
            queue: VecDeque::new(),
            current: None,
            pacer: Pacer::default(),
        };
        let task = tokio::spawn(sender.run(rx)).abort_handle();
        Self { tx, task, events }
    }

    pub fn send(&self, msg: FileMsg) {
        if let Err(e) = self.tx.try_send(msg) {
            tracing::warn!("file sender is behind; dropped {:?}", e.into_inner());
        }
    }
}

impl Drop for FileSession {
    fn drop(&mut self) {
        self.task.abort();
        let _ = self.events.send(Event::File(FileEvent::Cleared));
    }
}

/// Where file chunks go: the transport, or a test's stand-in.
trait ChunkOut {
    /// `Ok(false)` when the channel is full.
    async fn write(&self, chunk: Vec<u8>) -> Result<bool, String>;
}

struct TransportOut {
    transport: Transport,
}

impl ChunkOut for TransportOut {
    async fn write(&self, chunk: Vec<u8>) -> Result<bool, String> {
        self.transport
            .send_data(channels::FILE, true, chunk)
            .await
            .map_err(|e| e.to_string())
    }
}

struct Offer {
    name: String,
    size: u64,
    file: Arc<File>,
}

struct Sending {
    id: FileId,
    /// Bytes written to the channel.
    sent: u64,
    /// Bytes the browser has stored.
    acked: u64,
    /// Bytes read from disk at `block_start` and not sent yet.
    block: Vec<u8>,
    block_start: u64,
    /// The channel refused a write at this time; wait for it to drain.
    blocked: Option<Instant>,
    last_progress: Instant,
}

/// A token bucket over file bytes; no limit until a bandwidth estimate is
/// known.
#[derive(Debug, Default)]
struct Pacer {
    /// Bytes per second.
    rate: Option<f64>,
    tokens: f64,
    refilled: Option<Instant>,
}

impl Pacer {
    fn set_estimate(&mut self, bps: Option<u64>) {
        self.rate = bps.map(|bps| (bps as f64 * BWE_SHARE).max(MIN_RATE_BPS) / 8.0);
        if self.rate.is_none() {
            self.refilled = None;
        }
    }

    fn refill(&mut self, now: Instant) {
        let Some(rate) = self.rate else { return };
        let elapsed = self
            .refilled
            .map_or(Duration::ZERO, |t| now.saturating_duration_since(t));
        if self.refilled.is_none() {
            self.tokens = BURST;
        }
        self.tokens = (self.tokens + rate * elapsed.as_secs_f64()).min(BURST);
        self.refilled = Some(now);
    }

    /// `None` if `bytes` may go now, else when they may.
    fn wait(&mut self, bytes: usize, now: Instant) -> Option<Instant> {
        let rate = self.rate?;
        self.refill(now);
        let short = bytes as f64 - self.tokens;
        (short > 0.0).then(|| now + Duration::from_secs_f64(short / rate))
    }

    fn take(&mut self, bytes: usize) {
        if self.rate.is_some() {
            self.tokens -= bytes as f64;
        }
    }
}

struct Sender<O> {
    out: O,
    control: mpsc::Sender<HostControl>,
    events: mpsc::UnboundedSender<Event>,
    offers: HashMap<FileId, Offer>,
    /// Requested files waiting for the current one.
    queue: VecDeque<FileId>,
    current: Option<Sending>,
    pacer: Pacer,
}

impl<O: ChunkOut> Sender<O> {
    async fn run(mut self, mut rx: mpsc::Receiver<FileMsg>) {
        let mut wake = None;
        loop {
            tokio::select! {
                msg = rx.recv() => match msg {
                    Some(msg) => self.handle(msg).await,
                    None => break,
                },
                _ = sleep_until(wake) => {}
            }
            wake = self.pump().await;
        }
    }

    async fn handle(&mut self, msg: FileMsg) {
        match msg {
            FileMsg::Command(FileCommand::Share(paths)) => self.share(paths).await,
            FileMsg::Command(FileCommand::Stop(id)) => self.withdraw(id),
            FileMsg::Request(id) => self.request(id),
            FileMsg::Ack { id, received } => self.ack(id, received),
            FileMsg::Cancel(id) => self.cancel(id),
            FileMsg::ChannelLow => {
                if let Some(cur) = &mut self.current {
                    cur.blocked = None;
                }
            }
            FileMsg::Bitrate(bps) => self.pacer.set_estimate(bps),
        }
    }

    async fn share(&mut self, paths: Vec<PathBuf>) {
        let opened = tokio::task::spawn_blocking(move || {
            paths
                .into_iter()
                .map(|p| {
                    let r = open_offer(&p);
                    (p, r)
                })
                .collect::<Vec<_>>()
        })
        .await
        .unwrap_or_default();
        for (path, result) in opened {
            match result {
                Ok(offer) => {
                    let id = new_id(&self.offers);
                    tracing::info!(id = id.0, path = %path.display(), size = offer.size, "sharing");
                    self.reply(HostControl::FileOffer {
                        id,
                        name: offer.name.clone(),
                        size: offer.size,
                    });
                    self.event(FileEvent::Shared {
                        id,
                        name: offer.name.clone(),
                        size: offer.size,
                    });
                    self.offers.insert(id, offer);
                }
                Err(e) => {
                    let _ = self.events.send(Event::Error(format!(
                        "could not share {}: {e}",
                        path.display()
                    )));
                }
            }
        }
    }

    fn withdraw(&mut self, id: FileId) {
        let Some(offer) = self.offers.remove(&id) else {
            return;
        };
        self.queue.retain(|q| *q != id);
        if self.current.as_ref().is_some_and(|c| c.id == id) {
            self.current = None;
            self.next();
        }
        self.reply(HostControl::FileWithdrawn { id });
        self.event(FileEvent::Withdrawn {
            id,
            name: offer.name,
        });
    }

    fn request(&mut self, id: FileId) {
        if !self.offers.contains_key(&id) {
            tracing::warn!(id = id.0, "browser asked for a file that is not on offer");
            self.reply(HostControl::FileFailed {
                id,
                message: "that file is not shared".into(),
            });
            return;
        }
        if let Some(cur) = &self.current {
            if cur.id == id {
                // Asked again: start over.
                self.start(id);
            } else if !self.queue.contains(&id) {
                self.queue.push_back(id);
            }
            return;
        }
        self.start(id);
    }

    fn start(&mut self, id: FileId) {
        self.current = Some(Sending::new(id));
        if !self.check_done() {
            let name = self.name(id);
            self.event(FileEvent::Progress {
                id,
                name,
                received: 0,
            });
        }
    }

    fn ack(&mut self, id: FileId, received: u64) {
        let Some(cur) = &mut self.current else { return };
        if cur.id != id {
            return;
        }
        cur.acked = cur.acked.max(received.min(cur.sent));
        let (acked, due) = (cur.acked, cur.last_progress.elapsed() >= PROGRESS_EVERY);
        if due {
            cur.last_progress = Instant::now();
        }
        if !self.check_done() && due {
            let name = self.name(id);
            self.event(FileEvent::Progress {
                id,
                name,
                received: acked,
            });
        }
    }

    fn cancel(&mut self, id: FileId) {
        self.queue.retain(|q| *q != id);
        if self.current.as_ref().is_some_and(|c| c.id == id) {
            self.current = None;
            let name = self.name(id);
            self.event(FileEvent::Cancelled { id, name });
            self.next();
        }
    }

    /// Finishes the current file if the browser has all of it; true if so.
    fn check_done(&mut self) -> bool {
        let Some(cur) = &self.current else {
            return false;
        };
        let id = cur.id;
        let Some(size) = self.offers.get(&id).map(|o| o.size) else {
            return false;
        };
        if cur.acked < size {
            return false;
        }
        self.current = None;
        let name = self.name(id);
        tracing::info!(id = id.0, "file sent");
        self.event(FileEvent::Sent { id, name });
        self.next();
        true
    }

    fn fail(&mut self, id: FileId, message: String) {
        tracing::warn!(id = id.0, "file transfer failed: {message}");
        self.current = None;
        self.reply(HostControl::FileFailed {
            id,
            message: message.clone(),
        });
        let name = self.name(id);
        self.event(FileEvent::Failed { id, name, message });
        self.next();
    }

    /// Starts the next requested file, if any.
    fn next(&mut self) {
        while self.current.is_none() {
            let Some(id) = self.queue.pop_front() else {
                return;
            };
            if self.offers.contains_key(&id) {
                self.start(id);
            }
        }
    }

    /// Sends what the window, the pacer and the channel allow. Returns when
    /// to try again without being asked.
    async fn pump(&mut self) -> Option<Instant> {
        loop {
            let cur = self.current.as_ref()?;
            let id = cur.id;
            let offer = self.offers.get(&id)?;
            let size = offer.size;
            let now = Instant::now();
            if let Some(at) = cur.blocked {
                // `ChannelLow` may never come when the buffer that was full
                // is the one all channels share: try again after a while.
                if now < at + RETRY {
                    return Some(at + RETRY);
                }
                self.current.as_mut()?.blocked = None;
                continue;
            }
            if cur.sent >= size || cur.sent - cur.acked >= FILE_WINDOW {
                return None;
            }
            let len = FILE_CHUNK_MAX.min((size - cur.sent) as usize);
            if let Some(at) = self.pacer.wait(len, now) {
                return Some(at);
            }
            if cur.sent >= cur.block_start + cur.block.len() as u64 {
                let (file, start) = (offer.file.clone(), cur.sent);
                let want = READ_BLOCK.min((size - start) as usize);
                let read =
                    tokio::task::spawn_blocking(move || read_block(&file, start, want)).await;
                match read {
                    Ok(Ok(block)) => {
                        let Some(cur) = self.current.as_mut().filter(|c| c.id == id) else {
                            continue;
                        };
                        cur.block = block;
                        cur.block_start = start;
                    }
                    Ok(Err(e)) => {
                        self.fail(id, read_error(&e));
                        continue;
                    }
                    Err(e) => {
                        self.fail(id, format!("could not read the file: {e}"));
                        continue;
                    }
                }
            }
            let cur = self.current.as_mut()?;
            let at = (cur.sent - cur.block_start) as usize;
            let chunk = file_chunk(id, cur.sent, &cur.block[at..at + len]);
            match self.out.write(chunk).await {
                Ok(true) => {
                    cur.sent += len as u64;
                    self.pacer.take(len);
                }
                Ok(false) => cur.blocked = Some(now),
                Err(e) => {
                    self.fail(id, format!("the connection closed: {e}"));
                }
            }
        }
    }

    fn name(&self, id: FileId) -> String {
        self.offers
            .get(&id)
            .map(|o| o.name.clone())
            .unwrap_or_default()
    }

    fn reply(&self, msg: HostControl) {
        if self.control.try_send(msg).is_err() {
            tracing::warn!("control queue full; dropping a file message");
        }
    }

    fn event(&self, e: FileEvent) {
        let _ = self.events.send(Event::File(e));
    }
}

impl Sending {
    fn new(id: FileId) -> Self {
        Self {
            id,
            sent: 0,
            acked: 0,
            block: Vec::new(),
            block_start: 0,
            blocked: None,
            last_progress: Instant::now(),
        }
    }
}

async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at.into()).await,
        None => std::future::pending().await,
    }
}

/// Opens `path` for sharing: a regular file, held open from now on.
fn open_offer(path: &Path) -> Result<Offer, String> {
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_SHARE_READ: others may read it but not change, rename or
        // delete it while it is on offer.
        opts.share_mode(0x1);
    }
    let file = opts.open(path).map_err(|e| open_error(&e))?;
    let meta = file.metadata().map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("not a file".into());
    }
    Ok(Offer {
        name: display_name(path),
        size: meta.len(),
        file: Arc::new(file),
    })
}

fn open_error(e: &std::io::Error) -> String {
    // ERROR_SHARING_VIOLATION: another program has it open for writing.
    if cfg!(windows) && e.raw_os_error() == Some(32) {
        return "another program is writing to it; close it there first".into();
    }
    e.to_string()
}

fn read_error(e: &std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::UnexpectedEof {
        return "the file got shorter while it was being sent".into();
    }
    format!("could not read the file: {e}")
}

fn read_block(file: &File, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
    let mut f = file;
    f.seek(SeekFrom::Start(offset))?;
    let mut block = vec![0; len];
    f.read_exact(&mut block)?;
    Ok(block)
}

/// The bare file name, without control characters, for the browser to show
/// and suggest; it cleans it again before saving.
fn display_name(path: &Path) -> String {
    let name: String = path
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default()
        .chars()
        .filter(|&c| names::printable(c))
        .collect();
    let name = names::truncate_utf16(name.trim(), NAME_MAX).trim();
    if name.is_empty() {
        "file".into()
    } else {
        name.into()
    }
}

/// A random id not in use, small enough to stay exact in JavaScript.
fn new_id(offers: &HashMap<FileId, Offer>) -> FileId {
    loop {
        let mut bytes = [0u8; 8];
        getrandom::fill(&mut bytes).expect("OS random numbers");
        let id = FileId(u64::from_le_bytes(bytes) & FileId::MAX);
        if id.0 != 0 && !offers.contains_key(&id) {
            return id;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use ws_proto::parse_file_chunk;

    use super::*;

    /// Takes chunks until `room` runs out, then refuses them.
    #[derive(Clone, Default)]
    struct FakeOut {
        chunks: Arc<Mutex<Vec<Vec<u8>>>>,
        room: Arc<Mutex<Option<usize>>>,
    }

    impl ChunkOut for FakeOut {
        async fn write(&self, chunk: Vec<u8>) -> Result<bool, String> {
            let mut room = self.room.lock().unwrap();
            if let Some(r) = room.as_mut() {
                if *r == 0 {
                    return Ok(false);
                }
                *r -= 1;
            }
            self.chunks.lock().unwrap().push(chunk);
            Ok(true)
        }
    }

    struct Harness {
        sender: Sender<FakeOut>,
        out: FakeOut,
        control: mpsc::Receiver<HostControl>,
        events: mpsc::UnboundedReceiver<Event>,
        dir: PathBuf,
    }

    impl Harness {
        fn new(name: &str) -> Self {
            let (control_tx, control) = mpsc::channel(64);
            let (events_tx, events) = mpsc::unbounded_channel();
            let out = FakeOut::default();
            let dir = std::env::temp_dir().join(format!("ws-files-{}-{name}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            Self {
                sender: Sender {
                    out: out.clone(),
                    control: control_tx,
                    events: events_tx,
                    offers: HashMap::new(),
                    queue: VecDeque::new(),
                    current: None,
                    pacer: Pacer::default(),
                },
                out,
                control,
                events,
                dir,
            }
        }

        /// Shares a file of `data`; returns its id.
        async fn share(&mut self, name: &str, data: &[u8]) -> FileId {
            let path = self.dir.join(name);
            std::fs::write(&path, data).unwrap();
            self.sender
                .handle(FileMsg::Command(FileCommand::Share(vec![path])))
                .await;
            match self.control.try_recv().unwrap() {
                HostControl::FileOffer { id, name: n, size } => {
                    assert_eq!(n, name);
                    assert_eq!(size, data.len() as u64);
                    id
                }
                other => panic!("{other:?}"),
            }
        }

        async fn msg(&mut self, msg: FileMsg) -> Option<Instant> {
            self.sender.handle(msg).await;
            self.sender.pump().await
        }

        /// The file bytes sent so far for `id`, checking the offsets.
        fn received(&self, id: FileId) -> Vec<u8> {
            let mut data = Vec::new();
            for c in self.out.chunks.lock().unwrap().iter() {
                let (cid, offset, bytes) = parse_file_chunk(c).unwrap();
                if cid == id {
                    assert_eq!(offset, data.len() as u64);
                    data.extend_from_slice(bytes);
                }
            }
            data
        }

        fn file_events(&mut self) -> Vec<FileEvent> {
            let mut out = Vec::new();
            while let Ok(e) = self.events.try_recv() {
                if let Event::File(f) = e {
                    out.push(f);
                }
            }
            out
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 7 % 251) as u8).collect()
    }

    #[tokio::test]
    async fn sends_a_file_within_the_window_and_finishes_on_the_last_ack() {
        let mut h = Harness::new("window");
        let data = pattern(3 * FILE_WINDOW as usize / 2 + 123);
        let id = h.share("big.bin", &data).await;
        assert!(id.0 <= FileId::MAX);

        h.msg(FileMsg::Request(id)).await;
        assert_eq!(h.received(id).len() as u64, FILE_WINDOW);
        // An ack past what was sent counts only what was sent.
        h.msg(FileMsg::Ack {
            id,
            received: u64::MAX,
        })
        .await;
        assert_eq!(h.received(id), data);
        assert!(h.sender.current.is_some(), "not done before the last ack");
        h.msg(FileMsg::Ack {
            id,
            received: data.len() as u64,
        })
        .await;
        assert!(h.sender.current.is_none());
        let events = h.file_events();
        assert!(matches!(&events[0], FileEvent::Shared { name, .. } if name == "big.bin"));
        assert!(matches!(events.last(), Some(FileEvent::Sent { id: i, .. }) if *i == id));
    }

    #[tokio::test]
    async fn rejects_ids_not_on_offer() {
        let mut h = Harness::new("unknown");
        h.share("a.txt", b"hello").await;
        h.msg(FileMsg::Request(FileId(12345))).await;
        assert!(matches!(
            h.control.try_recv().unwrap(),
            HostControl::FileFailed {
                id: FileId(12345),
                ..
            }
        ));
        assert!(h.out.chunks.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn queues_requests_and_skips_withdrawn_ones() {
        let mut h = Harness::new("queue");
        let a = h.share("a.txt", b"aaaa").await;
        let b = h.share("b.txt", b"bbbb").await;
        let c = h.share("c.txt", b"cccc").await;
        h.msg(FileMsg::Request(a)).await;
        h.msg(FileMsg::Request(b)).await;
        h.msg(FileMsg::Request(c)).await;
        assert_eq!(h.received(a), b"aaaa");
        assert!(h.received(b).is_empty());
        h.msg(FileMsg::Command(FileCommand::Stop(b))).await;
        assert!(matches!(
            h.control.try_recv().unwrap(),
            HostControl::FileWithdrawn { id } if id == b
        ));
        h.msg(FileMsg::Ack { id: a, received: 4 }).await;
        assert_eq!(h.received(c), b"cccc");
        assert!(h.received(b).is_empty());
        // A withdrawn file can't be asked for again.
        h.msg(FileMsg::Request(b)).await;
        assert!(matches!(
            h.control.try_recv().unwrap(),
            HostControl::FileFailed { .. }
        ));
    }

    #[tokio::test]
    async fn waits_for_the_channel_to_drain() {
        let mut h = Harness::new("full");
        let data = pattern(5 * FILE_CHUNK_MAX);
        let id = h.share("f.bin", &data).await;
        *h.out.room.lock().unwrap() = Some(2);
        let retry = h.msg(FileMsg::Request(id)).await;
        assert!(retry.is_some(), "tries again later by itself");
        assert_eq!(h.received(id).len(), 2 * FILE_CHUNK_MAX);
        *h.out.room.lock().unwrap() = None;
        h.msg(FileMsg::ChannelLow).await;
        assert_eq!(h.received(id), data);
    }

    #[tokio::test]
    async fn retries_a_full_channel_without_channel_low() {
        let mut h = Harness::new("retry");
        let data = pattern(3 * FILE_CHUNK_MAX);
        let id = h.share("f.bin", &data).await;
        *h.out.room.lock().unwrap() = Some(1);
        let retry = h.msg(FileMsg::Request(id)).await.expect("a retry time");
        assert!(
            retry > Instant::now(),
            "the retry is in the future, not a spin"
        );
        // The shared buffer drains, but no ChannelLow comes for this channel.
        *h.out.room.lock().unwrap() = None;
        tokio::time::sleep_until(retry.into()).await;
        assert_eq!(h.sender.pump().await, None);
        assert_eq!(h.received(id), data);
    }

    #[tokio::test]
    async fn cancel_stops_and_a_new_request_starts_over() {
        let mut h = Harness::new("cancel");
        let data = pattern(2 * FILE_WINDOW as usize);
        let id = h.share("f.bin", &data).await;
        h.msg(FileMsg::Request(id)).await;
        h.msg(FileMsg::Cancel(id)).await;
        assert!(h.sender.current.is_none());
        assert!(
            h.file_events()
                .iter()
                .any(|e| matches!(e, FileEvent::Cancelled { .. }))
        );
        h.out.chunks.lock().unwrap().clear();
        h.msg(FileMsg::Request(id)).await;
        assert_eq!(h.received(id), data[..FILE_WINDOW as usize]);
    }

    #[tokio::test]
    async fn empty_files_are_done_at_once() {
        let mut h = Harness::new("empty");
        let id = h.share("empty.txt", b"").await;
        h.msg(FileMsg::Request(id)).await;
        assert!(h.sender.current.is_none());
        assert!(
            h.file_events()
                .iter()
                .any(|e| matches!(e, FileEvent::Sent { .. }))
        );
    }

    #[tokio::test]
    async fn directories_and_missing_files_are_not_shared() {
        let mut h = Harness::new("dirs");
        let dir = h.dir.clone();
        h.sender
            .handle(FileMsg::Command(FileCommand::Share(vec![
                dir.clone(),
                dir.join("missing"),
            ])))
            .await;
        assert!(h.control.try_recv().is_err());
        let errors: Vec<_> = std::iter::from_fn(|| h.events.try_recv().ok()).collect();
        assert_eq!(errors.len(), 2);
        assert!(errors.iter().all(|e| matches!(e, Event::Error(_))));
    }

    #[test]
    fn pacer_holds_files_to_a_share_of_the_estimate() {
        let mut p = Pacer::default();
        let t0 = Instant::now();
        assert_eq!(p.wait(1 << 20, t0), None, "no limit before an estimate");
        p.set_estimate(Some(32_000_000)); // 25% = 8 Mb/s = 1 MB/s
        assert_eq!(p.wait(FILE_CHUNK_MAX, t0), None);
        for _ in 0..4 {
            p.take(FILE_CHUNK_MAX);
        }
        let at = p.wait(FILE_CHUNK_MAX, t0).expect("burst used up");
        let wait = at - t0;
        assert!(
            wait > Duration::from_millis(15) && wait < Duration::from_millis(18),
            "{wait:?}"
        );
        assert_eq!(p.wait(FILE_CHUNK_MAX, at + Duration::from_millis(1)), None);
        // Never below the floor.
        p.set_estimate(Some(100_000));
        assert_eq!(p.rate, Some(MIN_RATE_BPS / 8.0));
        // No video, no limit.
        p.set_estimate(None);
        assert_eq!(p.wait(1 << 20, at), None);
    }

    #[test]
    fn names_are_bare_and_printable() {
        assert_eq!(display_name(Path::new("/a/b/report.pdf")), "report.pdf");
        assert_eq!(display_name(Path::new("/a/b\u{7}c\n.txt")), "bc.txt");
        assert_eq!(display_name(Path::new("/")), "file");
        assert_eq!(display_name(Path::new("/a/x\u{202E}txt.exe")), "xtxt.exe");
        assert_eq!(display_name(Path::new(&"x".repeat(300))).len(), NAME_MAX);
    }
}
