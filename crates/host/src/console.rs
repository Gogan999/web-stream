//! The host's events as lines of text and typed commands, for the
//! service's control pipe (and the test host in `examples`).

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
        Notice::Certificate {
            domain,
            fingerprint,
        } => format!("certificate from Let's Encrypt for {domain} in use, SHA-256 {fingerprint}"),
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
