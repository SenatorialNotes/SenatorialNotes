//! Storage and application services for SenatorialNotes.
//!
//! Markdown files are always the source of truth. UI code lives behind the
//! `gui` feature so storage tests can run without a graphical session.

pub mod config;
pub mod constants;
pub mod crypto;
pub mod error;
pub mod formatting;
pub mod markdown_spans;
pub mod model;
pub mod paths;
pub mod search;
pub mod sort;
pub mod storage;
pub mod ui_state;
pub mod vault;
pub mod vault_encrypted;
pub mod vault_export;
pub mod vault_lock;
pub mod vault_manifest;
pub mod vault_quarantine;
pub mod watcher;

pub use crypto::EncryptedSession;
pub use error::{Error, Result};
pub use model::{Note, NoteMetadata, NoteSummary};
pub use vault::{FileStamp, NotebookEntry, TrashEntry, Vault};
pub use vault_export::{ExportProgress, ExportReport, export_secure_vault_to_standard};
pub use vault_lock::{
    BlockedReason, DeadReason, LockAcquisition, LockOwner, LockStatus, VaultLock,
};
pub use vault_manifest::{Migration, VaultKind, VaultManifest};
pub use vault_quarantine::{ArtifactCategory, PendingQuarantine, QuarantineReport};
