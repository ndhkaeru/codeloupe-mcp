use std::io;
use std::path::Path;

pub(super) fn classify(
    path: &Path,
    error: &io::Error,
    not_found_code: &'static str,
) -> &'static str {
    if is_file_lock(path, error) {
        return "file_locked";
    }

    match error.kind() {
        io::ErrorKind::PermissionDenied => "permission_denied",
        io::ErrorKind::NotFound => not_found_code,
        io::ErrorKind::AlreadyExists => "already_exists",
        io::ErrorKind::InvalidInput => "invalid_input",
        _ => "io_error",
    }
}

#[cfg(windows)]
fn is_file_lock(path: &Path, error: &io::Error) -> bool {
    match error.raw_os_error() {
        Some(32 | 33) => true,
        Some(5) => access_denied_is_lock(path),
        _ => false,
    }
}

#[cfg(not(windows))]
fn is_file_lock(_path: &Path, _error: &io::Error) -> bool {
    false
}

#[cfg(windows)]
fn access_denied_is_lock(path: &Path) -> bool {
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt;

    const DELETE_ACCESS: u32 = 0x0001_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const FILE_SHARE_DELETE: u32 = 0x0000_0004;

    if !path.is_file()
        || path
            .metadata()
            .is_ok_and(|metadata| metadata.permissions().readonly())
    {
        return false;
    }

    OpenOptions::new()
        .access_mode(DELETE_ACCESS)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(path)
        .is_err_and(|probe_error| matches!(probe_error.raw_os_error(), Some(32 | 33)))
}

#[cfg(test)]
mod tests {
    use super::classify;
    use std::io;

    #[test]
    fn permission_denied_stays_permission_denied_without_lock_evidence() {
        let error = io::Error::new(io::ErrorKind::PermissionDenied, "denied");
        assert_eq!(
            classify(std::path::Path::new("missing"), &error, "not_found"),
            "permission_denied"
        );
    }

    #[cfg(windows)]
    #[test]
    fn access_denied_with_blocked_delete_share_is_file_locked() {
        use std::fs::OpenOptions;
        use std::os::windows::fs::OpenOptionsExt;

        const FILE_SHARE_READ: u32 = 0x0000_0001;
        const FILE_SHARE_WRITE: u32 = 0x0000_0002;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("locked.txt");
        std::fs::write(&path, "data").unwrap();
        let _locked = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .open(&path)
            .unwrap();

        let error = io::Error::from_raw_os_error(5);
        assert_eq!(classify(&path, &error, "not_found"), "file_locked");
    }
}
