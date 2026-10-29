//! The session host's console, on a named pipe only SYSTEM and elevated
//! administrators can open: whoever can open pairing can let a browser
//! drive the sign-in screen and UAC prompts, so a program running as a
//! normal user must not be able to.

use std::ffi::c_void;
use std::io::Write;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::options::ServeOptions;
use crate::serve::{self, Event, Notice};
use anyhow::{Context, bail};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};
use tokio::sync::{broadcast, mpsc};
use windows::Win32::Foundation::{HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::Storage::FileSystem::{SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT};
use windows::Win32::System::Pipes::GetNamedPipeServerProcessId;
use windows::core::PCWSTR;

use super::launch::wide;
use crate::console::{CONSOLE_HELP, EVENTS_JSON, Line, command_lines, describe, parse_line};

/// Protected DACL: full access for SYSTEM and Administrators, nobody else.
/// An administrator's unelevated token has Administrators as deny-only, so
/// it takes an elevated prompt.
const PIPE_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)";
const ERROR_ACCESS_DENIED: i32 = 5;
const ERROR_FILE_NOT_FOUND: i32 = 2;
const ERROR_PIPE_BUSY: i32 = 231;

/// What a console that connects late is told first: the host's start,
/// and the browser connected now.
#[derive(Default)]
struct Snapshot {
    started: Option<Event>,
    connected: Option<String>,
}

impl Snapshot {
    fn update(&mut self, e: &Event) {
        match e {
            Event::Started { .. } => self.started = Some(e.clone()),
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

    fn events(&self) -> Vec<Event> {
        let mut out: Vec<Event> = self.started.iter().cloned().collect();
        if let Some(device_name) = &self.connected {
            out.push(Event::Notice(Notice::SessionStarted {
                device_name: device_name.clone(),
            }));
        }
        out
    }
}

/// Runs the host with its console on the pipe, until it is told to quit.
/// `pipe` is the name the service picked (see `PIPE_PREFIX`). With a `link`
/// (an inherited pipe handle; 0 for none) this is the media side, and the
/// network host at the other end serves the browser.
pub async fn serve(pipe: String, link: usize, opts: ServeOptions) -> anyhow::Result<()> {
    let (commands, commands_rx) = mpsc::channel(8);
    let (events_tx, mut events) = mpsc::unbounded_channel::<Event>();
    let (sent, _) = broadcast::channel::<Event>(256);
    let snapshot = Arc::new(Mutex::new(Snapshot::default()));

    // The first instance fails if anyone else holds the name.
    let first = create(&pipe, true).with_context(|| format!("control pipe {pipe} is taken"))?;
    tokio::spawn(accept(
        pipe,
        first,
        commands,
        sent.clone(),
        snapshot.clone(),
    ));
    tokio::spawn(async move {
        while let Some(e) = events.recv().await {
            if let Some(text) = describe(e.clone()) {
                tracing::info!("{text}");
            }
            // Under the lock, so a console that connects now gets either
            // the snapshot with this event, or this event.
            let mut snapshot = snapshot.lock().unwrap();
            snapshot.update(&e);
            let _ = sent.send(e);
        }
    });
    if link == 0 {
        tracing::warn!(
            "running the whole host as SYSTEM: the network host could not be started \
             (service.log says why)"
        );
        return serve::serve(opts, commands_rx, events_tx).await;
    }
    // SAFETY: the service handed this process the pipe's server end,
    // overlapped and already connected, and nothing else here owns it.
    let link = unsafe { NamedPipeServer::from_raw_handle(link as *mut c_void) }
        .context("the link to the network host")?;
    crate::split::serve_media(opts, link, commands_rx, events_tx).await
}

fn create(name: &str, first: bool) -> std::io::Result<NamedPipeServer> {
    let sddl = wide(PIPE_SDDL);
    let mut sd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: `sddl` is NUL-terminated; `sd` is freed below.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &mut sd,
            None,
        )
    }
    .map_err(std::io::Error::other)?;
    let mut sa = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0,
        bInheritHandle: false.into(),
    };
    // SAFETY: `sa` points at a valid SECURITY_ATTRIBUTES for the call, which
    // copies the descriptor.
    let server = unsafe {
        ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(name, (&raw mut sa).cast::<c_void>())
    };
    // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
    unsafe { LocalFree(Some(HLOCAL(sd.0))) };
    server
}

async fn accept(
    name: String,
    mut server: NamedPipeServer,
    commands: mpsc::Sender<serve::Command>,
    sent: broadcast::Sender<Event>,
    snapshot: Arc<Mutex<Snapshot>>,
) {
    loop {
        let connected = server.connect().await;
        // A free instance for the next console before serving this one.
        let next = match create(&name, false) {
            Ok(s) => s,
            Err(e) => return tracing::error!("control pipe: {e}"),
        };
        let client = std::mem::replace(&mut server, next);
        if let Err(e) = connected {
            // Say, a client that left at once: this instance is done for.
            tracing::debug!("control pipe: {e}");
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            continue;
        }
        let (greeting, events) = {
            let snapshot = snapshot.lock().unwrap();
            (snapshot.events(), sent.subscribe())
        };
        tokio::spawn(client_session(client, commands.clone(), events, greeting));
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

async fn client_session(
    client: NamedPipeServer,
    commands: mpsc::Sender<serve::Command>,
    mut events: broadcast::Receiver<Event>,
    greeting: Vec<Event>,
) {
    let (read, mut write) = tokio::io::split(client);
    let mut read = BufReader::new(read).lines();
    let mut format = Format::Text;
    // The host's start, in text, for a console typed into by hand. One
    // that asks for JSON gets it again as JSON.
    if let Some(text) = greeting.first().and_then(|e| format_event(e, format)) {
        if write.write_all(text.as_bytes()).await.is_err() {
            return;
        }
    }
    loop {
        tokio::select! {
            line = read.next_line() => {
                let Ok(Some(line)) = line else { return };
                if line.trim() == EVENTS_JSON {
                    format = Format::Json;
                    for e in &greeting {
                        let Some(text) = format_event(e, format) else { continue };
                        if write.write_all(text.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                    continue;
                }
                match parse_line(&line) {
                    Line::Command(cmd) => {
                        if commands.send(cmd).await.is_err() {
                            return;
                        }
                    }
                    Line::Empty => {}
                    Line::Help => {
                        if format == Format::Json {
                            continue;
                        }
                        let help = format!("{CONSOLE_HELP}\n(quit restarts the host)\n");
                        if write.write_all(help.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                }
            }
            e = events.recv() => match e {
                Ok(e) => {
                    let Some(text) = format_event(&e, format) else { continue };
                    if write.write_all(text.as_bytes()).await.is_err() {
                        return;
                    }
                }
                // A console that fell this far behind has missed events it
                // can't do without: it connects again.
                Err(broadcast::error::RecvError::Lagged(_)) if format == Format::Json => return,
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => return,
            },
        }
    }
}

fn open_error(e: std::io::Error) -> anyhow::Error {
    match e.raw_os_error() {
        Some(ERROR_ACCESS_DENIED) => {
            anyhow::anyhow!("access denied: controlling the service takes administrator rights")
        }
        Some(ERROR_FILE_NOT_FOUND) => anyhow::anyhow!(
            "the service's host is not running. Is the service installed and started \
             (`sc query web-stream`)? Its log is host.log in the service's data folder."
        ),
        _ => anyhow::Error::new(e).context("open the host's control pipe"),
    }
}

/// The pipe of the host the service started last.
fn current_pipe() -> anyhow::Result<(String, u32)> {
    let file = super::scm::data_dir()?.join(super::PIPE_FILE);
    let text = match std::fs::read_to_string(&file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return Err(open_error(std::io::Error::from_raw_os_error(
                ERROR_ACCESS_DENIED,
            )));
        }
        Err(_) => {
            return Err(open_error(std::io::Error::from_raw_os_error(
                ERROR_FILE_NOT_FOUND,
            )));
        }
    };
    super::parse_pipe_file(&text)
        .with_context(|| format!("{} does not name a control pipe", file.display()))
}

/// Fails unless process `pid` serves the pipe `handle` is connected to: a
/// pipe name can be seen by anyone, and taken by anyone once its host is
/// gone.
fn check_server(handle: HANDLE, name: &str, pid: u32) -> anyhow::Result<()> {
    let mut server = 0u32;
    // SAFETY: a valid pipe handle.
    unsafe { GetNamedPipeServerProcessId(handle, &mut server) }
        .context("GetNamedPipeServerProcessId")?;
    if server != pid {
        anyhow::bail!("{name} is served by process {server}, not the service's host ({pid})");
    }
    Ok(())
}

/// Attaches to the service's host, checking it is the process the service
/// started, and passes on its events until the pipe breaks (the host
/// stopped) or `commands` closes. The tray app, run as an elevated
/// administrator, is the one user: it shows the service's host as it
/// would its own. [`serve::Command::Quit`] only detaches; it never stops
/// the service's host.
pub async fn attach(
    mut commands: mpsc::Receiver<serve::Command>,
    events: mpsc::UnboundedSender<Event>,
) -> anyhow::Result<()> {
    let client = open_client().await?;
    let (read, mut write) = tokio::io::split(client);
    let mut read = BufReader::new(read).lines();
    write
        .write_all(format!("{EVENTS_JSON}\n").as_bytes())
        .await
        .context("write to the service's host")?;
    loop {
        tokio::select! {
            line = read.next_line() => {
                let Some(line) = line.context("read from the service's host")? else {
                    bail!("the service's host stopped; the service starts it again");
                };
                if line.trim().is_empty() {
                    continue;
                }
                // A build from before the tray app could attach greets
                // in text.
                let e = serde_json::from_str::<Event>(&line).map_err(|_| {
                    anyhow::anyhow!(
                        "the service runs an older build: click \"Update the service\" \
                         in the settings"
                    )
                })?;
                if events.send(e).is_err() {
                    return Ok(());
                }
            }
            cmd = commands.recv() => {
                let Some(cmd) = cmd else { return Ok(()) };
                if cmd == serve::Command::Quit {
                    return Ok(());
                }
                for line in command_lines(&cmd) {
                    write
                        .write_all(format!("{line}\n").as_bytes())
                        .await
                        .context("write to the service's host")?;
                }
            }
        }
    }
}

async fn open_client() -> anyhow::Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    let (pipe, pid) = current_pipe()?;
    let client = loop {
        match ClientOptions::new().open(&pipe) {
            Ok(c) => break c,
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await
            }
            Err(e) => return Err(open_error(e)),
        }
    };
    check_server(HANDLE(client.as_raw_handle()), &pipe, pid)?;
    Ok(client)
}

/// Tells the host (process `pid`) to quit, retrying for a moment while
/// its pipe isn't up yet or is busy. Called by the service, which runs as
/// SYSTEM: the pipe is opened for identification only, so whoever answers
/// can't act as the service, and nothing is sent unless the host itself
/// answers.
pub fn request_quit(name: &str, pid: u32) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut pipe = loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .security_qos_flags(SECURITY_SQOS_PRESENT.0 | SECURITY_IDENTIFICATION.0)
            .open(name)
        {
            Ok(p) => break p,
            Err(e)
                if matches!(
                    e.raw_os_error(),
                    Some(ERROR_FILE_NOT_FOUND | ERROR_PIPE_BUSY)
                ) && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(e.into()),
        }
    };
    check_server(HANDLE(pipe.as_raw_handle()), name, pid)?;
    pipe.write_all(b"quit\n")?;
    Ok(())
}
