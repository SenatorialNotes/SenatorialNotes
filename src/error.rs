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

    #[error(
        "notebook {relative_path} is not empty ({note_count} note(s) inside); move, archive, or delete its notes first"
    )]
    NotebookNotEmpty {
        relative_path: PathBuf,
        note_count: usize,
    },

    #[error(
        "notebook {relative_path} contains files SenatorialNotes does not manage; it will not delete them"
    )]
    NotebookHasUnmanagedContent { relative_path: PathBuf },

    #[error("{relative_path} is a reserved notebook and cannot be renamed or deleted")]
    ReservedNotebook { relative_path: PathBuf },

    #[error(
        "this vault was created by a newer version of SenatorialNotes (manifest format {found}; this build supports up to {supported})"
    )]
    UnsupportedVaultVersion { found: u32, supported: u32 },

    #[error("the vault manifest is missing or unreadable: {0}")]
    VaultManifestCorrupt(String),

    #[error("encrypted vaults are not supported by this build")]
    UnsupportedVaultKind,

    #[error("this vault is open read-only and cannot be modified")]
    VaultReadOnly,

    #[error("this encrypted vault is locked")]
    VaultLocked,

    #[error("invalid encrypted vault: {0}")]
    InvalidEncryptedVault(String),

    #[error(
        "{0} already contains a vault or plaintext notes; an encrypted vault can only be created \
         in an empty folder (converting an existing vault is not supported in this release)"
    )]
    EncryptedVaultTargetNotEmpty(PathBuf),
}

pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn io_error(path: impl Into<PathBuf>, source: std::io::Error) -> Error {
    Error::Io {
        path: path.into(),
        source,
    }
}
