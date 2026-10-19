//! `ws-host serve`: the host with a small console for the host user.

use std::io::BufRead;

use tokio::sync::mpsc;
use ws_host::options::ServeOptions;
use ws_host::serve::{self, Command, Event, FileEvent, Notice};

const CONSOLE_HELP: &str = "\
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
        match e {
            Event::Started {
                port,
                fingerprint,
                web_root,
                data_dir,
                ..
            } => {
                println!("web-stream host on https://<this PC>:{port}/");
                println!("certificate SHA-256 {fingerprint}");
                match web_root {
                    Some(dir) => println!("serving the web client from {}", dir.display()),
                    None => println!("serving the web client built into this host"),
                }
                println!("data in {}", data_dir.display());
                println!("{CONSOLE_HELP}");
            }
            Event::PairingOpened => {
                println!("pairing is open for two minutes: request a code in the browser")
            }
            Event::UnpairedAll => println!("every paired browser was removed"),
            Event::Error(e) => println!("{e}"),
            Event::Notice(n) => print_notice(n),
            Event::File(f) => print_file(f),
        }
    }
}

fn print_notice(n: Notice) {
    match n {
        Notice::PairingCode {
            code,
            device_name,
            expires_in_secs,
        } => println!(
            "\n  PAIRING CODE {code}  for {device_name:?} (expires in {expires_in_secs}s)\n"
        ),
        Notice::Paired { device_name } => println!("paired {device_name:?}"),
        Notice::PairingFailed { attempts_left } => {
            println!("wrong pairing code; {attempts_left} attempts left")
        }
        Notice::PairingLocked => {
            println!("too many wrong codes: pairing is locked until you type `pair`")
        }
        Notice::SessionStarted { device_name } => println!("{device_name:?} connected"),
        Notice::SessionEnded { device_name } => println!("{device_name:?} disconnected"),
    }
}

fn print_file(f: FileEvent) {
    match f {
        FileEvent::Shared { name, size, .. } => {
            println!("sharing {name:?} ({size} bytes) with the browser")
        }
        FileEvent::Progress { .. } => {}
        FileEvent::Sent { name, .. } => println!("sent {name:?}"),
        FileEvent::Cancelled { name, .. } => println!("the browser stopped receiving {name:?}"),
        FileEvent::Failed { name, message, .. } => println!("sending {name:?} failed: {message}"),
        FileEvent::Withdrawn { name, .. } => println!("stopped sharing {name:?}"),
        FileEvent::Cleared => {}
        FileEvent::Receiving { name, size } => {
            println!("receiving {name:?} ({size} bytes) from the browser")
        }
        FileEvent::Received { path, .. } => println!("received {}", path.display()),
        FileEvent::ReceiveFailed { name, message } => {
            println!("receiving {name:?} failed: {message}")
        }
    }
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
                let line = line.trim();
                if let Some(path) = line.strip_prefix("send ") {
                    let path = path.trim().trim_matches('"');
                    let cmd = Command::ShareFiles(vec![path.into()]);
                    if commands.blocking_send(cmd).is_err() {
                        break;
                    }
                    continue;
                }
                let cmd = match line {
                    "pair" | "p" => Command::OpenPairing,
                    "drop" | "d" => Command::DropSession,
                    "unpair-all" => Command::UnpairAll,
                    "quit" | "q" | "exit" => Command::Quit,
                    "" => continue,
                    _ => {
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
