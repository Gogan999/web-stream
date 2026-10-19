//! The session host's console, on a named pipe only SYSTEM and elevated
//! administrators can open: whoever can open pairing can let a browser
//! drive the sign-in screen and UAC prompts, so a program running as a
//! normal user must not be able to.

use std::ffi::c_void;
use std::io::{BufRead, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
use ws_host::options::ServeOptions;
use ws_host::serve::{self, Event};

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
/// `pipe` is the name the service picked (see `PIPE_PREFIX`).
pub async fn serve(pipe: String, opts: ServeOptions) -> anyhow::Result<()> {
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
    serve::serve(opts, commands_rx, events_tx).await
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
        Some(ERROR_ACCESS_DENIED) => anyhow::anyhow!(
            "access denied: controlling the service takes an administrator prompt \
             (Run as administrator)"
        ),
        Some(ERROR_FILE_NOT_FOUND) => anyhow::anyhow!(
            "the service's host is not running. Is the service installed and started \
             (`sc query web-stream`)? Its log is host.log in the service's data folder."
        ),
        _ => anyhow::Error::new(e).context("open the host's control pipe"),
    }
}

/// The pipe of the host the service started last.
fn current_pipe() -> anyhow::Result<String> {
    let file = super::scm::data_dir()?.join(super::PIPE_FILE);
    let pipe = match std::fs::read_to_string(&file) {
        Ok(p) => p.trim().to_string(),
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
    if !super::valid_pipe_name(&pipe) {
        anyhow::bail!("{} does not hold a control pipe name", file.display());
    }
    Ok(pipe)
}

/// `ws-host service console` and `pair`: types commands into the service's
/// host and prints what it says, until Ctrl+C or the host stops.
pub fn console(first: Option<&str>) -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let pipe = current_pipe()?;
        let client = loop {
            match ClientOptions::new().open(&pipe) {
                Ok(c) => break c,
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await
                }
                Err(e) => return Err(open_error(e)),
            }
        };
        let (read, mut write) = tokio::io::split(client);
        if let Some(cmd) = first {
            write.write_all(format!("{cmd}\n").as_bytes()).await?;
        }
        println!("connected to the web-stream service; Ctrl+C leaves (the host keeps running)");
        let (typed_tx, mut typed) = mpsc::channel::<String>(8);
        std::thread::Builder::new()
            .name("ws-console".into())
            .spawn(move || {
                for line in std::io::stdin().lock().lines() {
                    let Ok(line) = line else { break };
                    if typed_tx.blocking_send(line).is_err() {
                        break;
                    }
                }
            })?;
        let mut read = BufReader::new(read).lines();
        loop {
            tokio::select! {
                line = read.next_line() => match line? {
                    Some(line) => println!("{line}"),
                    None => {
                        println!("the host stopped; the service starts it again");
                        return Ok(());
                    }
                },
                Some(line) = typed.recv() => {
                    write.write_all(format!("{line}\n").as_bytes()).await?;
                }
            }
        }
    })?;
    // The stdin thread may still be blocked reading.
    std::process::exit(0)
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
    let mut server = 0u32;
    // SAFETY: a valid pipe handle.
    unsafe { GetNamedPipeServerProcessId(HANDLE(pipe.as_raw_handle()), &mut server) }
        .context("GetNamedPipeServerProcessId")?;
    if server != pid {
        anyhow::bail!("{name} is served by process {server}, not the host ({pid})");
    }
    pipe.write_all(b"quit\n")?;
    Ok(())
}
