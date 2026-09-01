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
