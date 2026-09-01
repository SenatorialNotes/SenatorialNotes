use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("invalid note front matter: {0}")]
    InvalidFrontMatter(String),

    #[error("invalid path component: {0}")]
    InvalidPath(String),

    #[error("the file changed on disk after it was opened: {0}")]
    ExternalModification(PathBuf),

    #[error("a note already exists at {0}")]
    AlreadyExists(PathBuf),

    #[error("note not found: {0}")]
    NoteNotFound(PathBuf),

    #[error("configuration error: {0}")]
    Configuration(String),

    #[error("no platform configuration directory is available")]
    NoConfigDirectory,

    #[error("filesystem watcher error: {0}")]
    Watcher(String),

    #[error("invalid encrypted note: {0}")]
    InvalidEncryptedNote(String),

    #[error("the encrypted note could not be unlocked")]
    DecryptionFailed,

    #[error("encryption operation failed: {0}")]
    Encryption(String),

    #[error("password does not meet the minimum policy: {0}")]
    WeakPassword(String),

    #[error("the requested operation is not valid for this note type")]
    WrongNoteType,
}

pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn io_error(path: impl Into<PathBuf>, source: std::io::Error) -> Error {
    Error::Io {
        path: path.into(),
        source,
    }
}
