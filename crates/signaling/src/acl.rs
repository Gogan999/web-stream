//! Keep the TLS key and device store private to the account running the
//! host. Unix gets 0600/0700 modes at creation; Windows gets a protected DACL
//! allowing only the user running the host and SYSTEM, since a data dir under
//! `ProgramData` would otherwise be readable by every local user.
//!
//! The Windows ACE names the user's own SID, not "owner rights": a host run
//! as administrator creates files owned by the Administrators group, and an
//! owner-rights ACE then locks the same user out when they run the host
//! (or the tray app) normally.

use std::path::Path;

/// Restrict `dir` and every regular file directly inside it. Links are left
/// alone, so nothing outside the directory is changed through one.
pub fn restrict_tree(dir: &Path) -> std::io::Result<()> {
    restrict_to_owner(dir)?;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            restrict_to_owner(&entry.path())?;
        }
    }
    Ok(())
}

#[cfg(windows)]
pub fn restrict_to_owner(path: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
        SetFileSecurityW,
    };

    // Full access for this user and SYSTEM, inherited by anything created
    // inside a directory; P = do not inherit the parent's ACEs.
    let sddl: Vec<u16> = format!("D:P(A;OICI;FA;;;{})(A;OICI;FA;;;SY)", user_sid()?)
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: both strings are NUL-terminated and outlive the calls; `sd` is
    // allocated by the first call and freed with LocalFree below.
    unsafe {
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut sd,
            std::ptr::null_mut(),
        ) == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        let ok = SetFileSecurityW(
            wide.as_ptr(),
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            sd,
        );
        let err = std::io::Error::last_os_error();
        LocalFree(sd);
        if ok == 0 {
            return Err(err);
        }
    }
    Ok(())
}

/// The SID of the user this process runs as, in string form ("S-1-5-21-...").
/// The same when elevated.
#[cfg(windows)]
fn user_sid() -> std::io::Result<String> {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LocalFree};
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    static SID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    if let Some(sid) = SID.get() {
        return Ok(sid.clone());
    }
    // SAFETY: the token handle is closed before returning; `buf` is large
    // enough for a TOKEN_USER (checked by GetTokenInformation) and u64-aligned;
    // the string from ConvertSidToStringSidW is NUL-terminated and freed
    // with LocalFree.
    let sid = unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut buf = [0u64; 64];
        let mut len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenUser,
            buf.as_mut_ptr().cast(),
            std::mem::size_of_val(&buf) as u32,
            &mut len,
        );
        let err = std::io::Error::last_os_error();
        CloseHandle(token);
        if ok == 0 {
            return Err(err);
        }
        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut wide: windows_sys::core::PWSTR = std::ptr::null_mut();
        if ConvertSidToStringSidW(user.User.Sid, &mut wide) == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let len = (0..).take_while(|&i| *wide.add(i) != 0).count();
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(wide, len));
        LocalFree(wide.cast());
        s
    };
    Ok(SID.get_or_init(|| sid).clone())
}

#[cfg(unix)]
pub fn restrict_to_owner(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = if path.is_dir() { 0o700 } else { 0o600 };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(any(unix, windows)))]
pub fn restrict_to_owner(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn tightens_existing_files() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("devices.json");
        std::fs::write(&file, b"{}").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        super::restrict_tree(dir.path()).unwrap();
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&file), 0o600);
        assert_eq!(mode(dir.path()), 0o700);
    }
}
