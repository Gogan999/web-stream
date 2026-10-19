//! Starting the session host: a SYSTEM process on the user's screen.
//!
//! The service's own token is copied, moved to the console session and
//! stripped of every privilege the host doesn't need (debugging, loading
//! drivers, taking ownership, acting as part of the OS and so on), then the
//! host is started on that session's interactive desktop with its output
//! going to a log file. It runs in a job object, so it never outlives the
//! service.

use std::ffi::c_void;
use std::fs::File;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::time::Duration;

use anyhow::Context;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_NOT_ALL_ASSIGNED, GetLastError, HANDLE, HANDLE_FLAG_INHERIT, LUID,
    SetHandleInformation, WAIT_OBJECT_0,
};
use windows::Win32::Security::{
    AdjustTokenPrivileges, DuplicateTokenEx, GetTokenInformation, LookupPrivilegeValueW,
    SE_PRIVILEGE_REMOVED, SecurityIdentification, SetTokenInformation, TOKEN_ALL_ACCESS,
    TOKEN_DUPLICATE, TOKEN_PRIVILEGES, TOKEN_QUERY, TokenPrimary, TokenPrivileges, TokenSessionId,
};
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
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

/// Privileges the host keeps; every other one is removed from its token.
/// Capture, encoding, audio and input need none beyond these.
const KEEP_PRIVILEGES: &[&str] = &[
    "SeChangeNotifyPrivilege",
    // Raising the GPU and capture thread priority.
    "SeIncreaseBasePriorityPrivilege",
    "SeCreateGlobalPrivilege",
];

/// Log files are started over above this size; the previous one is kept
/// as `.old`.
const MAX_LOG: u64 = 10 * 1024 * 1024;

/// A handle closed on drop.
struct Owned(HANDLE);

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
pub fn spawn(session: u32, exe: &Path, args: &[String], log: &Path) -> anyhow::Result<Child> {
    let token = host_token(session)?;
    let env = Environment::new(&token)?;
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
    let mut desktop = wide(r"winsta0\default");

    let attrs = AttributeList::new(1)?;
    let handles = [log_handle];
    // SAFETY: `handles` outlives process creation; the size is its own.
    unsafe {
        UpdateProcThreadAttribute(
            attrs.list(),
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            Some(handles.as_ptr().cast()),
            size_of_val(&handles),
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
        ResumeThread(thread.0);
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
    strip_privileges(&token)?;
    Ok(token)
}

fn strip_privileges(token: &Owned) -> anyhow::Result<()> {
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
    let keep: Vec<LUID> = KEEP_PRIVILEGES
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
        let list = Self(vec![0u64; size.div_ceil(8)]);
        // SAFETY: the buffer is `size` bytes, aligned.
        unsafe { InitializeProcThreadAttributeList(Some(list.list()), count, None, &mut size) }
            .context("InitializeProcThreadAttributeList")?;
        Ok(list)
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
