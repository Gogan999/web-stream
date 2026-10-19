//! Temporary diagnostic log for chasing freezes and unexpected keyframes.
//!
//! Off unless `WS_DIAG` is set when the host starts, or a file named
//! `ws-diag.on` sits next to the executable (for the tray app and the
//! service, which don't inherit a shell's environment). `WS_DIAG=1` or the
//! marker file writes `ws-diag-<unix seconds>.log` next to the executable (or
//! in the temp directory if that isn't writable); any other `WS_DIAG` value
//! is the file path, and `WS_DIAG=0` turns it off even with the marker.
//!
//! Every line is `<unix ms> <host ms> <kind> <key=value ...>`: the first is
//! wall-clock time, to line up with the browser's diagnostic log, the second
//! the host's monotonic clock that frame timestamps use. Lines go through a
//! channel to a writer thread, so a slow disk never holds up capture or the
//! transport; if the writer falls behind, lines are dropped and counted.
//!
//! Meant to come out again: every call site uses [`diag!`], so removing this
//! crate and its uses is a grep.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Lines waiting for the writer thread.
const QUEUE: usize = 16 * 1024;

struct Sink {
    lines: SyncSender<String>,
    path: PathBuf,
    dropped: AtomicU64,
}

static SINK: OnceLock<Option<Sink>> = OnceLock::new();

fn sink() -> Option<&'static Sink> {
    SINK.get_or_init(open).as_ref()
}

/// Whether diagnostics are on. Cheap after the first call.
pub fn enabled() -> bool {
    sink().is_some()
}

/// Where the log goes, when diagnostics are on.
pub fn path() -> Option<PathBuf> {
    sink().map(|s| s.path.clone())
}

/// Writes one line. Use [`diag!`], which skips formatting when off.
pub fn write(kind: &str, args: std::fmt::Arguments<'_>) {
    let Some(sink) = sink() else { return };
    let wall = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let mono = ws_proto::mono_now() as f64 / 1000.0;
    let mut line = String::with_capacity(128);
    let _ = write!(line, "{wall} {mono:.3} {kind} {args}");
    match sink.lines.try_send(line) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => {
            sink.dropped.fetch_add(1, Ordering::Relaxed);
        }
        Err(TrySendError::Disconnected(_)) => {}
    }
}

/// Logs a diagnostic line: `diag!("kind", "key={} ...", value)`.
#[macro_export]
macro_rules! diag {
    ($kind:expr, $($arg:tt)*) => {
        if $crate::enabled() {
            $crate::write($kind, format_args!($($arg)*));
        }
    };
}

fn open() -> Option<Sink> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(PathBuf::from));
    let value = match std::env::var_os("WS_DIAG") {
        Some(v) if v.is_empty() || v == "0" => return None,
        Some(v) => v,
        None if exe_dir
            .as_ref()
            .is_some_and(|d| d.join("ws-diag.on").exists()) =>
        {
            "1".into()
        }
        None => return None,
    };
    let (file, path) = if value == "1" {
        let name = format!(
            "ws-diag-{}.log",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
        );
        exe_dir
            .map(|d| d.join(&name))
            .into_iter()
            .chain([std::env::temp_dir().join(&name)])
            .find_map(|p| std::fs::File::create(&p).ok().map(|f| (f, p)))?
    } else {
        let p = PathBuf::from(value);
        (std::fs::File::create(&p).ok()?, p)
    };
    let (tx, rx) = mpsc::sync_channel::<String>(QUEUE);
    std::thread::Builder::new()
        .name("ws-diag".into())
        .spawn(move || {
            let mut out = std::io::BufWriter::with_capacity(256 * 1024, file);
            // Flush whenever the queue runs dry, and at least every half
            // second, so a crash or a killed process loses little.
            loop {
                match rx.recv_timeout(Duration::from_millis(500)) {
                    Ok(line) => {
                        let _ = writeln!(out, "{line}");
                        while let Ok(line) = rx.try_recv() {
                            let _ = writeln!(out, "{line}");
                        }
                        let _ = out.flush();
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        let _ = out.flush();
                        if let Some(sink) = SINK.get().and_then(Option::as_ref) {
                            let dropped = sink.dropped.swap(0, Ordering::Relaxed);
                            if dropped > 0 {
                                let _ = writeln!(out, "- - diag_dropped lines={dropped}");
                            }
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                }
            }
        })
        .ok()?;
    Some(Sink {
        lines: tx,
        path,
        dropped: AtomicU64::new(0),
    })
}
