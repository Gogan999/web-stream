//! A headless host for tests and development: the host with commands typed
//! on stdin (`pair`, `drop`, `unpair-all`, `send <path>`, `quit`) and its
//! events printed on stdout. CI's browser tests drive it on Linux. People
//! run the tray app (`ws-host-gui`) instead.
//!
//! ```text
//! cargo run -p ws-host --example test-host -- [--split] [serve options, see --help]
//! ```
//!
//! `--split` (Unix) runs it as the Windows service does: the network side
//! (signaling, the transport) in a child process, linked to the media side
//! in this one (see `ws_host::split`).

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
    let network_half = args.first().is_some_and(|a| a == NETWORK_HALF);
    if network_half {
        args.remove(0);
    }
    let split = args.iter().any(|a| a == "--split");
    args.retain(|a| a != "--split");
    let opts = ServeOptions::parse(&args)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    if network_half {
        return runtime.block_on(split::network_half(opts));
    }
    runtime.block_on(run(opts, split.then_some(args)))
}

/// The child's first argument under `--split`.
const NETWORK_HALF: &str = "network-half";

/// Runs the host, or with `split` (the serve arguments) its media side,
/// with the network side in a child process.
async fn run(opts: ServeOptions, split: Option<Vec<String>>) -> anyhow::Result<()> {
    let (commands, commands_rx) = mpsc::channel(8);
    let (events_tx, events) = mpsc::unbounded_channel();
    tokio::spawn(print_events(events));
    console(commands.clone());
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = commands.send(Command::Quit).await;
        }
    });
    match split {
        None => serve::serve(opts, commands_rx, events_tx).await,
        Some(args) => split::media_half(opts, &args, commands_rx, events_tx).await,
    }
}

#[cfg(unix)]
mod split {
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::unix::net::UnixStream;

    use tokio::sync::mpsc;
    use ws_host::options::ServeOptions;
    use ws_host::serve::{Command, Event};

    /// Starts the network side as a child, its stdin one end of a socket
    /// pair, and serves the media side on the other.
    pub async fn media_half(
        opts: ServeOptions,
        args: &[String],
        commands: mpsc::Receiver<Command>,
        events: mpsc::UnboundedSender<Event>,
    ) -> anyhow::Result<()> {
        let (ours, theirs) = UnixStream::pair()?;
        let mut child = tokio::process::Command::new(std::env::current_exe()?)
            .arg(super::NETWORK_HALF)
            .args(args)
            .stdin(OwnedFd::from(theirs))
            .kill_on_drop(true)
            .spawn()?;
        ours.set_nonblocking(true)?;
        let link = tokio::net::UnixStream::from_std(ours)?;
        let result = ws_host::split::serve_media(opts, link, commands, events).await;
        // It stops once the link closes.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await;
        result
    }

    pub async fn network_half(opts: ServeOptions) -> anyhow::Result<()> {
        // SAFETY: stdin is the socket the parent handed over, and nothing
        // else in this process uses it.
        let link = unsafe { UnixStream::from_raw_fd(0) };
        link.set_nonblocking(true)?;
        let link = tokio::net::UnixStream::from_std(link)?;
        ws_host::split::serve_network(opts, link).await
    }
}

#[cfg(not(unix))]
mod split {
    use tokio::sync::mpsc;
    use ws_host::options::ServeOptions;
    use ws_host::serve::{Command, Event};

    pub async fn media_half(
        _: ServeOptions,
        _: &[String],
        _: mpsc::Receiver<Command>,
        _: mpsc::UnboundedSender<Event>,
    ) -> anyhow::Result<()> {
        anyhow::bail!("--split needs Unix; the Windows service splits the host itself")
    }

    pub async fn network_half(_: ServeOptions) -> anyhow::Result<()> {
        anyhow::bail!("--split needs Unix")
    }
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
