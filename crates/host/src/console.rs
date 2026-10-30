//! The host's events as lines of text and typed commands, for the
//! service's control pipe (and the test host in `examples`).

use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, mpsc};

use crate::serve::{Command, Event, FileEvent, FileId, Notice, PAIRING_WINDOW};

pub const CONSOLE_HELP: &str = "\
commands: pair (let a browser pair for two minutes), unpair-all, drop (end the session),
          send <path> (offer a file to the connected browser),
          unshare <id> (stop offering it), quit";

/// Sent first by a console that wants the host's [`Event`]s as JSON, one
/// per line, instead of lines of text (the tray app, attached to the
/// service's host).
pub const EVENTS_JSON: &str = "events json";

/// An event as the host user reads it; `None` for those not worth a line.
pub fn describe(e: Event) -> Option<String> {
    Some(match e {
        Event::Started {
            port,
            fingerprint,
            web_root,
            data_dir,
            ..
        } => {
            let web = match web_root {
                Some(dir) => format!("serving the web client from {}", dir.display()),
                None => "serving the web client built into this host".into(),
            };
            format!(
                "web-stream host on https://<this PC>:{port}/\n\
                 certificate SHA-256 {fingerprint}\n\
                 {web}\n\
                 data in {}\n\
                 {CONSOLE_HELP}",
                data_dir.display()
            )
        }
        // Never the link: these lines are logged.
        Event::PairingOpened { .. } => {
            "pairing is open for two minutes: request a code in the browser".into()
        }
        Event::UnpairedAll => "every paired browser was removed".into(),
        Event::Error(e) | Event::Update(e) => e,
        Event::Notice(n) => describe_notice(n),
        Event::File(f) => return describe_file(f),
    })
}

/// The pairing link's secret as a line for a console the host user reads
/// directly (the test host's stdout), never for a log: it pairs a browser
/// without a code while pairing is open.
pub fn pairing_link_line(e: &Event) -> Option<String> {
    match e {
        Event::PairingOpened { link, .. } => Some(format!(
            "  PAIRING LINK #pair={link}  (open the client's address with this on the end, once)"
        )),
        _ => None,
    }
}

fn describe_notice(n: Notice) -> String {
    match n {
        Notice::PairingCode {
            code,
            device_name,
            expires_in_secs,
        } => format!(
            "\n  PAIRING CODE {code}  for {device_name:?} (expires in {expires_in_secs}s)\n"
        ),
        Notice::Paired { device_name } => format!("paired {device_name:?}"),
        Notice::PairingFailed { attempts_left } => {
            format!("wrong pairing code; {attempts_left} attempts left")
        }
        Notice::PairingLocked => {
            "too many wrong codes: pairing is locked until you type `pair`".into()
        }
        Notice::SessionStarted { device_name } => format!("{device_name:?} connected"),
        Notice::SessionEnded { device_name } => format!("{device_name:?} disconnected"),
        Notice::Tunnel { url: Some(url) } => format!("Cloudflare Tunnel up: {url}/"),
        Notice::Tunnel { url: None } => "Cloudflare Tunnel down".into(),
        Notice::Warning(w) => w,
    }
}

fn describe_file(f: FileEvent) -> Option<String> {
    Some(match f {
        FileEvent::Shared { name, size, .. } => {
            format!("sharing {name:?} ({size} bytes) with the browser")
        }
        FileEvent::Progress { .. } | FileEvent::Cleared => return None,
        FileEvent::Sent { name, .. } => format!("sent {name:?}"),
        FileEvent::Cancelled { name, .. } => format!("the browser stopped receiving {name:?}"),
        FileEvent::Failed { name, message, .. } => format!("sending {name:?} failed: {message}"),
        FileEvent::Withdrawn { name, .. } => format!("stopped sharing {name:?}"),
        FileEvent::Receiving { name, size } => {
            format!("receiving {name:?} ({size} bytes) from the browser")
        }
        FileEvent::Received { path, .. } => format!("received {}", path.display()),
        FileEvent::ReceiveFailed { name, message } => {
            format!("receiving {name:?} failed: {message}")
        }
    })
}

/// The pairing window as last heard, with when it ends.
struct OpenWindow {
    link: String,
    until: Instant,
}

/// The pairing code as last heard, with when it ends.
struct IssuedCode {
    code: String,
    device_name: String,
    until: Instant,
}

/// What a console that connects late is told first: the host's start,
/// where the Cloudflare Tunnel is up, the browser connected now, and the
/// pairing in progress.
///
/// Pairing events carry time relative to when they were sent, so the
/// snapshot turns them into deadlines and, at greeting time, back into what
/// remains; whatever has run out is left out. The aim is that a console
/// ends up in the state it would have reached had it listened all along.
/// That follows the tray app's rules: opening pairing lifts a lockout (as
/// the host does), a lockout withdraws the code, and a completed pairing
/// ends the window and the code but not a lockout. The lockout lasts until
/// pairing is opened again, however long ago the window closed.
#[derive(Default)]
pub struct Snapshot {
    started: Option<Event>,
    tunnel_url: Option<String>,
    connected: Option<String>,
    window: Option<OpenWindow>,
    code: Option<IssuedCode>,
    locked: bool,
}

impl Snapshot {
    pub fn update(&mut self, e: &Event) {
        self.update_at(e, Instant::now());
    }

    /// [`update`](Self::update) for an event heard at `now`.
    fn update_at(&mut self, e: &Event, now: Instant) {
        match e {
            Event::Started { .. } => self.started = Some(e.clone()),
            Event::PairingOpened {
                link,
                remaining_secs,
            } => {
                let window = remaining_secs.map_or(PAIRING_WINDOW, Duration::from_secs);
                self.window = Some(OpenWindow {
                    link: link.clone(),
                    until: now + window,
                });
                self.locked = false;
            }
            Event::Notice(Notice::PairingCode {
                code,
                device_name,
                expires_in_secs,
            }) => {
                self.code = Some(IssuedCode {
                    code: code.clone(),
                    device_name: device_name.clone(),
                    until: now + Duration::from_secs(*expires_in_secs),
                });
            }
            // A failed attempt changes nothing the tray app shows beyond
            // its log, so it is not recorded.
            Event::Notice(Notice::Paired { .. }) => {
                self.window = None;
                self.code = None;
            }
            Event::Notice(Notice::PairingLocked) => {
                self.code = None;
                self.locked = true;
            }
            Event::Notice(Notice::Tunnel { url }) => self.tunnel_url = url.clone(),
            Event::Notice(Notice::SessionStarted { device_name }) => {
                self.connected = Some(device_name.clone())
            }
            Event::Notice(Notice::SessionEnded { device_name })
                if self.connected.as_ref() == Some(device_name) =>
            {
                self.connected = None
            }
            _ => {}
        }
    }

    pub fn events(&self) -> Vec<Event> {
        self.events_at(Instant::now())
    }

    /// [`events`](Self::events) as of `now`.
    fn events_at(&self, now: Instant) -> Vec<Event> {
        let mut out: Vec<Event> = self.started.iter().cloned().collect();
        if let Some(url) = &self.tunnel_url {
            out.push(Event::Notice(Notice::Tunnel {
                url: Some(url.clone()),
            }));
        }
        if let Some(device_name) = &self.connected {
            out.push(Event::Notice(Notice::SessionStarted {
                device_name: device_name.clone(),
            }));
        }
        // Opened before locked: hearing the opening lifts a lockout.
        // Less than a whole second left is as good as over: it would be
        // sent as zero, a state the console would only tidy away.
        let live = |until: Instant| until >= now + Duration::from_secs(1);
        if let Some(w) = self.window.as_ref().filter(|w| live(w.until)) {
            out.push(Event::PairingOpened {
                link: w.link.clone(),
                remaining_secs: Some((w.until - now).as_secs()),
            });
        }
        if let Some(c) = self.code.as_ref().filter(|c| live(c.until)) {
            out.push(Event::Notice(Notice::PairingCode {
                code: c.code.clone(),
                device_name: c.device_name.clone(),
                expires_in_secs: (c.until - now).as_secs(),
            }));
        }
        if self.locked {
            out.push(Event::Notice(Notice::PairingLocked));
        }
        out
    }
}

/// How a console reads the host's events.
#[derive(Clone, Copy, PartialEq)]
enum Format {
    Text,
    Json,
}

/// `e` as the console reads it, ending in a line break; `None` if there
/// is nothing to say.
fn format_event(e: &Event, format: Format) -> Option<String> {
    match format {
        Format::Text => describe(e.clone()).map(|t| format!("{t}\n")),
        Format::Json => serde_json::to_string(e).ok().map(|j| format!("{j}\n")),
    }
}

/// How long a console has to ask for JSON ([`EVENTS_JSON`]) before it is
/// greeted in text.
const ASK_FOR_JSON: Duration = Duration::from_millis(300);

/// One console on the service's control pipe: commands typed into it go to
/// the host, the host's events come back. `greeting` is the snapshot of
/// the host when it connected (see [`Snapshot`]).
pub async fn session<S: AsyncRead + AsyncWrite>(
    client: S,
    commands: mpsc::Sender<Command>,
    mut events: broadcast::Receiver<Event>,
    greeting: Vec<Event>,
) {
    let (read, mut write) = tokio::io::split(client);
    let mut read = BufReader::new(read).lines();
    // A console that wants JSON says so first, before it is greeted; one
    // typed into by hand gets the host's start in text.
    let first = match tokio::time::timeout(ASK_FOR_JSON, read.next_line()).await {
        Ok(Ok(Some(line))) => Some(line),
        Ok(_) => return,
        Err(_) => None,
    };
    // A text console is told only of the host's start: the rest of the
    // greeting is state for the tray app.
    let (format, greeting): (_, Vec<&Event>) = match &first {
        Some(line) if line.trim() == EVENTS_JSON => (Format::Json, greeting.iter().collect()),
        _ => (
            Format::Text,
            greeting
                .iter()
                .take(1)
                .filter(|e| matches!(e, Event::Started { .. }))
                .collect(),
        ),
    };
    for e in greeting {
        let Some(text) = format_event(e, format) else {
            continue;
        };
        if write.write_all(text.as_bytes()).await.is_err() {
            return;
        }
    }
    let mut pending = first.filter(|_| format == Format::Text);
    loop {
        let line = match pending.take() {
            Some(line) => Some(line),
            None => tokio::select! {
                line = read.next_line() => {
                    let Ok(Some(line)) = line else { return };
                    Some(line)
                }
                e = events.recv() => {
                    match e {
                        Ok(e) => {
                            let Some(text) = format_event(&e, format) else { continue };
                            if write.write_all(text.as_bytes()).await.is_err() {
                                return;
                            }
                        }
                        // A JSON console that fell this far behind has
                        // missed events it can't do without: it connects
                        // again.
                        Err(broadcast::error::RecvError::Lagged(_)) if format == Format::Json => {
                            return;
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => {}
                        Err(broadcast::error::RecvError::Closed) => return,
                    }
                    None
                }
            },
        };
        let Some(line) = line else { continue };
        match parse_line(&line) {
            Line::Command(cmd) => {
                if commands.send(cmd).await.is_err() {
                    return;
                }
            }
            Line::Empty => {}
            Line::Help if format == Format::Json => {}
            Line::Help => {
                let help = format!("{CONSOLE_HELP}\n(quit restarts the host)\n");
                if write.write_all(help.as_bytes()).await.is_err() {
                    return;
                }
            }
        }
    }
}

/// A line typed by the host user.
#[derive(Debug, PartialEq)]
pub enum Line {
    Command(Command),
    Help,
    Empty,
}

pub fn parse_line(line: &str) -> Line {
    let line = line.trim();
    if let Some(path) = line.strip_prefix("send ") {
        let path = path.trim().trim_matches('"');
        return Line::Command(Command::ShareFiles(vec![path.into()]));
    }
    if let Some(id) = line.strip_prefix("unshare ") {
        return match id.trim().parse() {
            Ok(id) => Line::Command(Command::StopSharing(FileId(id))),
            Err(_) => Line::Help,
        };
    }
    Line::Command(match line {
        "pair" | "p" => Command::OpenPairing,
        "drop" | "d" => Command::DropSession,
        "unpair-all" => Command::UnpairAll,
        "quit" | "q" | "exit" => Command::Quit,
        "" => return Line::Empty,
        _ => return Line::Help,
    })
}

/// The lines that type `cmd` into a console ([`parse_line`] reads them
/// back). A path with a line break in it can't be typed: it is left out.
pub fn command_lines(cmd: &Command) -> Vec<String> {
    match cmd {
        Command::OpenPairing => vec!["pair".into()],
        Command::DropSession => vec!["drop".into()],
        Command::UnpairAll => vec!["unpair-all".into()],
        Command::ShareFiles(paths) => paths
            .iter()
            .map(|p| p.display().to_string())
            .filter(|p| !p.contains(['\r', '\n']))
            .map(|p| format!("send {p}"))
            .collect(),
        Command::StopSharing(id) => vec![format!("unshare {}", id.0)],
        Command::Quit => vec!["quit".into()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines() {
        assert_eq!(parse_line(" pair "), Line::Command(Command::OpenPairing));
        assert_eq!(parse_line("q"), Line::Command(Command::Quit));
        assert_eq!(
            parse_line(r#"send "C:\a b.txt""#),
            Line::Command(Command::ShareFiles(vec![r"C:\a b.txt".into()]))
        );
        assert_eq!(parse_line(""), Line::Empty);
        assert_eq!(parse_line("bogus"), Line::Help);
        assert_eq!(parse_line("unshare x"), Line::Help);
    }

    #[test]
    fn commands_round_trip_as_lines() {
        for cmd in [
            Command::OpenPairing,
            Command::DropSession,
            Command::UnpairAll,
            Command::StopSharing(FileId(42)),
            Command::Quit,
            Command::ShareFiles(vec![r"C:\a b\c.txt".into()]),
        ] {
            let lines = command_lines(&cmd);
            assert_eq!(lines.len(), 1, "{cmd:?}");
            assert_eq!(parse_line(&lines[0]), Line::Command(cmd));
        }
        let two = command_lines(&Command::ShareFiles(vec!["a".into(), "b\nquit".into()]));
        assert_eq!(two, ["send a"]);
    }

    fn started() -> Event {
        Event::Started {
            port: 8443,
            ips: vec![],
            fingerprint: "AB:CD".into(),
            self_signed: true,
            web_root: None,
            data_dir: "data".into(),
        }
    }

    #[test]
    fn a_late_console_hears_where_the_tunnel_is() {
        let mut snapshot = Snapshot::default();
        snapshot.update(&started());
        let up = Event::Notice(Notice::Tunnel {
            url: Some("https://pc.example.com".into()),
        });
        snapshot.update(&up);
        assert_eq!(snapshot.events(), vec![started(), up]);
        snapshot.update(&Event::Notice(Notice::Tunnel { url: None }));
        assert_eq!(snapshot.events(), vec![started()]);
    }

    fn opened(remaining_secs: Option<u64>) -> Event {
        Event::PairingOpened {
            link: "secret".into(),
            remaining_secs,
        }
    }

    fn code(expires_in_secs: u64) -> Event {
        Event::Notice(Notice::PairingCode {
            code: "123456".into(),
            device_name: "phone".into(),
            expires_in_secs,
        })
    }

    #[test]
    fn a_console_attaching_while_pairing_is_open_hears_what_is_left() {
        let t = Instant::now();
        let mut snapshot = Snapshot::default();
        snapshot.update_at(&started(), t);
        snapshot.update_at(&opened(Some(120)), t);
        let later = t + Duration::from_secs(45);
        assert_eq!(snapshot.events_at(later), vec![started(), opened(Some(75))]);
    }

    #[test]
    fn a_console_attaching_after_a_code_hears_it_with_its_time_left() {
        let t = Instant::now();
        let mut snapshot = Snapshot::default();
        snapshot.update_at(&opened(Some(120)), t);
        snapshot.update_at(&code(120), t + Duration::from_secs(10));
        // The window ends before the code does: only the code is left.
        assert_eq!(
            snapshot.events_at(t + Duration::from_secs(100)),
            vec![opened(Some(20)), code(30)]
        );
        assert_eq!(
            snapshot.events_at(t + Duration::from_secs(125)),
            vec![code(5)]
        );
    }

    #[test]
    fn expired_pairing_is_not_replayed() {
        let t = Instant::now();
        let mut snapshot = Snapshot::default();
        snapshot.update_at(&opened(Some(120)), t);
        snapshot.update_at(&code(120), t);
        assert!(snapshot.events_at(t + Duration::from_secs(120)).is_empty());
        assert!(snapshot.events_at(t + Duration::from_secs(500)).is_empty());
        // About half a second left would be sent as zero: left out.
        assert!(
            snapshot
                .events_at(t + Duration::from_millis(119_500))
                .is_empty()
        );
        assert_eq!(
            snapshot.events_at(t + Duration::from_millis(119_000)),
            vec![opened(Some(1)), code(1)]
        );
    }

    #[test]
    fn a_finished_pairing_is_not_replayed_as_open() {
        let t = Instant::now();
        let mut snapshot = Snapshot::default();
        snapshot.update_at(&opened(Some(120)), t);
        snapshot.update_at(&code(120), t);
        snapshot.update_at(
            &Event::Notice(Notice::Paired {
                device_name: "phone".into(),
            }),
            t,
        );
        assert!(snapshot.events_at(t + Duration::from_secs(1)).is_empty());
    }

    #[test]
    fn a_missing_remaining_time_means_a_full_window_not_an_expired_one() {
        // From a build that predates the field.
        let old = r#"{"PairingOpened":{"link":"secret"}}"#;
        let e: Event = serde_json::from_str(old).unwrap();
        assert_eq!(e, opened(None));
        let t = Instant::now();
        let mut snapshot = Snapshot::default();
        snapshot.update_at(&e, t);
        assert_eq!(
            snapshot.events_at(t + Duration::from_secs(20)),
            vec![opened(Some(PAIRING_WINDOW.as_secs() - 20))]
        );
    }

    #[test]
    fn a_lockout_follows_the_opening_and_is_lifted_by_the_next() {
        let t = Instant::now();
        let locked = Event::Notice(Notice::PairingLocked);
        let mut snapshot = Snapshot::default();
        snapshot.update_at(&opened(Some(120)), t);
        snapshot.update_at(&code(120), t);
        snapshot.update_at(&locked, t);
        // The code is withdrawn; the link works on, and the lockout comes
        // after the opening, which would lift it.
        assert_eq!(
            snapshot.events_at(t + Duration::from_secs(10)),
            vec![opened(Some(110)), locked.clone()]
        );
        // The lockout outlives the window.
        assert_eq!(
            snapshot.events_at(t + Duration::from_secs(200)),
            vec![locked]
        );
        snapshot.update_at(&opened(Some(120)), t + Duration::from_secs(200));
        assert_eq!(
            snapshot.events_at(t + Duration::from_secs(200)),
            vec![opened(Some(120))]
        );
    }

    #[tokio::test]
    async fn a_text_console_is_not_told_of_pairing() {
        let (ours, theirs) = tokio::io::duplex(4096);
        let (commands, _commands_rx) = mpsc::channel(8);
        let (events, events_rx) = broadcast::channel(16);
        let mut snapshot = Snapshot::default();
        snapshot.update(&started());
        snapshot.update(&opened(Some(120)));
        snapshot.update(&code(120));
        assert_eq!(snapshot.events().len(), 3);
        tokio::spawn(session(theirs, commands, events_rx, snapshot.events()));
        let (read, write) = tokio::io::split(ours);
        let mut read = BufReader::new(read).lines();
        let first = read.next_line().await.unwrap().unwrap();
        assert!(first.starts_with("web-stream host on https://"), "{first}");
        events.send(Event::UnpairedAll).unwrap();
        // The start is several lines; nothing about pairing comes before
        // the live event.
        loop {
            let line = read.next_line().await.unwrap().unwrap();
            assert!(!line.to_lowercase().contains("pairing"), "{line}");
            if line.starts_with("every paired browser") {
                break;
            }
        }
        drop(write);
    }

    /// A console on one end of an in-memory pipe, the session on the other.
    fn console() -> (
        tokio::io::DuplexStream,
        mpsc::Receiver<Command>,
        broadcast::Sender<Event>,
    ) {
        let (ours, theirs) = tokio::io::duplex(4096);
        let (commands, commands_rx) = mpsc::channel(8);
        let (events, events_rx) = broadcast::channel(16);
        let mut snapshot = Snapshot::default();
        snapshot.update(&started());
        snapshot.update(&Event::Notice(Notice::SessionStarted {
            device_name: "phone".into(),
        }));
        tokio::spawn(session(theirs, commands, events_rx, snapshot.events()));
        (ours, commands_rx, events)
    }

    #[tokio::test]
    async fn a_json_console_gets_only_json() {
        let (ours, mut commands, events) = console();
        let (read, mut write) = tokio::io::split(ours);
        let mut read = BufReader::new(read).lines();
        write
            .write_all(format!("{EVENTS_JSON}\n").as_bytes())
            .await
            .unwrap();
        let next = |line: Option<String>| serde_json::from_str::<Event>(&line.unwrap()).unwrap();
        assert_eq!(next(read.next_line().await.unwrap()), started());
        assert_eq!(
            next(read.next_line().await.unwrap()),
            Event::Notice(Notice::SessionStarted {
                device_name: "phone".into()
            })
        );
        let link = Event::PairingOpened {
            link: "secret".into(),
            remaining_secs: Some(120),
        };
        events.send(link.clone()).unwrap();
        assert_eq!(next(read.next_line().await.unwrap()), link);
        write.write_all(b"pair\nbogus\n").await.unwrap();
        assert_eq!(commands.recv().await, Some(Command::OpenPairing));
        // No help text for a JSON console.
        events.send(Event::UnpairedAll).unwrap();
        assert_eq!(next(read.next_line().await.unwrap()), Event::UnpairedAll);
    }

    #[tokio::test]
    async fn a_console_typed_into_is_greeted_in_text() {
        let (ours, mut commands, _events) = console();
        let (read, mut write) = tokio::io::split(ours);
        let mut read = BufReader::new(read).lines();
        // Says nothing at first: greeted after a moment.
        let first = read.next_line().await.unwrap().unwrap();
        assert!(first.starts_with("web-stream host on https://"), "{first}");
        write.write_all(b"drop\n").await.unwrap();
        assert_eq!(commands.recv().await, Some(Command::DropSession));

        // A command typed at once is carried out after the greeting.
        let (ours, mut commands, _events) = console();
        let (read, mut write) = tokio::io::split(ours);
        let mut read = BufReader::new(read).lines();
        write.write_all(b"pair\n").await.unwrap();
        assert_eq!(commands.recv().await, Some(Command::OpenPairing));
        let first = read.next_line().await.unwrap().unwrap();
        assert!(first.starts_with("web-stream host on https://"), "{first}");
    }

    #[test]
    fn events_cross_as_json() {
        let e = Event::Started {
            port: 8443,
            ips: vec!["192.168.1.5".parse().unwrap()],
            fingerprint: "AB:CD".into(),
            self_signed: true,
            web_root: None,
            data_dir: "data".into(),
        };
        let line = serde_json::to_string(&e).unwrap();
        assert!(!line.contains('\n'));
        assert_eq!(serde_json::from_str::<Event>(&line).unwrap(), e);
        let f = Event::File(FileEvent::Shared {
            id: FileId(7),
            name: "a.txt".into(),
            size: 3,
        });
        assert_eq!(
            serde_json::from_str::<Event>(&serde_json::to_string(&f).unwrap()).unwrap(),
            f
        );
    }

    #[test]
    fn the_pairing_link_stays_out_of_logged_lines() {
        let e = Event::PairingOpened {
            link: "s3cret-link".into(),
            remaining_secs: None,
        };
        assert!(!describe(e.clone()).unwrap().contains("s3cret"));
        assert!(pairing_link_line(&e).unwrap().contains("#pair=s3cret-link"));
        assert_eq!(pairing_link_line(&Event::UnpairedAll), None);
    }
}
