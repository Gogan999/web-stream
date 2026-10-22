//! Starting the host's two processes.
//!
//! The session host is a SYSTEM process on the user's screen: the service's
//! own token is copied, moved to the console session and stripped of every
//! privilege the host doesn't need (debugging, loading drivers, taking
//! ownership, acting as part of the OS and so on), then the host is started
//! on that session's interactive desktop.
//!
//! The network host runs as LocalService with every privilege but one
//! removed (in particular impersonation, which would let it borrow a SYSTEM
//! token), in session 0, away from the user's desktop. It holds everything
//! that parses what comes off the network.
//!
//! Each writes to a log file and runs in a job object, so it never outlives
//! the service. A pipe made here links the two ([`link_pipe`]); each end is
//! inherited by one of them, so nothing else can ever connect to it.

use std::ffi::c_void;
use std::fs::File;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::time::Duration;

use anyhow::Context;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_NOT_ALL_ASSIGNED, GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE,
    HANDLE_FLAG_INHERIT, HLOCAL, LUID, LocalFree, SetHandleInformation, WAIT_OBJECT_0,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1, SE_FILE_OBJECT,
    SetSecurityInfo,
};
use windows::Win32::Security::{
    AdjustTokenPrivileges, DACL_SECURITY_INFORMATION, DuplicateTokenEx, GetSecurityDescriptorDacl,
    GetTokenInformation, LOGON32_LOGON_SERVICE, LOGON32_PROVIDER_DEFAULT, LogonUserW,
    LookupPrivilegeValueW, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
    SE_PRIVILEGE_REMOVED, SECURITY_ATTRIBUTES, SecurityIdentification, SetTokenInformation,
    TOKEN_ALL_ACCESS, TOKEN_DUPLICATE, TOKEN_PRIVILEGES, TOKEN_QUERY, TokenPrimary,
    TokenPrivileges, TokenSessionId,
};
use windows::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_FIRST_PIPE_INSTANCE,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAG_OVERLAPPED, FILE_FLAGS_AND_ATTRIBUTES,
    FILE_READ_ATTRIBUTES, FILE_SHARE_NONE, FILE_SHARE_READ, GetFileInformationByHandle,
    OPEN_EXISTING, PIPE_ACCESS_DUPLEX, READ_CONTROL, SECURITY_IDENTIFICATION,
    SECURITY_SQOS_PRESENT, WRITE_DAC,
};
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
use windows::Win32::System::Pipes::{
    CreateNamedPipeW, CreatePipe, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
    PIPE_WAIT, PeekNamedPipe,
};
use windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW,
    DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess,
    GetExitCodeProcess, InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
    OpenProcessToken, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_INFORMATION, ResumeThread,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute,
    WaitForSingleObject,
};
use windows::core::{PCWSTR, PWSTR};

use super::quote_arg;

/// Privileges the session host keeps; every other one is removed from its
/// token. Capture, encoding, audio and input need none beyond these.
const KEEP_PRIVILEGES: &[&str] = &[
    "SeChangeNotifyPrivilege",
    // Raising the GPU and capture thread priority.
    "SeIncreaseBasePriorityPrivilege",
    "SeCreateGlobalPrivilege",
];

/// The network host's only privilege: opening its files by path without
/// rights on every folder above them (its data folder is inside SYSTEM's
/// profile).
const NETWORK_KEEP_PRIVILEGES: &[&str] = &["SeChangeNotifyPrivilege"];

/// The network host's data folder: full access for SYSTEM, administrators
/// and LocalService, inherited by what is created inside; nothing from the
/// folder above.
pub const NETWORK_DIR_SDDL: &str = "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;LS)";

/// The link pipe: SYSTEM only. Both ends are opened here and handed down,
/// so no process ever opens it by name.
const LINK_SDDL: &str = "D:P(A;;GA;;;SY)";
const LINK_BUFFER: u32 = 1 << 20;

/// Log files are started over above this size; the previous one is kept
/// as `.old`.
const MAX_LOG: u64 = 10 * 1024 * 1024;

/// A handle closed on drop.
pub struct Owned(HANDLE);

impl Owned {
    /// The raw value, as a child that inherits it sees it.
    pub fn value(&self) -> usize {
        self.0.0 as usize
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: we own the handle.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

/// The session at the PC's own screen and keyboard, if there is one right
/// now (there briefly isn't while it changes).
pub fn console_session() -> Option<u32> {
    // SAFETY: no arguments.
    let id = unsafe { WTSGetActiveConsoleSessionId() };
    (id != u32::MAX).then_some(id)
}

pub struct Child {
    process: Owned,
    // Closing the job kills the host: kept until the child is dropped.
    job: Owned,
    pub session: u32,
    pub pid: u32,
}

impl Child {
    /// The exit code, once the host has exited.
    pub fn exit_code(&self) -> Option<u32> {
        if !self.wait(Duration::ZERO) {
            return None;
        }
        let mut code = 0;
        // SAFETY: a valid process handle.
        unsafe { GetExitCodeProcess(self.process.0, &mut code) }.ok()?;
        Some(code)
    }

    /// True once the host has exited, waiting up to `timeout`.
    pub fn wait(&self, timeout: Duration) -> bool {
        let ms = timeout.as_millis().min(u128::from(u32::MAX - 1)) as u32;
        // SAFETY: a valid process handle.
        unsafe { WaitForSingleObject(self.process.0, ms) == WAIT_OBJECT_0 }
    }

    pub fn kill(&self) {
        // SAFETY: a valid job handle.
        let _ = unsafe { TerminateJobObject(self.job.0, 1) };
    }
}

/// Starts `exe args` as SYSTEM in `session`, its output appended to `log`.
/// It inherits the handles in `inherit` (which must be inheritable) and no
/// others.
pub fn spawn(
    session: u32,
    exe: &Path,
    args: &[String],
    log: &Path,
    inherit: &[usize],
) -> anyhow::Result<Child> {
    let token = host_token(session)?;
    spawn_as(&token, r"winsta0\default", session, exe, args, log, inherit)
}

/// Starts `exe args` with `token` (from [`network_token`]), in session 0,
/// like [`spawn`] otherwise.
pub fn spawn_network(
    token: &Owned,
    exe: &Path,
    args: &[String],
    log: &Path,
    inherit: &[usize],
) -> anyhow::Result<Child> {
    // Empty: a window station of its own logon session's, which Windows
    // makes for it, rather than the service's, which it may not open.
    spawn_as(token, "", 0, exe, args, log, inherit)
}

fn spawn_as(
    token: &Owned,
    desktop: &str,
    session: u32,
    exe: &Path,
    args: &[String],
    log: &Path,
    inherit: &[usize],
) -> anyhow::Result<Child> {
    let env = Environment::new(token)?;
    let log = open_log(log)?;
    let log_handle = HANDLE(log.as_raw_handle());
    // Inherited by the host as its stdout and stderr, and nothing else is
    // (the handle list below).
    // SAFETY: a valid handle we own.
    unsafe { SetHandleInformation(log_handle, HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT) }
        .context("log handle")?;
    let job = kill_on_close_job()?;

    let mut command_line: Vec<u16> = std::iter::once(exe.display().to_string())
        .chain(args.iter().cloned())
        .map(|a| quote_arg(&a))
        .collect::<Vec<_>>()
        .join(" ")
        .encode_utf16()
        .chain([0])
        .collect();
    let exe_w = wide(&exe.display().to_string());
    let dir_w = wide(
        &exe.parent()
            .context("the service exe has no folder")?
            .display()
            .to_string(),
    );
    let mut desktop = wide(desktop);

    let attrs = AttributeList::new(1)?;
    let handles: Vec<HANDLE> = std::iter::once(log_handle)
        .chain(inherit.iter().map(|&h| HANDLE(h as *mut _)))
        .collect();
    // SAFETY: `handles` outlives process creation; the size is its own.
    unsafe {
        UpdateProcThreadAttribute(
            attrs.list(),
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            Some(handles.as_ptr().cast()),
            size_of_val(handles.as_slice()),
            None,
            None,
        )
    }
    .context("handle list")?;

    let mut si = STARTUPINFOEXW::default();
    si.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    si.StartupInfo.lpDesktop = PWSTR(desktop.as_mut_ptr());
    si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    si.StartupInfo.hStdOutput = log_handle;
    si.StartupInfo.hStdError = log_handle;
    si.lpAttributeList = attrs.list();
    let mut pi = PROCESS_INFORMATION::default();
    // SAFETY: every pointer is valid for the call; the strings are
    // NUL-terminated and the command line is writable.
    unsafe {
        CreateProcessAsUserW(
            Some(token.0),
            PCWSTR(exe_w.as_ptr()),
            Some(PWSTR(command_line.as_mut_ptr())),
            None,
            None,
            true,
            CREATE_SUSPENDED
                | CREATE_UNICODE_ENVIRONMENT
                | EXTENDED_STARTUPINFO_PRESENT
                | CREATE_NO_WINDOW,
            Some(env.0),
            PCWSTR(dir_w.as_ptr()),
            &si.StartupInfo,
            &mut pi,
        )
    }
    .context("CreateProcessAsUser")?;
    let process = Owned(pi.hProcess);
    let thread = Owned(pi.hThread);
    // SAFETY: valid handles from CreateProcessAsUserW.
    unsafe {
        if let Err(e) = AssignProcessToJobObject(job.0, process.0) {
            let _ = TerminateProcess(process.0, 1);
            return Err(e).context("job object");
        }
        if ResumeThread(thread.0) == u32::MAX {
            let e = windows::core::Error::from_thread();
            let _ = TerminateProcess(process.0, 1);
            return Err(e).context("ResumeThread");
        }
    }
    Ok(Child {
        process,
        job,
        session,
        pid: pi.dwProcessId,
    })
}

/// The service's SYSTEM token, as a primary token in `session` without the
/// privileges the host doesn't need.
fn host_token(session: u32) -> anyhow::Result<Owned> {
    let mut own = HANDLE::default();
    // SAFETY: the pseudo handle needs no closing.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_DUPLICATE | TOKEN_QUERY, &mut own) }
        .context("OpenProcessToken")?;
    let own = Owned(own);
    let mut dup = HANDLE::default();
    // SAFETY: `own` is a valid token handle.
    unsafe {
        DuplicateTokenEx(
            own.0,
            TOKEN_ALL_ACCESS,
            None,
            SecurityIdentification,
            TokenPrimary,
            &mut dup,
        )
    }
    .context("DuplicateTokenEx")?;
    let token = Owned(dup);
    // Needs SeTcbPrivilege, which the service (not the host) has.
    // SAFETY: a u32 for TokenSessionId.
    unsafe {
        SetTokenInformation(
            token.0,
            TokenSessionId,
            (&raw const session).cast::<c_void>(),
            size_of::<u32>() as u32,
        )
    }
    .context("SetTokenInformation(TokenSessionId)")?;
    strip_privileges(&token, KEEP_PRIVILEGES)?;
    Ok(token)
}

/// A LocalService token with no privilege but [`NETWORK_KEEP_PRIVILEGES`],
/// for the network host. LocalService has impersonation and token assignment, either of
/// which could turn a compromised network host back into SYSTEM.
pub fn network_token() -> anyhow::Result<Owned> {
    let (user, domain) = (wide("LocalService"), wide("NT AUTHORITY"));
    let mut token = HANDLE::default();
    // SAFETY: NUL-terminated strings; `token` receives a handle we own.
    unsafe {
        LogonUserW(
            PCWSTR(user.as_ptr()),
            PCWSTR(domain.as_ptr()),
            PCWSTR::null(),
            LOGON32_LOGON_SERVICE,
            LOGON32_PROVIDER_DEFAULT,
            &mut token,
        )
    }
    .context("LogonUser(LocalService)")?;
    let token = Owned(token);
    strip_privileges(&token, NETWORK_KEEP_PRIVILEGES)?;
    Ok(token)
}

fn strip_privileges(token: &Owned, keep: &[&str]) -> anyhow::Result<()> {
    let mut len = 0u32;
    // SAFETY: a size query; fails with the size needed.
    let _ = unsafe { GetTokenInformation(token.0, TokenPrivileges, None, 0, &mut len) };
    // u64s keep TOKEN_PRIVILEGES aligned.
    let mut buf = vec![0u64; (len as usize).div_ceil(8)];
    // SAFETY: `buf` is writable for `len` bytes.
    unsafe {
        GetTokenInformation(
            token.0,
            TokenPrivileges,
            Some(buf.as_mut_ptr().cast()),
            len,
            &mut len,
        )
    }
    .context("GetTokenInformation(TokenPrivileges)")?;
    let keep: Vec<LUID> = keep
        .iter()
        .filter_map(|name| {
            let mut luid = LUID::default();
            // SAFETY: a NUL-terminated name.
            unsafe { LookupPrivilegeValueW(None, PCWSTR(wide(name).as_ptr()), &mut luid) }
                .ok()
                .map(|()| luid)
        })
        .collect();
    let tp = buf.as_mut_ptr().cast::<TOKEN_PRIVILEGES>();
    // SAFETY: GetTokenInformation wrote a TOKEN_PRIVILEGES with
    // PrivilegeCount entries into `buf`.
    let privileges = unsafe {
        std::slice::from_raw_parts_mut(
            (&raw mut (*tp).Privileges).cast::<windows::Win32::Security::LUID_AND_ATTRIBUTES>(),
            (*tp).PrivilegeCount as usize,
        )
    };
    for p in privileges.iter_mut() {
        let kept = keep
            .iter()
            .any(|k| k.LowPart == p.Luid.LowPart && k.HighPart == p.Luid.HighPart);
        if !kept {
            p.Attributes = SE_PRIVILEGE_REMOVED;
        }
    }
    // SAFETY: `tp` points at a valid TOKEN_PRIVILEGES.
    unsafe { AdjustTokenPrivileges(token.0, false, Some(tp), 0, None, None) }
        .context("AdjustTokenPrivileges")?;
    // SAFETY: no arguments.
    if unsafe { GetLastError() } == ERROR_NOT_ALL_ASSIGNED {
        anyhow::bail!("could not remove the host's privileges");
    }
    Ok(())
}

struct Environment(*mut c_void);

impl Environment {
    fn new(token: &Owned) -> anyhow::Result<Self> {
        let mut env = std::ptr::null_mut();
        // SAFETY: `env` receives a block freed in drop.
        unsafe { CreateEnvironmentBlock(&mut env, Some(token.0), false) }
            .context("CreateEnvironmentBlock")?;
        Ok(Self(env))
    }
}

impl Drop for Environment {
    fn drop(&mut self) {
        // SAFETY: from CreateEnvironmentBlock.
        let _ = unsafe { DestroyEnvironmentBlock(self.0) };
    }
}

struct AttributeList(Vec<u64>);

impl AttributeList {
    fn new(count: u32) -> anyhow::Result<Self> {
        let mut size = 0usize;
        // SAFETY: a size query; fails with the size needed.
        let _ = unsafe { InitializeProcThreadAttributeList(None, count, None, &mut size) };
        let buf = vec![0u64; size.div_ceil(8)];
        let list = LPPROC_THREAD_ATTRIBUTE_LIST(buf.as_ptr().cast_mut().cast());
        // SAFETY: the buffer is `size` bytes, aligned. Only wrapped (and so
        // deleted on drop) once initialized.
        unsafe { InitializeProcThreadAttributeList(Some(list), count, None, &mut size) }
            .context("InitializeProcThreadAttributeList")?;
        Ok(Self(buf))
    }

    fn list(&self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        LPPROC_THREAD_ATTRIBUTE_LIST(self.0.as_ptr().cast_mut().cast())
    }
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        // SAFETY: initialized in new.
        unsafe { DeleteProcThreadAttributeList(self.list()) };
    }
}

fn kill_on_close_job() -> anyhow::Result<Owned> {
    // SAFETY: an unnamed job with default security.
    let job = Owned(unsafe { CreateJobObjectW(None, PCWSTR::null()) }.context("CreateJobObject")?);
    let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: `info` is the structure the class names.
    unsafe {
        SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            (&raw const info).cast(),
            size_of_val(&info) as u32,
        )
    }
    .context("SetInformationJobObject")?;
    Ok(job)
}

/// Opens `path` for appending, starting over (keeping one `.old`) once it
/// is large.
pub fn open_log(path: &Path) -> anyhow::Result<File> {
    if std::fs::metadata(path).is_ok_and(|m| m.len() > MAX_LOG) {
        let _ = std::fs::rename(path, path.with_extension("log.old"));
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open {}", path.display()))
}

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// Two connected ends of a fresh pipe, both inheritable: one for each
/// process. Overlapped, as tokio needs. Opened for identification only, so
/// neither end can act as the other.
pub fn link_pipe() -> anyhow::Result<(Owned, Owned)> {
    let name = format!(r"\\.\pipe\web-stream-link-{}", super::random_hex()?);
    let name_w = wide(&name);
    let sd = SecurityDescriptor::new(LINK_SDDL)?;
    let sa = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0.0,
        bInheritHandle: true.into(),
    };
    // SAFETY: `name_w` is NUL-terminated and `sa` valid for the call.
    let server = unsafe {
        CreateNamedPipeW(
            PCWSTR(name_w.as_ptr()),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            LINK_BUFFER,
            LINK_BUFFER,
            0,
            Some(&sa),
        )
    };
    if server.is_invalid() {
        return Err(windows::core::Error::from_thread()).context("CreateNamedPipe");
    }
    let server = Owned(server);
    // SAFETY: as above.
    let client = unsafe {
        CreateFileW(
            PCWSTR(name_w.as_ptr()),
            (GENERIC_READ | GENERIC_WRITE).0,
            FILE_SHARE_NONE,
            Some(&sa),
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
            None,
        )
    }
    .context("open the link pipe")?;
    Ok((server, Owned(client)))
}

/// A pipe the network host writes one byte to as the first thing its own
/// code does, before it touches the network: (the service's read end, the
/// inheritable write end). What was written stays there for the service to
/// see, whatever the network host does afterwards.
pub fn alive_pipe() -> anyhow::Result<(Owned, Owned)> {
    let sa = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: true.into(),
    };
    let (mut read, mut write) = (HANDLE::default(), HANDLE::default());
    // SAFETY: the out pointers and `sa` are valid for the call.
    unsafe { CreatePipe(&mut read, &mut write, Some(&sa), 0) }.context("CreatePipe")?;
    Ok((Owned(read), Owned(write)))
}

/// Whether anything was written to the pipe whose read end this is.
pub fn pipe_has_data(read: &Owned) -> bool {
    let mut available = 0u32;
    // SAFETY: `read` is an open pipe handle and `available` valid.
    unsafe { PeekNamedPipe(read.0, None, 0, None, Some(&mut available), None) }.is_ok()
        && available > 0
}

/// Sets the DACL of the folder `path` from `sddl`, replacing what it
/// inherited, through a handle: refused if the folder is a junction or other
/// reparse point (whoever can write the folder could have made it one, to
/// send this change elsewhere).
pub fn secure_dir(path: &Path, sddl: &str) -> anyhow::Result<()> {
    let sd = SecurityDescriptor::new(sddl)?;
    let path_w = wide(&path.display().to_string());
    // SAFETY: a NUL-terminated path; the handle is closed by `Owned`.
    let dir = Owned(
        unsafe {
            CreateFileW(
                PCWSTR(path_w.as_ptr()),
                (READ_CONTROL | WRITE_DAC | FILE_READ_ATTRIBUTES).0,
                FILE_SHARE_READ,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                None,
            )
        }
        .with_context(|| format!("open {}", path.display()))?,
    );
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: a valid handle and a structure to fill.
    unsafe { GetFileInformationByHandle(dir.0, &mut info) }
        .context("GetFileInformationByHandle")?;
    let attributes = FILE_FLAGS_AND_ATTRIBUTES(info.dwFileAttributes);
    if !attributes.contains(FILE_ATTRIBUTE_DIRECTORY)
        || attributes.contains(FILE_ATTRIBUTE_REPARSE_POINT)
    {
        anyhow::bail!("{} is not a plain folder", path.display());
    }
    let mut present = windows::core::BOOL::default();
    let mut defaulted = windows::core::BOOL::default();
    let mut dacl = std::ptr::null_mut();
    // SAFETY: `sd` is a valid descriptor; `dacl` points into it.
    unsafe { GetSecurityDescriptorDacl(sd.0, &mut present, &mut dacl, &mut defaulted) }
        .context("GetSecurityDescriptorDacl")?;
    // SAFETY: a valid handle opened with WRITE_DAC, and a DACL that
    // outlives the call.
    unsafe {
        SetSecurityInfo(
            dir.0,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(dacl.cast_const()),
            None,
        )
    }
    .ok()
    .with_context(|| format!("set the permissions of {}", path.display()))
}

/// A security descriptor from SDDL, freed on drop.
struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

impl SecurityDescriptor {
    fn new(sddl: &str) -> anyhow::Result<Self> {
        let sddl = wide(sddl);
        let mut sd = PSECURITY_DESCRIPTOR::default();
        // SAFETY: `sddl` is NUL-terminated; `sd` is freed in drop.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(sddl.as_ptr()),
                SDDL_REVISION_1,
                &mut sd,
                None,
            )
        }
        .context("security descriptor")?;
        Ok(Self(sd))
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
        unsafe { LocalFree(Some(HLOCAL(self.0.0))) };
    }
}
