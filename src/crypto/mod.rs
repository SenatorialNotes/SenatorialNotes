//! Cryptographic containers for SenatorialNotes.
//!
//! - [`note`] — the per-note `.snote` container, format version 1. **Frozen:**
//!   its behaviour and on-disk bytes are unchanged since v0.2. The whole-vault
//!   engine reuses only its Argon2id / KDF-validation / big-endian-read helpers
//!   (`pub(crate)`), never its container logic.
//! - [`vault`] — the whole-vault encryption engine (Stage D): the `SNVLT`
//!   keyfile, the random Vault Master Key, HKDF-SHA256 domain subkeys, and the
//!   `SNENC` object container.

pub mod note;
pub mod vault;

// The `.snote` API stays reachable at `crate::crypto::…` (and, via `lib.rs`,
// `senatorial_notes::EncryptedSession`) so the module split is invisible to
// existing callers.
pub use note::{
    ARGON2_ITERATIONS, ARGON2_LANES, ARGON2_MEMORY_KIB, EncryptedHeader, EncryptedSession, decrypt,
    encrypt_new, encrypt_with_session, inspect_header,
};
