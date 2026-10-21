//! Files the browser sends to the host, when the host user allows it
//! (`--allow-upload`).
//!
//! Everything lands in one folder. The browser's name is only a
//! suggestion: it is cut to a bare name, cleaned of whatever Windows would
//! read as a path, a stream or a device, and never replaces a file already
//! there (`name (2).ext`). The bytes go to `name.part`, which becomes the
//! file once complete; a cancelled or broken upload leaves nothing behind.
//! On Windows the file is marked as downloaded from the internet, so
//! SmartScreen and Office treat it with the same care as a download. The
//! host never opens what it receives.
//!
//! One file at a time, in [`file_chunk`]s on the `file` channel. The host
//! acks what it has written; the browser keeps at most [`FILE_WINDOW`]
//! past that in flight.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use tokio::sync::mpsc;
use tokio::task::{AbortHandle, block_in_place};
use ws_proto::{
    FILE_CHUNK_HEADER, FILE_CHUNK_MAX, FILE_WINDOW, FileId, HostControl, names, parse_file_chunk,
};

use crate::files::FileEvent;
use crate::serve::Event;

/// Largest file accepted.
pub const MAX_SIZE: u64 = 4 << 30;
/// Written bytes between acks.
const ACK_EVERY: u64 = 256 * 1024;
const _: () = assert!(ACK_EVERY <= FILE_WINDOW);
/// Chunks waiting to be written; the browser's window keeps it at about
/// 64.
const QUEUE: usize = 256;
/// Files one browser session may send, and their total size: a paired
/// browser can't fill the host's disk with a stream of files.
const SESSION_FILES: u32 = 500;
const SESSION_BYTES: u64 = 32 << 30;
/// Longest name kept, in UTF-16 units.
const NAME_MAX: usize = 200;
/// Device names Windows opens whatever the extension.
const RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$", "COM0", "COM1", "COM2", "COM3", "COM4",
    "COM5", "COM6", "COM7", "COM8", "COM9", "COM¹", "COM²", "COM³", "LPT0", "LPT1", "LPT2", "LPT3",
    "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9", "LPT¹", "LPT²", "LPT³",
];
/// Kinds of file Explorer reads by itself when it shows the folder, which
/// can make it fetch an icon from a remote share and hand over the user's
/// credentials. Saved with `.download` added, so they are only data.
const INERT_EXTENSIONS: &[&str] = &["url", "lnk", "scf", "library-ms", "searchconnector-ms"];
const INERT_NAMES: &[&str] = &["desktop.ini"];

#[derive(Debug)]
pub enum UploadMsg {
    Offer {
        id: FileId,
        name: String,
        size: u64,
    },
    /// One binary message from the `file` channel.
    Data(Vec<u8>),
    Cancel(FileId),
}

/// The upload receiver of one browser session; dropping it deletes a file
/// that is only partly received.
pub struct UploadSession {
    tx: mpsc::Sender<UploadMsg>,
    task: AbortHandle,
}

impl UploadSession {
    pub fn start(
        dir: Option<PathBuf>,
        control: mpsc::Sender<HostControl>,
        events: mpsc::UnboundedSender<Event>,
    ) -> Self {
        let (tx, mut rx) = mpsc::channel(QUEUE);
        let mut receiver = Receiver {
            dir,
            control,
            events,
            current: None,
            files: 0,
            bytes: 0,
        };
        let task = tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                receiver.handle(msg);
            }
        })
        .abort_handle();
        Self { tx, task }
    }

    pub fn send(&self, msg: UploadMsg) {
        if let UploadMsg::Data(d) = &msg {
            if d.len() > FILE_CHUNK_HEADER + FILE_CHUNK_MAX {
                // Oversized; the upload in progress fails on the gap.
                tracing::warn!(len = d.len(), "dropping an oversized file chunk");
                return;
            }
        }
        // The browser sent past its window. The upload in progress fails on
        // the gap this leaves.
        if let Err(mpsc::error::TrySendError::Full(UploadMsg::Data(_))) = self.tx.try_send(msg) {
            tracing::warn!("upload queue full; dropping file data");
        }
    }
}

impl Drop for UploadSession {
    fn drop(&mut self) {
        // The task owns the partial file; aborting it drops (deletes) it.
        self.task.abort();
    }
}

struct Receiver {
    dir: Option<PathBuf>,
    control: mpsc::Sender<HostControl>,
    events: mpsc::UnboundedSender<Event>,
    current: Option<Incoming>,
    /// Files accepted this session, and their total size.
    files: u32,
    bytes: u64,
}

/// A file being received. Dropped before it is complete, it deletes its
/// partial file and the name it held.
struct Incoming {
    id: FileId,
    name: String,
    size: u64,
    received: u64,
    acked: u64,
    file: Option<File>,
    part: PathBuf,
    path: PathBuf,
    done: bool,
}

impl Drop for Incoming {
    fn drop(&mut self) {
        if !self.done {
            self.file = None;
            let _ = std::fs::remove_file(&self.part);
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

impl Receiver {
    fn handle(&mut self, msg: UploadMsg) {
        match msg {
            UploadMsg::Offer { id, name, size } => self.offer(id, &name, size),
            UploadMsg::Data(data) => self.data(&data),
            UploadMsg::Cancel(id) => {
                if let Some(cur) = self.current.take_if(|c| c.id == id) {
                    tracing::info!(name = %cur.name, "the browser stopped sending a file");
                    self.event(FileEvent::ReceiveFailed {
                        name: cur.name.clone(),
                        message: "stopped by the browser".into(),
                    });
                }
            }
        }
    }

    fn offer(&mut self, id: FileId, name: &str, size: u64) {
        let Some(dir) = self.dir.clone() else {
            self.reply(HostControl::UploadFailed {
                id,
                message: "this host doesn't accept files; allow it in the host's settings".into(),
            });
            return;
        };
        if self.current.is_some() {
            self.reply(HostControl::UploadFailed {
                id,
                message: "another file is still coming".into(),
            });
            return;
        }
        if size > MAX_SIZE {
            self.reply(HostControl::UploadFailed {
                id,
                message: format!("too big: the host takes files up to {} GiB", MAX_SIZE >> 30),
            });
            return;
        }
        if self.files >= SESSION_FILES || self.bytes + size > SESSION_BYTES {
            self.reply(HostControl::UploadFailed {
                id,
                message: format!(
                    "the host takes up to {SESSION_FILES} files or {} GiB per connection",
                    SESSION_BYTES >> 30
                ),
            });
            return;
        }
        self.files += 1;
        self.bytes += size;
        let name = safe_name(name);
        let reserved = block_in_place(|| reserve(&dir, &name));
        let (path, part, file) = match reserved {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("receive {name:?}: {e}");
                self.reply(HostControl::UploadFailed {
                    id,
                    message: format!("the host can't save it: {e}"),
                });
                return;
            }
        };
        let name = file_name(&path);
        tracing::info!(path = %path.display(), size, "receiving a file from the browser");
        self.current = Some(Incoming {
            id,
            name: name.clone(),
            size,
            received: 0,
            acked: 0,
            file: Some(file),
            part,
            path,
            done: false,
        });
        self.reply(HostControl::UploadAccepted { id });
        self.event(FileEvent::Receiving { name, size });
        if size == 0 {
            self.finish();
        }
    }

    fn data(&mut self, msg: &[u8]) {
        let Some((id, offset, bytes)) = parse_file_chunk(msg) else {
            return;
        };
        let Some(cur) = self.current.as_mut().filter(|c| c.id == id) else {
            // Left over from an upload that ended.
            return;
        };
        if offset != cur.received {
            self.fail("some of the file went missing on the way");
            return;
        }
        if bytes.len() as u64 > cur.size - cur.received {
            self.fail("more bytes than the file's size");
            return;
        }
        let written = block_in_place(|| {
            cur.file
                .as_mut()
                .expect("open while receiving")
                .write_all(bytes)
        });
        if let Err(e) = written {
            self.fail(&format!("the host can't write it: {e}"));
            return;
        }
        cur.received += bytes.len() as u64;
        if cur.received == cur.size {
            self.finish();
        } else if cur.received - cur.acked >= ACK_EVERY {
            cur.acked = cur.received;
            let (id, received) = (cur.id, cur.received);
            self.reply(HostControl::UploadAck { id, received });
        }
    }

    fn finish(&mut self) {
        let Some(mut cur) = self.current.take() else {
            return;
        };
        let file = cur.file.take().expect("open while receiving");
        let saved = block_in_place(|| -> std::io::Result<()> {
            file.sync_all()?;
            drop(file);
            // Marked before it gets its name: the stream moves with it.
            mark_downloaded(&cur.part);
            // Replaces the empty file that held the name.
            std::fs::rename(&cur.part, &cur.path)?;
            Ok(())
        });
        if let Err(e) = saved {
            let message = format!("the host can't save it: {e}");
            tracing::warn!(name = %cur.name, "{message}");
            self.reply(HostControl::UploadFailed {
                id: cur.id,
                message: message.clone(),
            });
            self.event(FileEvent::ReceiveFailed {
                name: cur.name.clone(),
                message,
            });
            return;
        }
        cur.done = true;
        tracing::info!(path = %cur.path.display(), "received a file from the browser");
        self.reply(HostControl::UploadAck {
            id: cur.id,
            received: cur.size,
        });
        self.reply(HostControl::UploadSaved {
            id: cur.id,
            name: cur.name.clone(),
        });
        self.event(FileEvent::Received {
            name: cur.name.clone(),
            path: cur.path.clone(),
        });
    }

    fn fail(&mut self, message: &str) {
        let Some(cur) = self.current.take() else {
            return;
        };
        tracing::warn!(name = %cur.name, "receiving failed: {message}");
        self.reply(HostControl::UploadFailed {
            id: cur.id,
            message: message.into(),
        });
        self.event(FileEvent::ReceiveFailed {
            name: cur.name.clone(),
            message: message.into(),
        });
    }

    fn reply(&self, msg: HostControl) {
        if self.control.try_send(msg).is_err() {
            tracing::warn!("control queue full; dropping an upload message");
        }
    }

    fn event(&self, e: FileEvent) {
        let _ = self.events.send(Event::File(e));
    }
}

/// A bare file name Windows takes as it is: no folders, no `:` (a stream),
/// no device name, no trailing dots or spaces, nothing invisible, and
/// nothing Explorer acts on by itself.
fn safe_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = base
        .chars()
        .filter(|&c| c.is_control() || names::printable(c))
        .map(|c| {
            if c.is_control() || r#"<>:"|?*"#.contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    let trim = |s: &str| {
        s.trim_matches(|c: char| c == '.' || c.is_whitespace())
            .to_string()
    };
    let mut name = trim(names::truncate_utf16(&trim(&cleaned), NAME_MAX));
    let stem = name.split('.').next().unwrap_or("").trim_end();
    if RESERVED.iter().any(|r| r.eq_ignore_ascii_case(stem)) {
        name.insert(0, '_');
    }
    let lower = name.to_lowercase();
    let ext = lower.rsplit_once('.').map(|(_, e)| e);
    if INERT_NAMES.contains(&lower.as_str()) || ext.is_some_and(|e| INERT_EXTENSIONS.contains(&e)) {
        name.push_str(".download");
    }
    if name.is_empty() { "file".into() } else { name }
}

/// `name`, or `name (2)`, `name (3)`... before the extension.
fn numbered(name: &str, n: u32) -> String {
    if n == 1 {
        return name.into();
    }
    match name.rfind('.').filter(|&i| i > 0) {
        Some(i) => format!("{} ({n}){}", &name[..i], &name[i..]),
        None => format!("{name} ({n})"),
    }
}

/// Takes the first free name in `dir`: creates it empty (so nothing else
/// takes it) and its `.part` file to write into.
fn reserve(dir: &Path, name: &str) -> std::io::Result<(PathBuf, PathBuf, File)> {
    std::fs::create_dir_all(dir)?;
    let create = |p: &Path| {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(p)
    };
    for n in 1..=1000 {
        let path = dir.join(numbered(name, n));
        let part = dir.join(format!("{}.part", numbered(name, n)));
        match create(&path) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
        match create(&part) {
            Ok(file) => return Ok((path, part, file)),
            Err(e) => {
                let _ = std::fs::remove_file(&path);
                if e.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(e);
                }
            }
        }
    }
    Err(std::io::Error::other("too many files with that name"))
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The Mark of the Web: Windows treats the file as downloaded from the
/// internet. Best effort (FAT drives have no streams).
fn mark_downloaded(path: &Path) {
    #[cfg(windows)]
    {
        let mut stream = path.as_os_str().to_owned();
        stream.push(":Zone.Identifier");
        if let Err(e) = std::fs::write(&stream, "[ZoneTransfer]\r\nZoneId=3\r\n") {
            tracing::debug!(path = %path.display(), "no mark of the web: {e}");
        }
    }
    #[cfg(not(windows))]
    let _ = path;
}

#[cfg(test)]
mod tests {
    use ws_proto::file_chunk;

    use super::*;

    struct Harness {
        rx: Receiver,
        control: mpsc::Receiver<HostControl>,
        dir: PathBuf,
    }

    impl Harness {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("ws-up-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let (control_tx, control) = mpsc::channel(64);
            let (events, _) = mpsc::unbounded_channel();
            Self {
                rx: Receiver {
                    dir: Some(dir.clone()),
                    control: control_tx,
                    events,
                    current: None,
                    files: 0,
                    bytes: 0,
                },
                control,
                dir,
            }
        }

        fn replies(&mut self) -> Vec<HostControl> {
            std::iter::from_fn(|| self.control.try_recv().ok()).collect()
        }

        fn files(&self) -> Vec<String> {
            let mut names: Vec<_> = std::fs::read_dir(&self.dir)
                .map(|d| {
                    d.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default();
            names.sort();
            names
        }

        fn send(&mut self, id: u64, data: &[u8]) {
            for (i, part) in data.chunks(16 * 1024).enumerate() {
                let msg = file_chunk(FileId(id), (i * 16 * 1024) as u64, part);
                self.rx.handle(UploadMsg::Data(msg));
            }
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn offer(id: u64, name: &str, size: usize) -> UploadMsg {
        UploadMsg::Offer {
            id: FileId(id),
            name: name.into(),
            size: size as u64,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn saves_a_file_acks_and_never_overwrites() {
        let mut h = Harness::new("save");
        let data: Vec<u8> = (0..600_000u32).map(|i| i as u8).collect();
        h.rx.handle(offer(1, "../../notes.txt", data.len()));
        assert!(matches!(
            h.replies()[..],
            [HostControl::UploadAccepted { id: FileId(1) }]
        ));
        h.send(1, &data);
        let replies = h.replies();
        let acks: Vec<u64> = replies
            .iter()
            .filter_map(|r| match r {
                HostControl::UploadAck { received, .. } => Some(*received),
                _ => None,
            })
            .collect();
        assert_eq!(acks, vec![262_144, 524_288, 600_000]);
        assert!(matches!(
            replies.last(),
            Some(HostControl::UploadSaved { name, .. }) if name == "notes.txt"
        ));
        assert_eq!(std::fs::read(h.dir.join("notes.txt")).unwrap(), data);

        h.rx.handle(offer(2, "notes.txt", 2));
        h.send(2, b"hi");
        assert!(matches!(
            h.replies().last(),
            Some(HostControl::UploadSaved { name, .. }) if name == "notes (2).txt"
        ));
        assert_eq!(h.files(), vec!["notes (2).txt", "notes.txt"]);
        assert_eq!(std::fs::read(h.dir.join("notes.txt")).unwrap(), data);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancel_gaps_and_dropping_leave_nothing() {
        let mut h = Harness::new("cancel");
        h.rx.handle(offer(1, "a.bin", 100_000));
        h.send(1, &[1; 20_000]);
        assert_eq!(h.files(), vec!["a.bin", "a.bin.part"]);
        h.rx.handle(UploadMsg::Cancel(FileId(1)));
        assert!(h.files().is_empty());

        // A missing chunk fails the upload.
        h.rx.handle(offer(2, "b.bin", 100_000));
        h.rx.handle(UploadMsg::Data(file_chunk(FileId(2), 16_384, &[0; 10])));
        assert!(matches!(
            h.replies().last(),
            Some(HostControl::UploadFailed { id: FileId(2), .. })
        ));
        assert!(h.files().is_empty());

        // The session ends mid-file.
        h.rx.handle(offer(3, "c.bin", 100_000));
        h.send(3, &[1; 10]);
        h.rx.current = None;
        assert!(h.files().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn refuses_when_off_too_big_busy_or_overlong() {
        let mut h = Harness::new("refuse");
        h.rx.handle(offer(1, "big", (MAX_SIZE + 1) as usize));
        h.rx.handle(offer(2, "a", 10));
        h.rx.handle(offer(3, "b", 10));
        h.send(2, &[0; 11]);
        let replies = h.replies();
        assert!(matches!(
            replies[0],
            HostControl::UploadFailed { id: FileId(1), .. }
        ));
        assert!(matches!(
            replies[1],
            HostControl::UploadAccepted { id: FileId(2) }
        ));
        assert!(matches!(
            replies[2],
            HostControl::UploadFailed { id: FileId(3), .. }
        ));
        assert!(matches!(
            replies[3],
            HostControl::UploadFailed { id: FileId(2), .. }
        ));

        // A session can't send endless files.
        h.rx.files = SESSION_FILES;
        h.rx.handle(offer(5, "a", 1));
        assert!(matches!(
            h.replies()[..],
            [HostControl::UploadFailed { id: FileId(5), .. }]
        ));

        h.rx.dir = None;
        h.rx.handle(offer(4, "a", 1));
        assert!(matches!(
            h.replies()[..],
            [HostControl::UploadFailed { id: FileId(4), .. }]
        ));
        assert!(h.files().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn empty_files_are_saved_at_once() {
        let mut h = Harness::new("empty");
        h.rx.handle(offer(1, "empty.txt", 0));
        assert!(matches!(
            h.replies().last(),
            Some(HostControl::UploadSaved { .. })
        ));
        assert_eq!(h.files(), vec!["empty.txt"]);
    }

    #[test]
    fn names_windows_takes_as_they_are() {
        assert_eq!(safe_name("report.pdf"), "report.pdf");
        assert_eq!(safe_name("C:\\Users\\x\\evil.exe"), "evil.exe");
        assert_eq!(safe_name("../../.."), "file");
        assert_eq!(safe_name("a.txt:Zone.Identifier"), "a.txt_Zone.Identifier");
        assert_eq!(safe_name("con.txt"), "_con.txt");
        assert_eq!(safe_name("LPT1"), "_LPT1");
        assert_eq!(safe_name("notes. . "), "notes");
        assert_eq!(safe_name("tab\there?.txt"), "tab_here_.txt");
        assert_eq!(safe_name(&"y".repeat(500)).len(), NAME_MAX);
        assert_eq!(
            safe_name(&"😀".repeat(150)).encode_utf16().count(),
            NAME_MAX
        );
        assert_eq!(safe_name("pic\u{202E}gpj.exe"), "picgpj.exe");
        assert_eq!(safe_name("COM¹.txt"), "_COM¹.txt");
        assert_eq!(safe_name("conin$"), "_conin$");
        assert_eq!(safe_name("LPT0.log"), "_LPT0.log");
        assert_eq!(safe_name("evil.URL"), "evil.URL.download");
        assert_eq!(safe_name("x.lnk"), "x.lnk.download");
        assert_eq!(safe_name("Desktop.ini"), "Desktop.ini.download");
        assert_eq!(safe_name("notes.url.txt"), "notes.url.txt");
        assert_eq!(numbered("a.tar.gz", 2), "a.tar (2).gz");
        assert_eq!(numbered(".bashrc", 3), ".bashrc (3)");
        assert_eq!(numbered("README", 2), "README (2)");
    }
}
