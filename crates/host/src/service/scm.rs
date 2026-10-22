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
/// The service's copy of the tray app's exe. Named after the console
/// program it used to be, so installs from then update in place.
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

/// Where the service's copy of the host lives: under Program Files, so
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

/// The network host's data (certificate, paired browsers), which it can
/// read and write as LocalService: a folder in [`data_dir`], with its own
/// permissions.
pub fn network_dir() -> anyhow::Result<PathBuf> {
    Ok(data_dir()?.join("network"))
}

/// Makes the network host's folder and sets its permissions, and moves in
/// what the service kept in its own folder before the split.
///
/// LocalService can write in it, so nothing here goes by a path in it
/// that could lead elsewhere: the folder's own permissions are set through
/// a handle that refuses a junction, its files are never re-permitted (one
/// could be a hard link to a SYSTEM file), and files are only ever created
/// new, never written through what is there.
fn prepare_network_dir() -> anyhow::Result<PathBuf> {
    let dir = network_dir()?;
    // Inside the service's own folder, which only SYSTEM and administrators
    // can change: making it can't be redirected.
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    launch::secure_dir(&dir, launch::NETWORK_DIR_SDDL)?;
    let old = data_dir()?;
    for name in super::NETWORK_FILES {
        let from = old.join(name);
        let Ok(contents) = std::fs::read(&from) else {
            continue;
        };
        match write_new(&dir.join(name), &contents) {
            Ok(()) => {}
            // Already moved: the network host's copy is the current one.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e).with_context(|| format!("move {name} for the network host")),
        }
        let _ = std::fs::remove_file(&from);
    }
    Ok(dir)
}

/// Creates `path` with `contents`, failing if anything is there already
/// (so never writing through a link someone put there). The new file takes
/// its folder's permissions.
fn write_new(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(contents)
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

pub fn install(args: &[String]) -> anyhow::Result<String> {
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
    Ok(format!(
        "Installed and started the {SERVICE_NAME} service ({}).\n\
         Its data (certificate, paired browsers, logs) is in {}.\n\n\
         It uses the same ports as the tray app's own host, so the tray app stops hosting \
         while the service runs. The service has its own certificate and paired browsers: \
         pair again with \"Pair through the service\" in the tray app.",
        exe.display(),
        data_dir()?.display()
    ))
}

/// Whether the service is installed (`Some`), and running.
pub fn query() -> anyhow::Result<Option<bool>> {
    let manager = manager(ServiceManagerAccess::CONNECT)?;
    match manager.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS) {
        Ok(s) => Ok(Some(
            s.query_status()?.current_state != ServiceState::Stopped,
        )),
        Err(windows_service::Error::Winapi(e)) if e.raw_os_error() == Some(1060) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// The host reads `--cert` and `--key` as LocalService (or SYSTEM): copies
/// them into the network host's data folder, which only administrators (and
/// that host) can change, so a user who can write where they came from
/// can't swap them later.
/// The same goes for `--duckdns`'s token, which is always passed as a
/// file in that folder.
fn copy_certificate(
    args: &[String],
    opts: &crate::options::ServeOptions,
) -> anyhow::Result<Vec<String>> {
    let mut copies = Vec::new();
    if let Some((cert, key)) = &opts.cert {
        copies.push(("--cert", cert, super::CERT_COPY));
        copies.push(("--key", key, super::KEY_COPY));
    }
    let mut args = args.to_vec();
    if let Some(duck) = &opts.duckdns {
        if !args.iter().any(|a| a == "--duckdns-token-file") {
            args.extend(["--duckdns-token-file".into(), String::new()]);
        }
        copies.push(("--duckdns-token-file", &duck.token_file, super::TOKEN_COPY));
    }
    if copies.is_empty() {
        return Ok(args);
    }
    // Where the network host reads them.
    let dir = prepare_network_dir()?;
    for (flag, src, name) in copies {
        let dest = dir.join(name);
        // Installing again with the copy itself: nothing to copy.
        if !same_file(src, &dest) {
            let contents = std::fs::read(src).with_context(|| format!("read {}", src.display()))?;
            // Whatever is there goes (a link goes, not what it points at),
            // and the copy is made new.
            match std::fs::remove_file(&dest) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    return Err(e).with_context(|| format!("replace {}", dest.display()));
                }
                _ => {}
            }
            write_new(&dest, &contents)
                .with_context(|| format!("copy {} to {}", src.display(), dest.display()))?;
        }
        args = super::replace_value(&args, flag, &dest.display().to_string());
    }
    Ok(args)
}

pub fn update() -> anyhow::Result<String> {
    let manager = manager(ServiceManagerAccess::CONNECT)?;
    let Some(service) = open_installed(&manager)? else {
        bail!("the service is not installed: install it from the tray app's settings");
    };
    stop_and_wait(&service)?;
    let exe = install_exe()?;
    service.start::<&str>(&[]).context("start the service")?;
    Ok(format!(
        "Updated {} and restarted the service.",
        exe.display()
    ))
}

pub fn uninstall() -> anyhow::Result<String> {
    let manager = manager(ServiceManagerAccess::CONNECT)?;
    let Some(service) = open_installed(&manager)? else {
        return Ok("The service is not installed.".into());
    };
    stop_and_wait(&service)?;
    service.delete().context("delete the service")?;
    drop(service);
    let dir = install_dir()?;
    let _ = std::fs::remove_file(dir.join(EXE_NAME));
    let _ = std::fs::remove_dir(&dir);
    Ok(format!(
        "Removed the service; its data stays in {}.",
        data_dir()?.display()
    ))
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
    service_dispatcher::start(SERVICE_NAME, ffi_service_main).context(
        "not started by the service control manager (install it from the tray app's settings)",
    )?;
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
    /// The network host it serves the browser through, unless it runs the
    /// whole host itself.
    network: Option<launch::Child>,
    pipe: String,
    started: Instant,
}

enum NetworkStart {
    /// With the session host's end of the link.
    Started(launch::Child, launch::Owned),
    /// Run the whole host in the session host, as SYSTEM.
    CantSplit,
    /// Try again later.
    Failed(anyhow::Error),
}

/// How long the network host gets to end the session and close its
/// sockets once the session host has gone.
const NETWORK_QUIT_TIMEOUT: Duration = Duration::from_secs(5);

/// Keeps one session host running in the console session.
struct Supervisor {
    exe: PathBuf,
    args: Vec<String>,
    data_dir: PathBuf,
    /// Set by the host to ask for Ctrl+Alt+Del; 0 if it couldn't be made.
    sas_event: usize,
    host: Option<Host>,
    /// The network host can't start on this PC (see [`STARTUP_FAILED`]):
    /// the session host runs the whole host until the service restarts.
    split_broken: bool,
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
            split_broken: false,
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

    /// Starts the network host. Only a refused LocalService logon (or, see
    /// [`stop_network`], a window station) means running the whole host as
    /// SYSTEM instead: anything else could be the network host's own doing,
    /// from a previous run (its folder is writable to it), and is retried.
    fn start_network(&self) -> NetworkStart {
        if self.split_broken {
            tracing::warn!(
                "the network host can't start here; the session host runs the whole host"
            );
            return NetworkStart::CantSplit;
        }
        let token = match launch::network_token() {
            Ok(token) => token,
            Err(e) => {
                tracing::error!(
                    "no LocalService logon for the network host, so the session host runs the \
                     whole host as SYSTEM: {e:#}"
                );
                return NetworkStart::CantSplit;
            }
        };
        let started = (|| {
            let dir = prepare_network_dir()?;
            let args = super::relocate_files(&self.args, &self.data_dir, &dir);
            let (ours, theirs) = launch::link_pipe()?;
            let args = super::network_host_args(theirs.value(), &args, &dir);
            let log = self.data_dir.join("network.log");
            let child = launch::spawn_network(&token, &self.exe, &args, &log, &[theirs.value()])?;
            anyhow::Ok((child, ours))
        })();
        match started {
            Ok((child, ours)) => {
                tracing::info!(pid = child.pid, "started the network host");
                NetworkStart::Started(child, ours)
            }
            Err(e) => NetworkStart::Failed(e),
        }
    }

    fn start_host(&mut self, session: u32) {
        let mut network = match self.start_network() {
            NetworkStart::Started(child, link) => Some((child, link)),
            NetworkStart::CantSplit => None,
            NetworkStart::Failed(e) => {
                tracing::error!("could not start the network host: {e:#}");
                return self.retry_later(Duration::ZERO);
            }
        };
        let started = super::random_pipe_name().and_then(|pipe| {
            let link = network.as_ref().map_or(0, |(_, link)| link.value());
            // The service's own folder, never the network host's: in the
            // fallback this process keeps the certificate and paired
            // browsers, as SYSTEM, where LocalService can't reach.
            let args =
                super::session_host_args(&pipe, self.sas_event, link, &self.args, &self.data_dir);
            let inherit: Vec<usize> = [self.sas_event, link]
                .into_iter()
                .filter(|&h| h != 0)
                .collect();
            let log = self.data_dir.join("host.log");
            let child = launch::spawn(session, &self.exe, &args, &log, &inherit)?;
            // For `console`, which checks the pipe is served by this process.
            let file = self.pipe_file();
            std::fs::write(&file, super::pipe_file_contents(&pipe, child.pid))
                .with_context(|| format!("write {}", file.display()))?;
            Ok((child, pipe))
        });
        // The children have their ends of the link; this process's copy of
        // the session host's goes (the network host's went with `theirs`).
        let network = network.take().map(|(child, _link)| child);
        match started {
            Ok((child, pipe)) => {
                tracing::info!(session, pid = child.pid, "started the host");
                self.host = Some(Host {
                    child,
                    network,
                    pipe,
                    started: Instant::now(),
                });
            }
            Err(e) => {
                tracing::error!(session, "could not start the host: {e:#}");
                if let Some(network) = network {
                    network.kill();
                }
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
        if let Some(network) = &host.network {
            self.split_broken |= stop_network(network);
        }
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
        if let Some(network) = &host.network {
            self.split_broken |= stop_network(network);
        }
        self.backoff = Duration::ZERO;
        self.next_start = Instant::now();
    }
}

/// STATUS_DLL_INIT_FAILED: a process that died before any of its own code
/// ran, which for the network host means Windows wouldn't give it a window
/// station. Nothing from the network can cause it, unlike a crash, so it is
/// the one exit that makes the service stop splitting the host (falling
/// back on a crash would let whoever can crash it get the network code
/// running as SYSTEM).
const STARTUP_FAILED: u32 = 0xC000_0142;

/// Waits for the network host to stop, which it does once the session host
/// is gone (its link closes), and kills it if it doesn't: the next one
/// needs its ports. True if it never started ([`STARTUP_FAILED`]).
fn stop_network(network: &launch::Child) -> bool {
    if let Some(code) = network.exit_code() {
        tracing::warn!(code, "the network host had exited (its log is network.log)");
        if code == STARTUP_FAILED {
            tracing::error!(
                "the network host could not start (no window station for LocalService); \
                 the session host runs the whole host as SYSTEM until the service restarts"
            );
            return true;
        }
    } else if !network.wait(NETWORK_QUIT_TIMEOUT) {
        tracing::warn!("the network host did not stop, killing it");
        network.kill();
        network.wait(Duration::from_secs(2));
    }
    false
}

/// The supervisor's own log, `service.log` in the data folder.
pub fn service_log() -> Option<std::fs::File> {
    let dir = data_dir().ok()?;
    std::fs::create_dir_all(&dir).ok()?;
    launch::open_log(&dir.join("service.log")).ok()
}
