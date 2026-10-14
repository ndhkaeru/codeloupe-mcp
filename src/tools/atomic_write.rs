use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;

#[cfg(windows)]
const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
#[cfg(windows)]
const MOVEFILE_WRITE_THROUGH: u32 = 0x8;

#[cfg(windows)]
unsafe extern "system" {
    fn MoveFileExW(existing_file_name: *const u16, new_file_name: *const u16, flags: u32) -> i32;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Publication {
    pub(crate) atomic: bool,
    pub(crate) permissions_preserved: bool,
}

impl Publication {
    pub(crate) const ATOMIC: Self = Self {
        atomic: true,
        permissions_preserved: true,
    };

    pub(crate) fn attach(self, response: &mut serde_json::Value) {
        if !self.atomic {
            response["publication_atomic"] = serde_json::json!(false);
        }
        if !self.permissions_preserved {
            response["permissions_preserved"] = serde_json::json!(false);
        }
    }
}

#[cfg(test)]
pub fn write_bytes(path: &Path, bytes: &[u8], replace_existing: bool) -> io::Result<()> {
    write_bytes_with_status(path, bytes, replace_existing).map(|_| ())
}

pub(crate) fn write_bytes_with_status(
    path: &Path,
    bytes: &[u8],
    replace_existing: bool,
) -> io::Result<Publication> {
    write_bytes_with_permissions(path, bytes, replace_existing, |file, permissions| {
        file.set_permissions(permissions)
    })
}

fn write_bytes_with_permissions<P>(
    path: &Path,
    bytes: &[u8],
    replace_existing: bool,
    set_permissions: P,
) -> io::Result<Publication>
where
    P: FnOnce(&File, fs::Permissions) -> io::Result<()>,
{
    let permissions = if replace_existing {
        Some(fs::metadata(path)?.permissions())
    } else {
        None
    };
    let temp_path = unique_temp_path(parent(path), path);
    let cleanup = TempCleanup(temp_path.clone());
    let permissions_preserved = {
        let mut file = open_new_file(&temp_path, permissions.as_ref())?;
        file.write_all(bytes)?;
        let preserved = permissions
            .as_ref()
            .is_none_or(|mode| set_permissions(&file, mode.clone()).is_ok());
        file.sync_all()?;
        preserved
    };
    let mut publication = publish(&temp_path, path, replace_existing, None)?;
    publication.permissions_preserved &= permissions_preserved;
    drop(cleanup);
    notify(path);
    Ok(publication)
}

/// Streams into a sibling temporary file, verifies it, and publishes without
/// replacing an existing destination. Returns whether publication was atomic.
pub(crate) fn copy_verified(
    source: &Path,
    target: &Path,
    expected_hash: &str,
) -> io::Result<Publication> {
    let source_canonical = crate::security::path_guard::GUARD.check_path(source);
    let target_canonical = crate::security::path_guard::GUARD.check_path(target);
    let temp_path = unique_temp_path(parent(target), target);
    let cleanup = TempCleanup(temp_path.clone());
    let mut input = File::open(source)?;
    let permissions = if cfg!(windows) {
        None
    } else {
        Some(input.metadata()?.permissions())
    };
    let temp_permissions_preserved = {
        let mut output = open_new_file(&temp_path, permissions.as_ref())?;
        io::copy(&mut input, &mut output)?;
        let preserved = permissions
            .as_ref()
            .is_none_or(|mode| output.set_permissions(mode.clone()).is_ok());
        output.sync_all()?;
        preserved
    };
    if super::file_hash::sha256_file(&temp_path)? != expected_hash
        || super::file_hash::sha256_file(source)? != expected_hash
    {
        return Err(io::Error::other("source changed while copying"));
    }
    if crate::security::path_guard::GUARD.check_path(source) != source_canonical
        || crate::security::path_guard::GUARD.check_path(target) != target_canonical
    {
        return Err(io::Error::other("canonical path changed while copying"));
    }
    let mut publication = publish(&temp_path, target, false, permissions.as_ref())?;
    if publication.atomic {
        publication.permissions_preserved = temp_permissions_preserved;
    }
    drop(cleanup);
    notify(target);
    Ok(publication)
}

/// Set the desired Unix mode before any bytes are written. Umask can narrow
/// these bits; a later best-effort fchmod may restore them, but its failure
/// must never expose bytes with broader mode bits.
fn open_new_file(path: &Path, permissions: Option<&fs::Permissions>) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    if let Some(permissions) = permissions {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        options.mode(permissions.mode() & 0o7777);
    }
    #[cfg(not(unix))]
    let _ = permissions;
    options.open(path)
}

fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn publish(
    temp_path: &Path,
    path: &Path,
    replace_existing: bool,
    permissions: Option<&fs::Permissions>,
) -> io::Result<Publication> {
    #[cfg(not(unix))]
    let _ = permissions;
    if !replace_existing {
        #[cfg(windows)]
        {
            return rename_windows(temp_path, path, false).map(|_| Publication::ATOMIC);
        }
        #[cfg(unix)]
        {
            return publish_no_replace_with(
                temp_path,
                path,
                permissions,
                native_rename_no_replace,
                |a, b| fs::hard_link(a, b),
            );
        }
        #[cfg(not(any(windows, unix)))]
        {
            return fs::hard_link(temp_path, path).map(|_| Publication::ATOMIC);
        }
    }
    match fs::rename(temp_path, path) {
        Ok(()) => Ok(Publication::ATOMIC),
        Err(err) => {
            #[cfg(windows)]
            {
                let _ = err;
                rename_windows(temp_path, path, true).map(|_| Publication::ATOMIC)
            }
            #[cfg(not(windows))]
            {
                Err(err)
            }
        }
    }
}

/// Linux uses a syscall so an old libc does not require a renameat2 symbol.
/// All native variants decide no-replace in the OS operation itself.
#[cfg(unix)]
pub(crate) fn native_rename_no_replace(source: &Path, target: &Path) -> io::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let source = CString::new(source.as_os_str().as_bytes())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let target = CString::new(target.as_os_str().as_bytes())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        #[cfg(target_os = "linux")]
        let result = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                libc::AT_FDCWD,
                source.as_ptr(),
                libc::AT_FDCWD,
                target.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        #[cfg(target_os = "macos")]
        let result =
            unsafe { libc::renamex_np(source.as_ptr(), target.as_ptr(), libc::RENAME_EXCL) };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (source, target);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "native no-replace rename unavailable",
        ))
    }
}

/// EPERM may mean that vfat/FUSE does not implement links or rename flags.
/// Retrying with O_EXCL does not bypass permissions and still cannot overwrite.
#[cfg(unix)]
pub(crate) fn publication_fallback_allowed(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::Unsupported
            | io::ErrorKind::PermissionDenied
            | io::ErrorKind::InvalidInput
            | io::ErrorKind::CrossesDevices
    ) || error.raw_os_error() == Some(libc::ENOSYS)
}

#[cfg(unix)]
fn publish_no_replace_with<R, L>(
    temp: &Path,
    target: &Path,
    permissions: Option<&fs::Permissions>,
    rename: R,
    link: L,
) -> io::Result<Publication>
where
    R: FnOnce(&Path, &Path) -> io::Result<()>,
    L: FnOnce(&Path, &Path) -> io::Result<()>,
{
    match rename(temp, target) {
        Ok(()) => return Ok(Publication::ATOMIC),
        Err(e) if publication_fallback_allowed(&e) => {}
        Err(e) => return Err(e),
    }
    match link(temp, target) {
        Ok(()) => return Ok(Publication::ATOMIC),
        Err(e) if publication_fallback_allowed(&e) => {}
        Err(e) => return Err(e),
    }
    exclusive_create_with(temp, target, permissions, |input, output| {
        copy_exclusive_contents_with(input, output, permissions, |file, mode| {
            file.set_permissions(mode)
        })
    })
}

/// New byte writes keep default creation permissions. Copies attempt to retain
/// the source mode, but unsupported chmod must not prevent publishing bytes.
#[cfg(unix)]
fn copy_exclusive_contents_with<P>(
    input: &mut File,
    output: &mut File,
    permissions: Option<&fs::Permissions>,
    set_permissions: P,
) -> io::Result<bool>
where
    P: FnOnce(&File, fs::Permissions) -> io::Result<()>,
{
    io::copy(input, output)?;
    let permissions_preserved =
        permissions.is_none_or(|mode| set_permissions(output, mode.clone()).is_ok());
    output.sync_all()?;
    Ok(permissions_preserved)
}

#[cfg(unix)]
fn exclusive_create_with<F>(
    source: &Path,
    target: &Path,
    permissions: Option<&fs::Permissions>,
    writer: F,
) -> io::Result<Publication>
where
    F: FnOnce(&mut File, &mut File) -> io::Result<bool>,
{
    let mut input = File::open(source)?;
    // No precheck. Existing files, including dangling symlinks, fail here.
    let mut output = open_new_file(target, permissions)?;
    let owned = output.metadata()?;
    match writer(&mut input, &mut output) {
        Ok(permissions_preserved) => Ok(Publication {
            atomic: false,
            permissions_preserved,
        }),
        Err(error) => {
            // Keep the descriptor open so the owned inode cannot be recycled.
            if let Err(cleanup) = remove_owned_file(target, &owned) {
                return Err(io::Error::new(
                    error.kind(),
                    format!("{error}; exclusive-create cleanup incomplete: {cleanup}"),
                ));
            }
            Err(error)
        }
    }
}

#[cfg(unix)]
fn remove_owned_file(path: &Path, owned: &fs::Metadata) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
        Ok(current) if current.dev() == owned.dev() && current.ino() == owned.ino() => {
            fs::remove_file(path)
        }
        Ok(_) => Err(io::Error::other(
            "target was replaced; refusing to remove another file",
        )),
    }
}

#[cfg(windows)]
pub(crate) fn rename_windows(source: &Path, target: &Path, replace: bool) -> io::Result<()> {
    let source_wide = wide_path(source);
    let target_wide = wide_path(target);
    let flags = MOVEFILE_WRITE_THROUGH
        | if replace {
            MOVEFILE_REPLACE_EXISTING
        } else {
            0
        };
    let ok = unsafe { MoveFileExW(source_wide.as_ptr(), target_wide.as_ptr(), flags) };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn wide_path(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

pub(crate) fn notify(path: &Path) {
    if let Some(parent) = path.parent() {
        crate::indexer::notify_content_directory_changed(parent);
    }
}

fn unique_temp_path(parent: &Path, target: &Path) -> PathBuf {
    let file_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    parent.join(format!(".{file_name}.{}.{}.tmp", process::id(), nanos))
}

struct TempCleanup(PathBuf);
impl Drop for TempCleanup {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_replace_is_enforced_at_publication() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        fs::write(&target, b"concurrent writer").unwrap();
        let err = write_bytes(&target, b"replacement", false).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&target).unwrap(), b"concurrent writer");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn replacing_existing_bytes_survives_permission_copy_failure() {
        for kind in [io::ErrorKind::PermissionDenied, io::ErrorKind::Unsupported] {
            let dir = tempfile::tempdir().unwrap();
            let target = dir.path().join("target");
            fs::write(&target, b"old").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
            }
            let publication = write_bytes_with_permissions(&target, b"new", true, |_, _| {
                Err(io::Error::new(kind, "injected chmod failure"))
            })
            .unwrap();
            assert!(publication.atomic);
            assert!(!publication.permissions_preserved);
            assert_eq!(fs::read(&target).unwrap(), b"new");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    fs::metadata(&target).unwrap().permissions().mode() & 0o077,
                    0
                );
            }
            assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
        }
    }

    #[test]
    fn no_replace_creates_complete_file_and_replace_updates_it() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        write_bytes(&target, b"first", false).unwrap();
        write_bytes(&target, b"second", true).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"second");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}

#[cfg(all(test, unix))]
mod unix_publication_tests {
    use super::*;

    #[test]
    fn unsupported_native_and_eperm_links_fall_back_to_exclusive_create() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("temp");
        let target = dir.path().join("target");
        fs::write(&source, b"complete bytes").unwrap();
        let publication = publish_no_replace_with(
            &source,
            &target,
            None,
            |_, _| Err(io::Error::from_raw_os_error(libc::ENOSYS)),
            |_, _| Err(io::Error::from_raw_os_error(libc::EPERM)),
        )
        .unwrap();
        assert!(!publication.atomic);
        assert!(publication.permissions_preserved);
        assert_eq!(fs::read(&target).unwrap(), b"complete bytes");
        let mut response = serde_json::json!({});
        publication.attach(&mut response);
        assert_eq!(response["publication_atomic"], false);
    }

    #[test]
    fn exclusive_create_retains_bytes_when_chmod_is_denied_or_unsupported() {
        for errno in [libc::EPERM, libc::ENOTSUP] {
            let dir = tempfile::tempdir().unwrap();
            let source = dir.path().join("source");
            let target = dir.path().join("target");
            use std::os::unix::fs::PermissionsExt;
            let permissions = fs::Permissions::from_mode(0o600);
            open_new_file(&source, Some(&permissions))
                .unwrap()
                .write_all(b"complete bytes")
                .unwrap();
            let publication =
                exclusive_create_with(&source, &target, Some(&permissions), |input, output| {
                    assert_eq!(output.metadata()?.permissions().mode() & 0o077, 0);
                    copy_exclusive_contents_with(input, output, Some(&permissions), |_, _| {
                        Err(io::Error::from_raw_os_error(errno))
                    })
                })
                .unwrap();
            assert!(!publication.atomic);
            assert!(!publication.permissions_preserved);
            assert_eq!(fs::read(&target).unwrap(), b"complete bytes");
            assert_eq!(
                fs::metadata(&target).unwrap().permissions().mode() & 0o077,
                0
            );
            assert_eq!(fs::read(&source).unwrap(), b"complete bytes");
            let mut response = serde_json::json!({});
            publication.attach(&mut response);
            assert_eq!(response["publication_atomic"], false);
            assert_eq!(response["permissions_preserved"], false);
        }
    }

    #[test]
    fn exclusive_byte_creation_does_not_attempt_chmod() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let target = dir.path().join("target");
        fs::write(&source, b"new bytes").unwrap();
        let publication = exclusive_create_with(&source, &target, None, |input, output| {
            copy_exclusive_contents_with(input, output, None, |_, _| {
                panic!("new byte creation must keep its default permissions")
            })
        })
        .unwrap();
        assert!(publication.permissions_preserved);
        assert_eq!(fs::read(&target).unwrap(), b"new bytes");
    }

    #[test]
    fn unsupported_native_can_publish_by_link_without_exclusive_create() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("temp");
        let target = dir.path().join("target");
        fs::write(&source, b"complete bytes").unwrap();
        let publication = publish_no_replace_with(
            &source,
            &target,
            None,
            |_, _| Err(io::Error::from_raw_os_error(libc::ENOSYS)),
            |a, b| fs::hard_link(a, b),
        )
        .unwrap();
        assert_eq!(publication, Publication::ATOMIC);
        assert_eq!(fs::read(&target).unwrap(), b"complete bytes");
    }

    #[test]
    fn existing_target_stops_the_ladder_at_native_rename() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("temp");
        let target = dir.path().join("target");
        fs::write(&source, b"replacement").unwrap();
        fs::write(&target, b"independent writer").unwrap();
        let error = publish_no_replace_with(
            &source,
            &target,
            None,
            |_, _| Err(io::Error::from_raw_os_error(libc::EEXIST)),
            |_, _| panic!("must not retry an existing destination"),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&target).unwrap(), b"independent writer");
    }

    #[test]
    fn exclusive_create_also_refuses_a_target_created_after_unsupported_errors() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("temp");
        let target = dir.path().join("target");
        fs::write(&source, b"replacement").unwrap();
        let error = publish_no_replace_with(
            &source,
            &target,
            None,
            |_, _| Err(io::Error::from_raw_os_error(libc::ENOSYS)),
            |_, path| {
                fs::write(path, b"concurrent writer").unwrap();
                Err(io::Error::from_raw_os_error(libc::EPERM))
            },
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&target).unwrap(), b"concurrent writer");
    }

    #[test]
    fn partial_exclusive_write_failure_removes_only_the_owned_file() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("temp");
        let target = dir.path().join("target");
        fs::write(&source, b"complete bytes").unwrap();
        exclusive_create_with(&source, &target, None, |_, output| {
            output.write_all(b"partial")?;
            Err(io::Error::other("injected copy failure"))
        })
        .unwrap_err();
        assert!(!target.exists());
        assert_eq!(fs::read(&source).unwrap(), b"complete bytes");
    }

    #[test]
    fn exclusive_cleanup_retains_a_replacement_created_by_another_writer() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("temp");
        let target = dir.path().join("target");
        fs::write(&source, b"complete bytes").unwrap();
        let error = exclusive_create_with(&source, &target, None, |_, output| {
            output.write_all(b"partial")?;
            fs::remove_file(&target)?;
            fs::write(&target, b"other writer")?;
            Err(io::Error::other("injected copy failure"))
        })
        .unwrap_err();
        assert!(error.to_string().contains("cleanup incomplete"));
        assert_eq!(fs::read(&target).unwrap(), b"other writer");
    }

    #[test]
    fn exclusive_cleanup_does_not_follow_a_replacement_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("temp");
        let target = dir.path().join("target");
        fs::write(&source, b"complete bytes").unwrap();
        exclusive_create_with(&source, &target, None, |_, _| {
            fs::remove_file(&target)?;
            std::os::unix::fs::symlink(&source, &target)?;
            Err(io::Error::other("injected sync failure"))
        })
        .unwrap_err();
        assert!(
            fs::symlink_metadata(&target)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(&source).unwrap(), b"complete bytes");
    }
}
