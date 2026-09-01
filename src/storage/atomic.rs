use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use uuid::Uuid;

use crate::Result;
use crate::error::io_error;

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

/// Replaces a file atomically using a temporary sibling.
///
/// The temporary file is flushed before the rename. On Linux the containing
/// directory is also synced after the rename, so a reported success has a
/// durable directory entry. Existing permissions are preserved.
pub fn atomic_write(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| crate::Error::InvalidPath(format!("{} has no parent", path.display())))?;
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| {
            crate::Error::InvalidPath(format!("{} has no valid filename", path.display()))
        })?;
    let temporary = parent.join(format!(".{file_name}.{}.tmp", Uuid::new_v4().simple()));

    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);

        let mut file = options
            .open(&temporary)
            .map_err(|source| io_error(&temporary, source))?;

        #[cfg(unix)]
        if let Ok(existing) = fs::metadata(path) {
            let mode = existing.permissions().mode();
            fs::set_permissions(&temporary, fs::Permissions::from_mode(mode))
                .map_err(|source| io_error(&temporary, source))?;
        }

        file.write_all(contents)
            .map_err(|source| io_error(&temporary, source))?;
        file.sync_all()
            .map_err(|source| io_error(&temporary, source))?;
        drop(file);

        fs::rename(&temporary, path).map_err(|source| io_error(path, source))?;

        #[cfg(unix)]
        {
            let directory = fs::File::open(parent).map_err(|source| io_error(parent, source))?;
            directory
                .sync_all()
                .map_err(|source| io_error(parent, source))?;
        }

        Ok(())
    })();

    if result.is_err() {
        let _ignored = fs::remove_file(&temporary);
    }
    result
}

/// Renames `from` to `to`, refusing atomically if `to` already exists instead
/// of silently overwriting it the way plain `fs::rename` would.
///
/// Uses the Linux `renameat2(RENAME_NOREPLACE)` syscall (this application
/// targets Linux only), which makes the existence check and the rename a
/// single atomic kernel operation - immune to a collision that appears
/// between a separate check and a separate rename. Falls back to a
/// check-then-rename on a kernel/filesystem that doesn't support the flag
/// (`ENOSYS`/`EINVAL`), matching the convention used elsewhere in this
/// module.
pub fn rename_no_replace(from: &Path, to: &Path) -> Result<()> {
    #[cfg(target_os = "linux")]
    if let Some(result) = renameat2_no_replace(from, to) {
        return result;
    }

    if to.exists() {
        return Err(crate::Error::AlreadyExists(to.to_path_buf()));
    }
    fs::rename(from, to).map_err(|source| io_error(to, source))
}

/// Returns `None` when `renameat2`/`RENAME_NOREPLACE` is unavailable on this
/// kernel/filesystem, so the caller can fall back to check-then-rename.
#[cfg(target_os = "linux")]
fn renameat2_no_replace(from: &Path, to: &Path) -> Option<Result<()>> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let from_c = CString::new(from.as_os_str().as_bytes()).ok()?;
    let to_c = CString::new(to.as_os_str().as_bytes()).ok()?;

    let outcome = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            from_c.as_ptr(),
            libc::AT_FDCWD,
            to_c.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if outcome == 0 {
        return Some(Ok(()));
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(code) if code == libc::EEXIST => {
            Some(Err(crate::Error::AlreadyExists(to.to_path_buf())))
        }
        Some(code) if code == libc::ENOSYS || code == libc::EINVAL => None,
        _ => Some(Err(io_error(to, error))),
    }
}
