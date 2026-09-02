//! The `.senatorial-notes/vault.toml` manifest: schema, versioning, and the
//! backwards-compatible v1 → v2 migration.
//!
//! v1 (`0.1`/`0.2`) manifests carry only `format_version`, `vault_id`, and
//! `created_at`. v2 adds an explicit [`VaultKind`]. A v1 manifest predates the
//! notion of an encrypted vault entirely, so it always migrates to
//! [`VaultKind::Ordinary`] and never to [`VaultKind::Encrypted`].
//!
//! This module is pure: it reads and writes exactly one file (`vault.toml`) and
//! never touches a note, notebook, attachment, or any other vault content.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::storage::atomic::atomic_write;
use crate::{Error, Result};

/// The highest `vault.toml` schema version this build understands.
/// `2` = an ordinary vault; `3` = an encrypted vault (Stage D).
pub const CURRENT_MANIFEST_VERSION: u32 = 3;

/// The version written for an ordinary vault (a fresh one, or a v1 migration).
pub const ORDINARY_MANIFEST_VERSION: u32 = 2;

/// The version of an encrypted vault.
pub const ENCRYPTED_MANIFEST_VERSION: u32 = 3;

const MANIFEST_FILE: &str = "vault.toml";

/// Absolute path of the manifest inside a vault's state directory.
pub fn manifest_path(state_dir: &Path) -> PathBuf {
    state_dir.join(MANIFEST_FILE)
}

/// What kind of storage a vault uses.
///
/// A v1 manifest has no `kind` field; it deserializes to [`VaultKind::Ordinary`]
/// via `#[serde(default)]`, and migration forces the value to `Ordinary`
/// regardless of anything a hand-edit may have added.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VaultKind {
    /// Plaintext Markdown notes and per-note `.snote` containers, exactly as in
    /// v0.1 / v0.2.
    #[default]
    Ordinary,
    /// Whole-vault encryption. The engine is not implemented in this build;
    /// [`Vault::create`](crate::Vault::create) refuses such a vault with
    /// [`Error::UnsupportedVaultKind`].
    Encrypted,
}

/// Parsed `.senatorial-notes/vault.toml`.
///
/// No `#[serde(deny_unknown_fields)]`: a manifest written by a newer
/// SenatorialNotes still parses far enough here to read its `format_version`
/// and be refused cleanly, and forward-compatible additions (e.g. an
/// `[encryption]` table) are ignored rather than rejected.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultManifest {
    pub format_version: u32,
    pub vault_id: Uuid,
    pub created_at: DateTime<Utc>,
    /// Absent in a v1 manifest (`#[serde(default)]` → `Ordinary`).
    #[serde(default)]
    pub kind: VaultKind,
    /// Set to the prior `format_version` when [`VaultManifest::load`] upgraded
    /// this file in place. Informational; omitted from a freshly created
    /// manifest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migrated_from: Option<u32>,
    /// Present only for an encrypted (`format_version = 3`) vault.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encryption: Option<EncryptionManifest>,
}

/// The `[encryption]` table of an encrypted vault's `vault.toml`. Non-secret:
/// it only points at the keyfile and records its format number.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncryptionManifest {
    pub format: u32,
    pub keyfile: String,
}

impl VaultManifest {
    /// A brand-new ordinary-vault manifest (`format_version = 2`).
    pub fn new_ordinary() -> Self {
        Self {
            format_version: ORDINARY_MANIFEST_VERSION,
            vault_id: Uuid::new_v4(),
            created_at: Utc::now(),
            kind: VaultKind::Ordinary,
            migrated_from: None,
            encryption: None,
        }
    }

    /// A brand-new encrypted-vault manifest (`format_version = 3`).
    pub fn new_encrypted(vault_id: Uuid, keyfile: &str) -> Self {
        Self {
            format_version: ENCRYPTED_MANIFEST_VERSION,
            vault_id,
            created_at: Utc::now(),
            kind: VaultKind::Encrypted,
            migrated_from: None,
            encryption: Some(EncryptionManifest {
                format: 1,
                keyfile: keyfile.to_string(),
            }),
        }
    }

    /// Reads `<state_dir>/vault.toml` and, if it is a v1 manifest, upgrades it
    /// in place to v2 `Ordinary`.
    ///
    /// - **v2** → returned as written ([`Migration::NotNeeded`]).
    /// - **v1** → `format_version` bumped to 2, `kind` forced to `Ordinary`,
    ///   `migrated_from` set to 1. The new manifest is written back with
    ///   [`atomic_write`]; if that write fails (e.g. a read-only vault) the
    ///   in-memory value is still returned ([`Migration::InMemoryOnly`]) so the
    ///   open path does not fail.
    /// - `format_version` greater than [`CURRENT_MANIFEST_VERSION`] →
    ///   [`Error::UnsupportedVaultVersion`], without touching the file.
    /// - unparseable / `format_version` 0 → [`Error::VaultManifestCorrupt`],
    ///   without touching the file.
    ///
    /// `vault_id` and `created_at` are always preserved verbatim. No file other
    /// than `vault.toml` is read or written.
    pub fn load(state_dir: &Path) -> Result<LoadOutcome> {
        let path = manifest_path(state_dir);
        let text = fs::read_to_string(&path).map_err(|source| {
            Error::VaultManifestCorrupt(format!("cannot read {}: {source}", path.display()))
        })?;

        // Read only the version first, so a manifest from a newer schema fails
        // as "unsupported version" rather than "corrupt".
        let probe: VersionProbe = toml::from_str(&text)
            .map_err(|error| Error::VaultManifestCorrupt(error.to_string()))?;
        if probe.format_version == 0 {
            return Err(Error::VaultManifestCorrupt(
                "format_version 0 is not a valid vault manifest".into(),
            ));
        }
        if probe.format_version > CURRENT_MANIFEST_VERSION {
            return Err(Error::UnsupportedVaultVersion {
                found: probe.format_version,
                supported: CURRENT_MANIFEST_VERSION,
            });
        }

        let mut manifest: VaultManifest = toml::from_str(&text)
            .map_err(|error| Error::VaultManifestCorrupt(error.to_string()))?;

        match manifest.format_version {
            2 | 3 => Ok(LoadOutcome {
                manifest,
                migration: Migration::NotNeeded,
            }),
            1 => {
                let from = 1;
                manifest.format_version = ORDINARY_MANIFEST_VERSION;
                // v1 predates VaultKind: never honour a `kind` a hand-edit added,
                // and never migrate to Encrypted.
                manifest.kind = VaultKind::Ordinary;
                manifest.migrated_from = Some(from);
                manifest.encryption = None;
                let migration = match write(state_dir, &manifest) {
                    Ok(()) => Migration::Persisted { from },
                    Err(error) => Migration::InMemoryOnly {
                        from,
                        reason: error.to_string(),
                    },
                };
                Ok(LoadOutcome {
                    manifest,
                    migration,
                })
            }
            other => Err(Error::VaultManifestCorrupt(format!(
                "unsupported manifest format_version {other}"
            ))),
        }
    }
}

/// Writes `manifest` to `<state_dir>/vault.toml` atomically.
pub fn write(state_dir: &Path, manifest: &VaultManifest) -> Result<()> {
    let path = manifest_path(state_dir);
    let text = toml::to_string_pretty(manifest)
        .map_err(|error| Error::Configuration(error.to_string()))?;
    atomic_write(&path, text.as_bytes())
}

/// The result of [`VaultManifest::load`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadOutcome {
    pub manifest: VaultManifest,
    pub migration: Migration,
}

/// What [`VaultManifest::load`] did to the on-disk manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Migration {
    /// The manifest was already at [`CURRENT_MANIFEST_VERSION`]; nothing was
    /// written.
    NotNeeded,
    /// A v`from` manifest was upgraded and the new manifest was written to disk.
    Persisted { from: u32 },
    /// A v`from` manifest was upgraded in memory but the new manifest could not
    /// be persisted (`reason`). The open path continues with the in-memory
    /// `Ordinary` manifest; the on-disk file is still the old version.
    InMemoryOnly { from: u32, reason: String },
}

impl Migration {
    /// A human-readable warning when the upgrade could not be persisted, for
    /// the UI to surface. `None` in every other case.
    pub fn warning(&self) -> Option<String> {
        match self {
            Migration::InMemoryOnly { from, reason } => Some(format!(
                "This vault's format could not be upgraded from version {from} (it opened read-only \
                 for now): {reason}"
            )),
            _ => None,
        }
    }
}

/// Deserialize target for reading `format_version` alone.
#[derive(Deserialize)]
struct VersionProbe {
    format_version: u32,
}
