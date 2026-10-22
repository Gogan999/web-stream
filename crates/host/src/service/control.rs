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
use crate::serve::{self, Event};
use anyhow::Context;
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
use crate::console::{CONSOLE_HELP, Line, describe, parse_line};

/// Protected DACL: full access for SYSTEM and Administrators, nobody else.
/// An administrator's unelevated token has Administrators as deny-only, so
/// it takes an elevated prompt.
const PIPE_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)";
const ERROR_ACCESS_DENIED: i32 = 5;
const ERROR_FILE_NOT_FOUND: i32 = 2;
const ERROR_PIPE_BUSY: i32 = 231;

/// Runs the host with its console on the pipe, until it is told to quit.
/// `pipe` is the name the service picked (see `PIPE_PREFIX`). With a `link`
/// (an inherited pipe handle; 0 for none) this is the media side, and the
/// network host at the other end serves the browser.
pub async fn serve(pipe: String, link: usize, opts: ServeOptions) -> anyhow::Result<()> {
    let (commands, commands_rx) = mpsc::channel(8);
    let (events_tx, mut events) = mpsc::unbounded_channel();
    let (lines, _) = broadcast::channel::<String>(256);
    // What the host printed on starting, for each console that connects.
    let started = Arc::new(Mutex::new(None::<String>));

    // The first instance fails if anyone else holds the name.
    let first = create(&pipe, true).with_context(|| format!("control pipe {pipe} is taken"))?;
    tokio::spawn(accept(
        pipe,
        first,
        commands,
        lines.clone(),
        started.clone(),
    ));
    tokio::spawn(async move {
        while let Some(e) = events.recv().await {
            let is_start = matches!(e, Event::Started { .. });
            let Some(text) = describe(e) else { continue };
            tracing::info!("{text}");
            if is_start {
                *started.lock().unwrap() = Some(text.clone());
            }
            let _ = lines.send(text);
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
    lines: broadcast::Sender<String>,
    started: Arc<Mutex<Option<String>>>,
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
        let greeting = started.lock().unwrap().clone();
        tokio::spawn(client_session(
            client,
            commands.clone(),
            lines.subscribe(),
            greeting,
        ));
    }
}

async fn client_session(
    client: NamedPipeServer,
    commands: mpsc::Sender<serve::Command>,
    mut lines: broadcast::Receiver<String>,
    greeting: Option<String>,
) {
    let (read, mut write) = tokio::io::split(client);
    let mut read = BufReader::new(read).lines();
    if let Some(text) = greeting {
        if write
            .write_all(format!("{text}\n").as_bytes())
            .await
            .is_err()
        {
            return;
        }
    }
    loop {
        tokio::select! {
            line = read.next_line() => {
                let Ok(Some(line)) = line else { return };
                match parse_line(&line) {
                    Line::Command(cmd) => {
                        if commands.send(cmd).await.is_err() {
                            return;
                        }
                    }
                    Line::Empty => {}
                    Line::Help => {
                        let help = format!("{CONSOLE_HELP}\n(quit restarts the host)\n");
                        if write.write_all(help.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                }
            }
            text = lines.recv() => match text {
                Ok(text) => {
                    if write.write_all(format!("{text}\n").as_bytes()).await.is_err() {
                        return;
                    }
                }
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

/// A connection to the service's host on its control pipe: what the host
/// says, and the commands typed into it. The tray app's service console
/// window, run as an elevated administrator, is the one user.
pub struct ServiceConsole {
    /// Lines from the host; disconnected once the host stops or the pipe
    /// breaks.
    pub lines: std::sync::mpsc::Receiver<String>,
    commands: mpsc::UnboundedSender<String>,
}

impl ServiceConsole {
    /// Types `line` (`pair`, `drop`, `unpair-all`, `send <path>`) into the
    /// host; false once the connection is gone.
    pub fn send(&self, line: &str) -> bool {
        self.commands.send(line.to_owned()).is_ok()
    }
}

/// Connects to the service's host, checking it is the process the service
/// started. `notify` is called (on another thread) after each line.
pub fn connect(notify: impl Fn() + Send + 'static) -> anyhow::Result<ServiceConsole> {
    let (lines_tx, lines) = std::sync::mpsc::channel();
    let (commands, mut commands_rx) = mpsc::unbounded_channel::<String>();
    let (ready_tx, ready) = std::sync::mpsc::sync_channel::<anyhow::Result<()>>(1);
    std::thread::Builder::new()
        .name("ws-service-console".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = ready_tx.send(Err(e.into()));
                    return;
                }
            };
            rt.block_on(async move {
                let client = match open_client().await {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                let _ = ready_tx.send(Ok(()));
                let (read, mut write) = tokio::io::split(client);
                let mut read = BufReader::new(read).lines();
                loop {
                    tokio::select! {
                        line = read.next_line() => {
                            let Ok(Some(line)) = line else {
                                let _ = lines_tx.send(
                                    "the service's host stopped; the service starts it again".into(),
                                );
                                notify();
                                return;
                            };
                            if lines_tx.send(line).is_err() {
                                return;
                            }
                            notify();
                        }
                        cmd = commands_rx.recv() => {
                            let Some(cmd) = cmd else { return };
                            if write.write_all(format!("{cmd}\n").as_bytes()).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            });
        })?;
    ready
        .recv()
        .unwrap_or_else(|_| Err(anyhow::anyhow!("the control pipe thread stopped")))?;
    Ok(ServiceConsole { lines, commands })
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
