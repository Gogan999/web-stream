//! Keep the TLS key and device store private to the account running the
//! host. Unix gets 0600/0700 modes at creation; Windows gets a protected DACL
//! allowing only the file's owner and SYSTEM, since a data dir under
//! `ProgramData` would otherwise be readable by every local user.

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

    // Full access for Owner Rights and SYSTEM, inherited by anything created
    // inside a directory; P = do not inherit the parent's ACEs.
    let sddl: Vec<u16> = "D:P(A;OICI;FA;;;OW)(A;OICI;FA;;;SY)"
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
