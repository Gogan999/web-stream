//! The host's events as lines of text and typed commands, for the
//! service's control pipe (and the test host in `examples`).

use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, mpsc};

use crate::serve::{Command, Event, FileEvent, FileId, Notice};

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
        Event::PairingOpened { link } => Some(format!(
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

/// What a console that connects late is told first: the host's start,
/// where the Cloudflare Tunnel is up, and the browser connected now.
#[derive(Default)]
pub struct Snapshot {
    started: Option<Event>,
    tunnel_url: Option<String>,
    connected: Option<String>,
}

impl Snapshot {
    pub fn update(&mut self, e: &Event) {
        match e {
            Event::Started { .. } => self.started = Some(e.clone()),
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
    let (format, greeting) = match &first {
        Some(line) if line.trim() == EVENTS_JSON => (Format::Json, &greeting[..]),
        _ => (Format::Text, greeting.get(..1).unwrap_or_default()),
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
        };
        assert!(!describe(e.clone()).unwrap().contains("s3cret"));
        assert!(pairing_link_line(&e).unwrap().contains("#pair=s3cret-link"));
        assert_eq!(pairing_link_line(&Event::UnpairedAll), None);
    }
}
