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
pub mod watcher;

pub use crypto::EncryptedSession;
pub use error::{Error, Result};
pub use model::{Note, NoteMetadata, NoteSummary};
pub use vault::{FileStamp, NotebookEntry, TrashEntry, Vault};
