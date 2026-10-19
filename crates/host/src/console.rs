//! `ws-host serve`: the host with a small console for the host user.

use std::io::BufRead;

use tokio::sync::mpsc;
use ws_host::options::ServeOptions;
use ws_host::serve::{self, Command, Event, FileEvent, Notice};

pub const CONSOLE_HELP: &str = "\
commands: pair (let a browser pair for two minutes), unpair-all, drop (end the session),
          send <path> (offer a file to the connected browser), quit";

pub async fn run(opts: ServeOptions) -> anyhow::Result<()> {
    let (commands, commands_rx) = mpsc::channel(8);
    let (events_tx, events) = mpsc::unbounded_channel();
    tokio::spawn(print_events(events));
    console(commands.clone());
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = commands.send(Command::Quit).await;
        }
    });
    serve::serve(opts, commands_rx, events_tx).await
}

async fn print_events(mut events: mpsc::UnboundedReceiver<Event>) {
    while let Some(e) = events.recv().await {
        if let Some(text) = describe(e) {
            println!("{text}");
        }
    }
}

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
        Event::PairingOpened => {
            "pairing is open for two minutes: request a code in the browser".into()
        }
        Event::UnpairedAll => "every paired browser was removed".into(),
        Event::Error(e) => e,
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

/// Reads commands typed on stdin. A thread, since tokio's stdin would block
/// shutdown. When stdin closes (run without a console) the host keeps
/// serving: `commands` lives on in the ctrl-c task.
fn console(commands: mpsc::Sender<Command>) {
    std::thread::Builder::new()
        .name("ws-console".into())
        .spawn(move || {
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                let cmd = match parse_line(&line) {
                    Line::Command(cmd) => cmd,
                    Line::Empty => continue,
                    Line::Help => {
                        println!("{CONSOLE_HELP}");
                        continue;
                    }
                };
                if commands.blocking_send(cmd).is_err() {
                    break;
                }
            }
        })
        .expect("spawn console thread");
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
