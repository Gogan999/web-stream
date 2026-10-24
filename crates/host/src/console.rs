//! The host's events as lines of text and typed commands, for the
//! service's control pipe (and the test host in `examples`).

use crate::serve::{Command, Event, FileEvent, Notice};

pub const CONSOLE_HELP: &str = "\
commands: pair (let a browser pair for two minutes), unpair-all, drop (end the session),
          send <path> (offer a file to the connected browser), quit";

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
        Event::PairingOpened { link } => format!(
            "pairing is open for two minutes: request a code in the browser, or open the \
             client's address with this on the end (once)\n  PAIRING LINK #pair={link}\n"
        ),
        Event::UnpairedAll => "every paired browser was removed".into(),
        Event::Error(e) | Event::Update(e) => e,
        Event::Notice(n) => describe_notice(n),
        Event::File(f) => return describe_file(f),
    })
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
    Line::Command(match line {
        "pair" | "p" => Command::OpenPairing,
        "drop" | "d" => Command::DropSession,
        "unpair-all" => Command::UnpairAll,
        "quit" | "q" | "exit" => Command::Quit,
        "" => return Line::Empty,
        _ => return Line::Help,
    })
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
    }
}
