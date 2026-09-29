use std::fs::{self, OpenOptions};
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

pub fn write_bytes(path: &Path, bytes: &[u8], replace_existing: bool) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let temp_path = unique_temp_path(parent, path);

    let cleanup = TempCleanup(temp_path.clone());
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }

    if let Ok(metadata) = fs::metadata(path) {
        let _ = fs::set_permissions(&temp_path, metadata.permissions());
    }

    let result = match fs::rename(&temp_path, path) {
        Ok(()) => {
            cleanup.forget();
            Ok(())
        }
        Err(err) if replace_existing && path.exists() => {
            #[cfg(windows)]
            {
                let _ = err;
                replace_existing_file(&temp_path, path)?;
                cleanup.forget();
                Ok(())
            }
            #[cfg(not(windows))]
            {
                Err(err)
            }
        }
        Err(err) => Err(err),
    };
    if result.is_ok()
        && let Some(parent) = path.parent()
    {
        crate::indexer::notify_content_directory_changed(parent);
    }
    result
}

#[cfg(windows)]
fn replace_existing_file(source: &Path, target: &Path) -> io::Result<()> {
    let source_wide = wide_path(source);
    let target_wide = wide_path(target);
    let ok = unsafe {
        MoveFileExW(
            source_wide.as_ptr(),
            target_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
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

impl TempCleanup {
    fn forget(self) {
        std::mem::forget(self);
    }
}

impl Drop for TempCleanup {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
