//! A headless host for tests and development: the host with commands typed
//! on stdin (`pair`, `drop`, `unpair-all`, `send <path>`, `quit`) and its
//! events printed on stdout. CI's browser tests drive it on Linux. People
//! run the tray app (`ws-host-gui`) instead.
//!
//! ```text
//! cargo run -p ws-host --example test-host -- [serve options, see --help]
//! ```

use std::io::{BufRead, IsTerminal};

use tokio::sync::mpsc;
use ws_host::console::{CONSOLE_HELP, Line, describe, parse_line};
use ws_host::options::{self, ServeOptions};
use ws_host::serve::{self, Command, Event};

fn main() -> anyhow::Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,str0m=warn".into()),
        )
        .with_ansi(std::io::stdout().is_terminal())
        .init();
    // Accepted for old scripts: `test-host serve [options]`.
    if args.first().is_some_and(|a| a == "serve") {
        args.remove(0);
    }
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{}", options::USAGE);
        return Ok(());
    }
    let opts = ServeOptions::parse(&args)?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(opts))
}

async fn run(opts: ServeOptions) -> anyhow::Result<()> {
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

/// Reads commands typed on stdin. A thread, since tokio's stdin would block
/// shutdown. When stdin closes the host keeps serving: `commands` lives on
/// in the ctrl-c task.
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
