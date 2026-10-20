//! Installing the service, and the service itself: a supervisor that keeps
//! one session host running on the PC's own screen.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::{
    FOLDERID_ProgramFiles, FOLDERID_Windows, KF_FLAG_DEFAULT, SHGetKnownFolderPath,
};
use windows::core::GUID;
use windows_service::service::{
    Service, ServiceAccess, ServiceAction, ServiceActionType, ServiceControl, ServiceControlAccept,
    ServiceErrorControl, ServiceExitCode, ServiceFailureActions, ServiceFailureResetPeriod,
    ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_service::{define_windows_service, service_dispatcher};

use super::{control, launch};

pub const SERVICE_NAME: &str = "web-stream";
const DISPLAY_NAME: &str = "web-stream host";
const DESCRIPTION: &str = "Streams this PC to a paired browser, including the sign-in \
     screen, UAC prompts and programs running as administrator.";
const EXE_NAME: &str = "ws-host.exe";
const ACCESS: ServiceAccess = ServiceAccess::QUERY_STATUS
    .union(ServiceAccess::START)
    .union(ServiceAccess::STOP)
    .union(ServiceAccess::CHANGE_CONFIG)
    .union(ServiceAccess::DELETE);

/// How often the supervisor checks the console session and the host.
const POLL: Duration = Duration::from_millis(500);
/// How long the host gets to quit (putting back a changed resolution)
/// before it is killed.
const QUIT_TIMEOUT: Duration = Duration::from_secs(10);

fn known_folder(id: &GUID) -> anyhow::Result<PathBuf> {
    // SAFETY: the returned string is freed below.
    let p = unsafe { SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None) }
        .context("SHGetKnownFolderPath")?;
    // SAFETY: a valid NUL-terminated string from the shell.
    let path = unsafe { p.to_string() }.map(PathBuf::from);
    // SAFETY: allocated by the shell with CoTaskMemAlloc.
    unsafe { CoTaskMemFree(Some(p.0.cast())) };
    Ok(path?)
}

/// Where the service's copy of ws-host.exe lives: under Program Files, so
/// only administrators can replace what runs as SYSTEM. (Not from an
/// environment variable, which the user could point elsewhere.)
fn install_dir() -> anyhow::Result<PathBuf> {
    Ok(known_folder(&FOLDERID_ProgramFiles)?.join("web-stream"))
}

/// The service's data: certificate, paired browsers, logs, the control
/// pipe's name. SYSTEM's own profile, which only SYSTEM and administrators
/// can read; the same folder whoever asks (the service, or an
/// administrator's `install` or `console`).
pub fn data_dir() -> anyhow::Result<PathBuf> {
    Ok(known_folder(&FOLDERID_Windows)?
        .join(r"System32\config\systemprofile\AppData\Local\web-stream"))
}

fn manager(access: ServiceManagerAccess) -> anyhow::Result<ServiceManager> {
    ServiceManager::local_computer(None::<&str>, access).map_err(admin_hint)
}

fn admin_hint(e: windows_service::Error) -> anyhow::Error {
    if let windows_service::Error::Winapi(io) = &e {
        if io.raw_os_error() == Some(5) {
            return anyhow::anyhow!(
                "access denied: run this from an administrator prompt (Run as administrator)"
            );
        }
    }
    e.into()
}

fn open_installed(manager: &ServiceManager) -> anyhow::Result<Option<Service>> {
    match manager.open_service(SERVICE_NAME, ACCESS) {
        Ok(s) => Ok(Some(s)),
        // ERROR_SERVICE_DOES_NOT_EXIST
        Err(windows_service::Error::Winapi(e)) if e.raw_os_error() == Some(1060) => Ok(None),
        Err(e) => Err(admin_hint(e)),
    }
}

pub fn install(args: &[String]) -> anyhow::Result<()> {
    let opts = super::check_install_args(args)?;
    let args = &copy_certificate(args, &opts)?;
    let manager = manager(ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE)?;
    let existing = open_installed(&manager)?;
    if let Some(s) = &existing {
        stop_and_wait(s)?;
    }
    let exe = install_exe()?;
    let info = ServiceInfo {
        name: SERVICE_NAME.into(),
        display_name: DISPLAY_NAME.into(),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe.clone(),
        launch_arguments: ["service", "run"]
            .into_iter()
            .map(OsString::from)
            .chain(args.iter().map(OsString::from))
            .collect(),
        dependencies: Vec::new(),
        // LocalSystem.
        account_name: None,
        account_password: None,
    };
    let service = match existing {
        Some(s) => {
            s.change_config(&info).context("update the service")?;
            s
        }
        None => manager
            .create_service(&info, ACCESS)
            .context("create the service")?,
    };
    service.set_description(DESCRIPTION)?;
    // The supervisor restarts the host itself; this covers the supervisor.
    service.update_failure_actions(ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(24 * 3600)),
        reboot_msg: None,
        command: None,
        actions: Some(vec![
            ServiceAction {
                action_type: ServiceActionType::Restart,
                delay: Duration::from_secs(5),
            };
            3
        ]),
    })?;
    service.start::<&str>(&[]).context("start the service")?;
    println!(
        "installed and started the {SERVICE_NAME} service ({})",
        exe.display()
    );
    println!(
        "data (certificate, paired browsers, logs) in {}",
        data_dir()?.display()
    );
    println!();
    println!("It uses the same ports as the tray app: quit the tray app and turn off its");
    println!("\"Start with Windows\". The service has its own certificate and paired browsers,");
    println!("so pair again: run `ws-host service pair` from an administrator prompt.");
    Ok(())
}

/// The host reads `--cert` and `--key` as SYSTEM: copies them into the
/// service's data folder, which only administrators can change, so a user
/// who can write where they came from can't swap them later.
fn copy_certificate(
    args: &[String],
    opts: &ws_host::options::ServeOptions,
) -> anyhow::Result<Vec<String>> {
    let Some((cert, key)) = &opts.cert else {
        return Ok(args.to_vec());
    };
    let dir = data_dir()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let mut args = args.to_vec();
    for (flag, src, name) in [
        ("--cert", cert, "service-cert.pem"),
        ("--key", key, "service-key.pem"),
    ] {
        let dest = dir.join(name);
        // Installing again with the copy itself: nothing to copy.
        if !same_file(src, &dest) {
            std::fs::copy(src, &dest)
                .with_context(|| format!("copy {} to {}", src.display(), dest.display()))?;
        }
        args = super::replace_value(&args, flag, &dest.display().to_string());
    }
    Ok(args)
}

pub fn update() -> anyhow::Result<()> {
    let manager = manager(ServiceManagerAccess::CONNECT)?;
    let Some(service) = open_installed(&manager)? else {
        bail!("the service is not installed: run `ws-host service install`");
    };
    stop_and_wait(&service)?;
    let exe = install_exe()?;
    service.start::<&str>(&[]).context("start the service")?;
    println!("updated {} and restarted the service", exe.display());
    Ok(())
}

pub fn uninstall() -> anyhow::Result<()> {
    let manager = manager(ServiceManagerAccess::CONNECT)?;
    let Some(service) = open_installed(&manager)? else {
        println!("the service is not installed");
        return Ok(());
    };
    stop_and_wait(&service)?;
    service.delete().context("delete the service")?;
    drop(service);
    let dir = install_dir()?;
    let _ = std::fs::remove_file(dir.join(EXE_NAME));
    let _ = std::fs::remove_dir(&dir);
    println!(
        "removed the service; its data stays in {}",
        data_dir()?.display()
    );
    Ok(())
}

fn stop_and_wait(service: &Service) -> anyhow::Result<()> {
    let status = service.query_status()?;
    if status.current_state == ServiceState::Stopped {
        return Ok(());
    }
    if status.current_state != ServiceState::StopPending {
        service.stop().context("stop the service")?;
    }
    let deadline = Instant::now() + QUIT_TIMEOUT + Duration::from_secs(20);
    while Instant::now() < deadline {
        if service.query_status()?.current_state == ServiceState::Stopped {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    bail!("the service did not stop")
}

/// Copies this exe to the install folder (unless it is that copy).
fn install_exe() -> anyhow::Result<PathBuf> {
    let src = std::env::current_exe()?;
    let dir = install_dir()?;
    let dest = dir.join(EXE_NAME);
    if same_file(&src, &dest) {
        return Ok(dest);
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let tmp = dir.join("ws-host.exe.new");
    std::fs::copy(&src, &tmp).with_context(|| format!("copy to {}", tmp.display()))?;
    // The old host may take a moment to let go of the file after stopping.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match std::fs::rename(&tmp, &dest) {
            Ok(()) => return Ok(dest),
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(250)),
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(e).with_context(|| format!("replace {}", dest.display()));
            }
        }
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// The serve options from the service's command line (`service run ...`),
/// for the service thread the dispatcher starts.
static SERVE_ARGS: OnceLock<Vec<String>> = OnceLock::new();

define_windows_service!(ffi_service_main, service_main);

/// Runs as the service; started by the service control manager.
pub fn run(args: &[String]) -> anyhow::Result<()> {
    super::imp::harden_dll_search();
    let _ = SERVE_ARGS.set(args.to_vec());
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
        .context("not started by the service control manager (use `ws-host service install`)")?;
    Ok(())
}

fn service_main(_: Vec<OsString>) {
    if let Err(e) = run_service() {
        tracing::error!("service stopped: {e:#}");
    }
}

enum Wake {
    Stop,
    SessionChange,
}

fn run_service() -> anyhow::Result<()> {
    let (tx, rx) = mpsc::channel();
    let handler = move |control| match control {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            let _ = tx.send(Wake::Stop);
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::SessionChange(_) => {
            let _ = tx.send(Wake::SessionChange);
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    };
    let status = service_control_handler::register(SERVICE_NAME, handler)?;
    let set = |state, accept, wait, code| {
        let _ = status.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: accept,
            exit_code: code,
            checkpoint: 0,
            wait_hint: wait,
            process_id: None,
        });
    };
    let ok = ServiceExitCode::Win32(0);
    set(
        ServiceState::Running,
        ServiceControlAccept::STOP
            | ServiceControlAccept::SHUTDOWN
            | ServiceControlAccept::SESSION_CHANGE,
        Duration::ZERO,
        ok,
    );
    let result = Supervisor::new().map(|mut s| {
        s.run(&rx);
        set(
            ServiceState::StopPending,
            ServiceControlAccept::empty(),
            QUIT_TIMEOUT + Duration::from_secs(5),
            ok,
        );
        s.stop_host();
    });
    // A failure exit code lets the failure actions restart the service.
    let code = match result {
        Ok(()) => ok,
        Err(_) => ServiceExitCode::ServiceSpecific(1),
    };
    set(
        ServiceState::Stopped,
        ServiceControlAccept::empty(),
        Duration::ZERO,
        code,
    );
    result
}

struct Host {
    child: launch::Child,
    pipe: String,
    started: Instant,
}

/// Keeps one session host running in the console session.
struct Supervisor {
    exe: PathBuf,
    args: Vec<String>,
    data_dir: PathBuf,
    /// Set by the host to ask for Ctrl+Alt+Del; 0 if it couldn't be made.
    sas_event: usize,
    host: Option<Host>,
    backoff: Duration,
    next_start: Instant,
}

impl Supervisor {
    fn new() -> anyhow::Result<Self> {
        let data_dir = data_dir()?;
        std::fs::create_dir_all(&data_dir)
            .with_context(|| format!("create {}", data_dir.display()))?;
        tracing::info!("service started");
        let sas_event = match super::sas::new_event() {
            Ok(event) => {
                super::sas::spawn_sender(event);
                event
            }
            Err(e) => {
                tracing::error!("Ctrl+Alt+Del won't work: {e:#}");
                0
            }
        };
        Ok(Self {
            exe: std::env::current_exe()?,
            args: SERVE_ARGS.get().cloned().unwrap_or_default(),
            data_dir,
            sas_event,
            host: None,
            backoff: Duration::ZERO,
            next_start: Instant::now(),
        })
    }

    /// Returns when the service is asked to stop.
    fn run(&mut self, wake: &mpsc::Receiver<Wake>) {
        loop {
            self.reap();
            let console = launch::console_session();
            if let (Some(host), Some(session)) = (&self.host, console) {
                if host.child.session != session {
                    tracing::info!(
                        from = host.child.session,
                        to = session,
                        "the screen moved to another session, moving the host"
                    );
                    self.stop_host();
                }
            }
            if self.host.is_none() && Instant::now() >= self.next_start {
                if let Some(session) = console {
                    self.start_host(session);
                }
            }
            match wake.recv_timeout(POLL) {
                Ok(Wake::Stop) | Err(RecvTimeoutError::Disconnected) => return,
                Ok(Wake::SessionChange) | Err(RecvTimeoutError::Timeout) => {}
            }
        }
    }

    fn start_host(&mut self, session: u32) {
        let started = super::random_pipe_name().and_then(|pipe| {
            let args = super::session_host_args(&pipe, self.sas_event, &self.args, &self.data_dir);
            let inherit: &[usize] = match self.sas_event {
                0 => &[],
                event => &[event],
            };
            let log = self.data_dir.join("host.log");
            let child = launch::spawn(session, &self.exe, &args, &log, inherit)?;
            // For `console`, which checks the pipe is served by this process.
            let file = self.pipe_file();
            std::fs::write(&file, super::pipe_file_contents(&pipe, child.pid))
                .with_context(|| format!("write {}", file.display()))?;
            Ok((child, pipe))
        });
        match started {
            Ok((child, pipe)) => {
                tracing::info!(session, pid = child.pid, "started the host");
                self.host = Some(Host {
                    child,
                    pipe,
                    started: Instant::now(),
                });
            }
            Err(e) => {
                tracing::error!(session, "could not start the host: {e:#}");
                self.retry_later(Duration::ZERO);
            }
        }
    }

    fn pipe_file(&self) -> PathBuf {
        self.data_dir.join(super::PIPE_FILE)
    }

    /// No console may look for a host that is gone.
    fn forget_pipe(&self) {
        let _ = std::fs::remove_file(self.pipe_file());
    }

    /// Notices a host that exited and schedules the next start: right away
    /// after a long run, backing off while it keeps failing (say, the tray
    /// app holds the port).
    fn reap(&mut self) {
        let Some(host) = &self.host else {
            return;
        };
        let Some(code) = host.child.exit_code() else {
            return;
        };
        let ran = host.started.elapsed();
        tracing::warn!(
            code,
            ran_secs = ran.as_secs(),
            "the host exited (its log is host.log)"
        );
        self.host = None;
        self.forget_pipe();
        self.retry_later(ran);
    }

    fn retry_later(&mut self, ran: Duration) {
        self.backoff = super::next_backoff(self.backoff, ran);
        self.next_start = Instant::now() + self.backoff;
    }

    /// Asks the host to quit, so it puts back a changed resolution, and
    /// kills it if it doesn't.
    /// Starts the next host right away with a fresh backoff: a stop is not
    /// a failure.
    fn stop_host(&mut self) {
        let Some(host) = self.host.take() else {
            return;
        };
        self.forget_pipe();
        if let Err(e) = control::request_quit(&host.pipe, host.child.pid) {
            tracing::warn!("could not ask the host to quit: {e:#}");
        }
        if !host.child.wait(QUIT_TIMEOUT) {
            tracing::warn!("the host did not quit, killing it");
            host.child.kill();
        }
        self.backoff = Duration::ZERO;
        self.next_start = Instant::now();
    }
}

/// The supervisor's own log, `service.log` in the data folder.
pub fn service_log() -> Option<std::fs::File> {
    let dir = data_dir().ok()?;
    std::fs::create_dir_all(&dir).ok()?;
    launch::open_log(&dir.join("service.log")).ok()
}
