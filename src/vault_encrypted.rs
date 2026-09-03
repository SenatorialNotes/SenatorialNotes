//! The encrypted-vault storage backend — v0.3 Stage D.
//!
//! An encrypted vault stores no plaintext note/notebook/attachment data on
//! disk. Every object is an `SNENC` ciphertext blob with an **opaque** random
//! filename under `<root>/.senatorial-notes/store/`, and the whole logical
//! structure (notebook tree, note↔blob mapping, trash, recovery, attachment
//! index, `created_at`) lives in a single encrypted `manifest` blob (keyed by
//! `k_names`). Old pre-v0.3 binaries never scan `.senatorial-notes/`, so they
//! cannot mistake a blob for a `Notes/*.md` file.
//!
//! This module implements the same logical operations `Vault` performs on an
//! ordinary vault; `Vault` dispatches to it when `manifest.kind == Encrypted`.
//! Filesystem-atomicity guarantees are the same as the plaintext path
//! (`storage::atomic::atomic_write`): a partial write never replaces valid
//! ciphertext.

use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config::VaultSessionState;
use crate::crypto::vault::{ObjectType, VaultKeys, open_keyfile, rewrap_keyfile};
use crate::crypto::{self, EncryptedSession};
use crate::error::io_error;
use crate::model::{Note, NoteSummary};
use crate::paths::{
    encrypted_note_filename, ensure_relative_note_path, note_filename, validate_notebook_name,
    validate_notebook_path,
};
use crate::storage::atomic::atomic_write;
use crate::vault::{
    DEFAULT_NOTEBOOK, FileStamp, NotebookEntry, TrashEntry, Vault, check_password_policy,
    create_private_directory, file_stamp, normalized_title,
};
use crate::{Error, Result};

pub(crate) const KEYFILE_NAME: &str = "vault.keys";
pub(crate) const STORE_DIR: &str = "store";
pub(crate) const MANIFEST_NAME: &str = "manifest";
const ORPHAN_DIR: &str = "orphans";
const MANIFEST_SCHEMA: u32 = 1;

/// The manifest object's UUID is a fixed constant (its `object_type` +
/// `k_names` subkey already isolate it from every note blob).
pub(crate) const MANIFEST_OBJECT_UUID: Uuid = Uuid::from_bytes([
    0x5e, 0x11, 0xa7, 0x00, 0x1a, 0x9e, 0x57, 0x00, 0x9b, 0xd0, 0xd4, 0xad, 0xe3, 0x90, 0x46, 0x01,
]);

// --- manifest -------------------------------------------------------------

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Manifest {
    schema: u32,
    created_at: DateTime<Utc>,
    /// Notebook relative paths (under a virtual `Notes/`), always including
    /// `Inbox`. Order is not significant.
    pub(crate) notebooks: Vec<String>,
    pub(crate) notes: Vec<NoteRecord>,
    #[serde(default)]
    pub(crate) trash: Vec<TrashRecord>,
    #[serde(default)]
    recovery: Vec<RecoveryRecord>,
    #[serde(default)]
    pub(crate) attachments: Vec<AttachmentRecord>,
    /// Per-vault UI/session state (last note, last view, recently-opened
    /// notes, editor scroll). For a Secure Vault this lives here - sealed -
    /// never in the plaintext app config. Additive; the `schema` number is
    /// unchanged (an older reader ignores it, a newer one defaults it).
    #[serde(default)]
    session: VaultSessionState,
}

impl Manifest {
    fn fresh() -> Self {
        Self {
            schema: MANIFEST_SCHEMA,
            created_at: Utc::now(),
            notebooks: vec![DEFAULT_NOTEBOOK.to_string()],
            notes: Vec::new(),
            trash: Vec::new(),
            recovery: Vec::new(),
            attachments: Vec::new(),
            session: VaultSessionState::default(),
        }
    }

    fn all_object_ids(&self) -> impl Iterator<Item = &str> {
        self.notes
            .iter()
            .map(|record| record.object_id.as_str())
            .chain(self.trash.iter().map(|record| record.object_id.as_str()))
            .chain(self.recovery.iter().map(|record| record.object_id.as_str()))
            .chain(
                self.attachments
                    .iter()
                    .map(|record| record.object_id.as_str()),
            )
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct NoteRecord {
    pub(crate) object_id: String,
    pub(crate) object_uuid: Uuid,
    pub(crate) notebook: String,
    pub(crate) filename: String,
    pub(crate) snote: bool,
}

impl NoteRecord {
    pub(crate) fn relative_path(&self) -> PathBuf {
        Path::new(&self.notebook).join(&self.filename)
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct TrashRecord {
    pub(crate) object_id: String,
    pub(crate) object_uuid: Uuid,
    pub(crate) note_id: Uuid,
    pub(crate) original_relative_path: String,
    pub(crate) trashed_at: DateTime<Utc>,
    pub(crate) snote: bool,
    title: String,
}

#[derive(Clone, Serialize, Deserialize)]
struct RecoveryRecord {
    object_id: String,
    object_uuid: Uuid,
    note_id: Uuid,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct AttachmentRecord {
    object_id: String,
    object_uuid: Uuid,
    note_id: Uuid,
    logical_name: String,
}

// --- store ---------------------------------------------------------------

struct Unlocked {
    keys: VaultKeys,
    manifest: Manifest,
}

/// The encrypted backend for one vault. Cloning shares the same locked/unlocked
/// state (`Rc<RefCell<…>>`) so every `Vault` clone in the UI sees one session.
#[derive(Clone)]
pub struct EncryptedStore {
    state_dir: PathBuf,
    store_dir: PathBuf,
    vault_id: Uuid,
    inner: Rc<RefCell<Option<Unlocked>>>,
}

impl std::fmt::Debug for EncryptedStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncryptedStore")
            .field("vault_id", &self.vault_id)
            .field("unlocked", &self.inner.borrow().is_some())
            .finish()
    }
}

impl EncryptedStore {
    fn new(state_dir: PathBuf, vault_id: Uuid) -> Self {
        let store_dir = state_dir.join(STORE_DIR);
        Self {
            state_dir,
            store_dir,
            vault_id,
            inner: Rc::new(RefCell::new(None)),
        }
    }

    pub(crate) fn keyfile_path(state_dir: &Path) -> PathBuf {
        state_dir.join(KEYFILE_NAME)
    }

    /// Creates a brand-new encrypted store from already-derived key material
    /// (`create_keyfile` output). Writes the keyfile and an empty sealed
    /// manifest; returns an **unlocked** store.
    pub(crate) fn create_from(
        state_dir: &Path,
        vault_id: Uuid,
        keyfile_bytes: &[u8],
        keys: VaultKeys,
    ) -> Result<Self> {
        let store = Self::new(state_dir.to_path_buf(), vault_id);
        create_private_directory(&store.store_dir)?;
        atomic_write(&Self::keyfile_path(state_dir), keyfile_bytes)?;
        let manifest = Manifest::fresh();
        store.write_manifest(&keys, &manifest)?;
        *store.inner.borrow_mut() = Some(Unlocked { keys, manifest });
        Ok(store)
    }

    #[cfg(test)]
    pub(crate) fn create(state_dir: &Path, vault_id: Uuid, password: &str) -> Result<Self> {
        check_password_policy(password)?;
        let (keyfile_bytes, keys) = crate::crypto::vault::create_keyfile(vault_id, password)?;
        Self::create_from(state_dir, vault_id, &keyfile_bytes, keys)
    }

    /// Opens an existing (locked) encrypted store. No key material yet.
    pub(crate) fn open(state_dir: &Path, vault_id: Uuid) -> Result<Self> {
        let store = Self::new(state_dir.to_path_buf(), vault_id);
        if !Self::keyfile_path(state_dir).is_file() {
            return Err(Error::InvalidEncryptedVault(
                "encrypted vault has no keyfile".into(),
            ));
        }
        create_private_directory(&store.store_dir)?;
        Ok(store)
    }

    pub(crate) fn keyfile_bytes(&self) -> Result<Vec<u8>> {
        let path = Self::keyfile_path(&self.state_dir);
        fs::read(&path).map_err(|source| io_error(&path, source))
    }

    pub fn is_unlocked(&self) -> bool {
        self.inner.borrow().is_some()
    }

    pub fn lock(&self) {
        // Drops `Unlocked` -> `VaultKeys` zeroizes; the cached manifest
        // plaintext is freed.
        *self.inner.borrow_mut() = None;
    }

    /// Completes an unlock with key material already derived (off the GTK main
    /// thread by the caller). Loads the manifest and runs the reconciliation
    /// pass. On any failure the store is left locked.
    pub(crate) fn finish_unlock(&self, keys: VaultKeys) -> Result<()> {
        if keys.vault_id() != self.vault_id {
            return Err(Error::InvalidEncryptedVault(
                "key material is for a different vault".into(),
            ));
        }
        let manifest = self.read_manifest(&keys)?;
        self.reconcile(&keys, &manifest)?;
        *self.inner.borrow_mut() = Some(Unlocked { keys, manifest });
        Ok(())
    }

    /// Convenience unlock (derives keys inline — used by tests and non-GUI
    /// callers; the GUI derives keys on a worker thread).
    pub(crate) fn unlock(&self, password: &str) -> Result<()> {
        let keys = open_keyfile(&self.keyfile_bytes()?, self.vault_id, password)?;
        self.finish_unlock(keys)
    }

    pub(crate) fn change_password(&self, old_password: &str, new_password: &str) -> Result<()> {
        rewrap_vault_keyfile(&self.state_dir, self.vault_id, old_password, new_password)
    }

    // --- manifest I/O ---------------------------------------------------

    fn manifest_path(&self) -> PathBuf {
        self.store_dir.join(MANIFEST_NAME)
    }

    fn read_manifest(&self, keys: &VaultKeys) -> Result<Manifest> {
        let path = self.manifest_path();
        let blob = fs::read(&path).map_err(|source| io_error(&path, source))?;
        let (plaintext, _) = keys.open(ObjectType::Manifest, MANIFEST_OBJECT_UUID, &blob)?;
        let manifest: Manifest = serde_json::from_slice(&plaintext)
            .map_err(|error| Error::InvalidEncryptedVault(format!("manifest: {error}")))?;
        if manifest.schema != MANIFEST_SCHEMA {
            return Err(Error::InvalidEncryptedVault(format!(
                "unsupported manifest schema {}",
                manifest.schema
            )));
        }
        Ok(manifest)
    }

    fn write_manifest(&self, keys: &VaultKeys, manifest: &Manifest) -> Result<()> {
        let json = serde_json::to_vec(manifest)
            .map_err(|error| Error::Encryption(format!("manifest serialize: {error}")))?;
        let blob = keys.seal(ObjectType::Manifest, MANIFEST_OBJECT_UUID, false, &json)?;
        atomic_write(&self.manifest_path(), &blob)
    }

    /// Move any blob not referenced by the manifest into `store/orphans/`
    /// (never deleted). A crash between "write blob" and "write manifest"
    /// leaves such a blob; note data is never lost.
    fn reconcile(&self, _keys: &VaultKeys, manifest: &Manifest) -> Result<()> {
        let referenced: std::collections::HashSet<&str> = manifest.all_object_ids().collect();
        let entries = match fs::read_dir(&self.store_dir) {
            Ok(entries) => entries,
            Err(_) => return Ok(()),
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_file() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if name == MANIFEST_NAME || referenced.contains(name.as_str()) {
                continue;
            }
            let orphan_dir = self.store_dir.join(ORPHAN_DIR);
            create_private_directory(&orphan_dir)?;
            let _ = fs::rename(entry.path(), orphan_dir.join(&name));
        }
        Ok(())
    }

    // --- blob helpers -------------------------------------------------

    fn blob_path(&self, object_id: &str) -> PathBuf {
        self.store_dir.join(object_id)
    }

    fn new_object_id() -> Result<String> {
        let mut bytes = [0_u8; 16];
        getrandom::fill(&mut bytes).map_err(|error| Error::Encryption(error.to_string()))?;
        Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
    }

    /// Write a content blob first (fsynced by `atomic_write`), then the caller
    /// writes the manifest.
    fn write_blob(
        &self,
        keys: &VaultKeys,
        object_id: &str,
        object_type: ObjectType,
        object_uuid: Uuid,
        inner_snote: bool,
        plaintext: &[u8],
    ) -> Result<FileStamp> {
        let blob = keys.seal(object_type, object_uuid, inner_snote, plaintext)?;
        let path = self.blob_path(object_id);
        atomic_write(&path, &blob)?;
        file_stamp(&path)
    }

    fn read_note_markdown(&self, keys: &VaultKeys, record: &NoteRecord) -> Result<Note> {
        let path = self.blob_path(&record.object_id);
        let blob = fs::read(&path).map_err(|source| io_error(&path, source))?;
        let (plaintext, _) = keys.open(ObjectType::Note, record.object_uuid, &blob)?;
        let text = String::from_utf8(plaintext.to_vec())
            .map_err(|_| Error::InvalidEncryptedVault("note is not valid UTF-8".into()))?;
        Note::parse(&text, record.relative_path())
    }

    /// The inner `.snote` v1 container bytes for a `.snote`-kind record.
    fn read_inner_snote(&self, keys: &VaultKeys, record: &NoteRecord) -> Result<Vec<u8>> {
        let path = self.blob_path(&record.object_id);
        let blob = fs::read(&path).map_err(|source| io_error(&path, source))?;
        let (plaintext, inner) = keys.open(ObjectType::InnerSnote, record.object_uuid, &blob)?;
        if !inner {
            return Err(Error::InvalidEncryptedVault(
                "object is not marked as an inner .snote".into(),
            ));
        }
        Ok(plaintext.to_vec())
    }

    // --- helpers over `Unlocked` -----------------------------------------

    fn with_locked<T>(&self, f: impl FnOnce(&Unlocked) -> Result<T>) -> Result<T> {
        let guard = self.inner.borrow();
        let unlocked = guard.as_ref().ok_or(Error::VaultLocked)?;
        f(unlocked)
    }

    /// Runs `f` against a mutable manifest copy; on `Ok`, seals and persists it
    /// (blob writes inside `f` must already have happened), then commits it to
    /// the in-memory session.
    fn with_manifest_mut<T>(
        &self,
        f: impl FnOnce(&VaultKeys, &mut Manifest) -> Result<T>,
    ) -> Result<T> {
        let mut guard = self.inner.borrow_mut();
        let unlocked = guard.as_mut().ok_or(Error::VaultLocked)?;
        let mut manifest = unlocked.manifest.clone();
        let value = f(&unlocked.keys, &mut manifest)?;
        self.write_manifest(&unlocked.keys, &manifest)?;
        unlocked.manifest = manifest;
        Ok(value)
    }

    fn find_note<'a>(manifest: &'a Manifest, relative: &Path) -> Result<&'a NoteRecord> {
        let notebook = relative
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let filename = relative
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .ok_or_else(|| Error::InvalidPath(relative.display().to_string()))?;
        manifest
            .notes
            .iter()
            .find(|record| record.notebook == notebook && record.filename == filename)
            .ok_or_else(|| Error::NoteNotFound(relative.to_path_buf()))
    }

    fn find_note_index(manifest: &Manifest, relative: &Path) -> Result<usize> {
        let notebook = relative
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let filename = relative
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .ok_or_else(|| Error::InvalidPath(relative.display().to_string()))?;
        manifest
            .notes
            .iter()
            .position(|record| record.notebook == notebook && record.filename == filename)
            .ok_or_else(|| Error::NoteNotFound(relative.to_path_buf()))
    }

    /// The persisted per-vault session state (sealed inside the manifest).
    pub(crate) fn session_state(&self) -> Result<VaultSessionState> {
        self.with_locked(|unlocked| Ok(unlocked.manifest.session.clone()))
    }

    /// Replaces the persisted per-vault session state, re-sealing the manifest.
    pub(crate) fn set_session_state(&self, session: VaultSessionState) -> Result<()> {
        self.with_manifest_mut(|_keys, manifest| {
            manifest.session = session;
            Ok(())
        })
    }

    // ================================================================
    // The `Vault`-equivalent operations.
    // ================================================================

    /// Used by the unlock/lock UI (Stage D Layer 4) and integration tests.
    #[allow(dead_code)]
    pub(crate) fn vault_id(&self) -> Uuid {
        self.vault_id
    }

    pub(crate) fn watch_paths(&self) -> Vec<PathBuf> {
        vec![self.store_dir.clone()]
    }

    pub(crate) fn note_path(&self, relative: &Path) -> Result<PathBuf> {
        let relative = ensure_relative_note_path(relative)?;
        self.with_locked(|u| {
            let record = Self::find_note(&u.manifest, &relative)?;
            Ok(self.blob_path(&record.object_id))
        })
    }

    pub(crate) fn current_stamp(&self, relative: &Path) -> Result<FileStamp> {
        file_stamp(&self.note_path(relative)?)
    }

    pub(crate) fn list_notebooks(&self) -> Result<Vec<NotebookEntry>> {
        self.with_locked(|u| {
            let mut entries: Vec<NotebookEntry> = u
                .manifest
                .notebooks
                .iter()
                .map(|notebook| NotebookEntry {
                    relative_path: PathBuf::from(notebook),
                    direct_note_count: u
                        .manifest
                        .notes
                        .iter()
                        .filter(|record| &record.notebook == notebook)
                        .count(),
                })
                .collect();
            entries.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
            Ok(entries)
        })
    }

    pub(crate) fn create_notebook(&self, relative: &Path) -> Result<PathBuf> {
        let relative = validate_notebook_path(relative)?;
        let name = relative.to_string_lossy().to_string();
        self.with_manifest_mut(|_keys, manifest| {
            if !manifest.notebooks.contains(&name) {
                manifest.notebooks.push(name.clone());
            }
            Ok(())
        })?;
        Ok(relative)
    }

    pub(crate) fn rename_notebook(&self, relative: &Path, new_name: &str) -> Result<PathBuf> {
        let relative = validate_notebook_path(relative)?;
        if Vault::is_reserved_notebook(&relative) {
            return Err(Error::ReservedNotebook {
                relative_path: relative,
            });
        }
        let sanitized = validate_notebook_name(new_name)?;
        let old = relative.to_string_lossy().to_string();
        let parent = relative.parent().unwrap_or_else(|| Path::new(""));
        let next_relative = parent.join(&sanitized);
        let new = next_relative.to_string_lossy().to_string();
        if new == old {
            return Ok(relative);
        }
        if Vault::is_reserved_notebook(&next_relative) {
            return Err(Error::ReservedNotebook {
                relative_path: next_relative,
            });
        }
        self.with_manifest_mut(|_keys, manifest| {
            if manifest.notebooks.iter().any(|n| n == &new) {
                return Err(Error::AlreadyExists(next_relative.clone()));
            }
            for notebook in &mut manifest.notebooks {
                if notebook == &old {
                    *notebook = new.clone();
                } else if let Some(rest) = notebook.strip_prefix(&format!("{old}/")) {
                    *notebook = format!("{new}/{rest}");
                }
            }
            for record in &mut manifest.notes {
                if record.notebook == old {
                    record.notebook = new.clone();
                } else if let Some(rest) = record.notebook.strip_prefix(&format!("{old}/")) {
                    record.notebook = format!("{new}/{rest}");
                }
            }
            Ok(())
        })?;
        Ok(next_relative)
    }

    pub(crate) fn delete_notebook(&self, relative: &Path) -> Result<()> {
        let relative = validate_notebook_path(relative)?;
        if Vault::is_reserved_notebook(&relative) {
            return Err(Error::ReservedNotebook {
                relative_path: relative,
            });
        }
        let target = relative.to_string_lossy().to_string();
        self.with_manifest_mut(|_keys, manifest| {
            let prefix = format!("{target}/");
            let note_count = manifest
                .notes
                .iter()
                .filter(|record| record.notebook == target || record.notebook.starts_with(&prefix))
                .count();
            if note_count > 0 {
                return Err(Error::NotebookNotEmpty {
                    relative_path: relative.clone(),
                    note_count,
                });
            }
            manifest
                .notebooks
                .retain(|notebook| notebook != &target && !notebook.starts_with(&prefix));
            Ok(())
        })
    }

    pub(crate) fn create_note(&self, title: &str, notebook: &Path) -> Result<Note> {
        let notebook = validate_notebook_path(notebook)?;
        self.create_notebook(&notebook)?;
        let metadata = crate::model::NoteMetadata::new(title);
        let filename = note_filename(title, metadata.id);
        let relative_path = notebook.join(&filename);
        let note = Note {
            metadata,
            body: String::new(),
            relative_path: relative_path.clone(),
        };
        let object_id = Self::new_object_id()?;

        let mut guard = self.inner.borrow_mut();
        let unlocked = guard.as_mut().ok_or(Error::VaultLocked)?;
        // 1. blob first.
        self.write_blob(
            &unlocked.keys,
            &object_id,
            ObjectType::Note,
            note.metadata.id,
            false,
            note.to_markdown()?.as_bytes(),
        )?;
        // 2. manifest.
        let mut manifest = unlocked.manifest.clone();
        manifest.notes.push(NoteRecord {
            object_id,
            object_uuid: note.metadata.id,
            notebook: notebook.to_string_lossy().to_string(),
            filename,
            snote: false,
        });
        self.write_manifest(&unlocked.keys, &manifest)?;
        unlocked.manifest = manifest;
        Ok(note)
    }

    pub(crate) fn load_note(&self, relative: &Path) -> Result<(Note, FileStamp)> {
        let relative = ensure_relative_note_path(relative)?;
        if relative.extension().and_then(|v| v.to_str()) != Some("md") {
            return Err(Error::WrongNoteType);
        }
        self.with_locked(|u| {
            let record = Self::find_note(&u.manifest, &relative)?;
            if record.snote {
                return Err(Error::WrongNoteType);
            }
            let note = self.read_note_markdown(&u.keys, record)?;
            let stamp = file_stamp(&self.blob_path(&record.object_id))?;
            Ok((note, stamp))
        })
    }

    pub(crate) fn load_encrypted_note(
        &self,
        relative: &Path,
        password: &str,
    ) -> Result<(Note, FileStamp, EncryptedSession)> {
        let relative = ensure_relative_note_path(relative)?;
        if relative.extension().and_then(|v| v.to_str()) != Some("snote") {
            return Err(Error::WrongNoteType);
        }
        self.with_locked(|u| {
            let record = Self::find_note(&u.manifest, &relative)?;
            if !record.snote {
                return Err(Error::WrongNoteType);
            }
            let inner = self.read_inner_snote(&u.keys, record)?;
            let (note, session) = crypto::decrypt(&inner, password, relative.clone())?;
            let stamp = file_stamp(&self.blob_path(&record.object_id))?;
            Ok((note, stamp, session))
        })
    }

    pub(crate) fn save_note(
        &self,
        note: &mut Note,
        expected: Option<&FileStamp>,
    ) -> Result<FileStamp> {
        if note.relative_path.extension().and_then(|v| v.to_str()) != Some("md") {
            return Err(Error::WrongNoteType);
        }
        let relative = ensure_relative_note_path(&note.relative_path)?;
        note.metadata.updated_at = Utc::now();
        let markdown = note.to_markdown()?;
        let mut guard = self.inner.borrow_mut();
        let unlocked = guard.as_mut().ok_or(Error::VaultLocked)?;
        let record = Self::find_note(&unlocked.manifest, &relative)?.clone();
        verify_blob_stamp(&self.blob_path(&record.object_id), expected)?;
        let stamp = self.write_blob(
            &unlocked.keys,
            &record.object_id,
            ObjectType::Note,
            record.object_uuid,
            false,
            markdown.as_bytes(),
        )?;
        // Drop any recovery blob for this note.
        self.remove_recovery_locked(unlocked, note.metadata.id)?;
        Ok(stamp)
    }

    pub(crate) fn commit_title(
        &self,
        note: &mut Note,
        expected: Option<&FileStamp>,
        title: &str,
    ) -> Result<FileStamp> {
        if note.relative_path.extension().and_then(|v| v.to_str()) != Some("md") {
            return Err(Error::WrongNoteType);
        }
        let relative = ensure_relative_note_path(&note.relative_path)?;
        let normalized = normalized_title(title);
        let new_filename = note_filename(&normalized, note.metadata.id);
        note.metadata.title = normalized;
        note.metadata.updated_at = Utc::now();
        let markdown = note.to_markdown()?;

        let mut guard = self.inner.borrow_mut();
        let unlocked = guard.as_mut().ok_or(Error::VaultLocked)?;
        let index = Self::find_note_index(&unlocked.manifest, &relative)?;
        let object_id = unlocked.manifest.notes[index].object_id.clone();
        let object_uuid = unlocked.manifest.notes[index].object_uuid;
        verify_blob_stamp(&self.blob_path(&object_id), expected)?;
        let stamp = self.write_blob(
            &unlocked.keys,
            &object_id,
            ObjectType::Note,
            object_uuid,
            false,
            markdown.as_bytes(),
        )?;
        if unlocked.manifest.notes[index].filename != new_filename {
            let mut manifest = unlocked.manifest.clone();
            let notebook = manifest.notes[index].notebook.clone();
            manifest.notes[index].filename = new_filename.clone();
            self.write_manifest(&unlocked.keys, &manifest)?;
            unlocked.manifest = manifest;
            note.relative_path = Path::new(&notebook).join(&new_filename);
        }
        self.remove_recovery_locked(unlocked, note.metadata.id)?;
        Ok(stamp)
    }

    pub(crate) fn save_encrypted_note(
        &self,
        note: &mut Note,
        session: &EncryptedSession,
        expected: Option<&FileStamp>,
    ) -> Result<FileStamp> {
        if note.relative_path.extension().and_then(|v| v.to_str()) != Some("snote") {
            return Err(Error::WrongNoteType);
        }
        let relative = ensure_relative_note_path(&note.relative_path)?;
        note.metadata.updated_at = Utc::now();
        let inner = crypto::encrypt_with_session(note, session)?;
        let mut guard = self.inner.borrow_mut();
        let unlocked = guard.as_mut().ok_or(Error::VaultLocked)?;
        let record = Self::find_note(&unlocked.manifest, &relative)?.clone();
        verify_blob_stamp(&self.blob_path(&record.object_id), expected)?;
        self.write_blob(
            &unlocked.keys,
            &record.object_id,
            ObjectType::InnerSnote,
            record.object_uuid,
            true,
            &inner,
        )
    }

    pub(crate) fn encrypt_note(
        &self,
        note: &mut Note,
        expected: Option<&FileStamp>,
        password: &str,
    ) -> Result<(FileStamp, EncryptedSession)> {
        if note.relative_path.extension().and_then(|v| v.to_str()) != Some("md") {
            return Err(Error::WrongNoteType);
        }
        check_password_policy(password)?;
        let relative = ensure_relative_note_path(&note.relative_path)?;
        note.metadata.updated_at = Utc::now();
        let (inner, session) = crypto::encrypt_new(note, password)?;
        let new_filename = encrypted_note_filename(note.metadata.id);

        let mut guard = self.inner.borrow_mut();
        let unlocked = guard.as_mut().ok_or(Error::VaultLocked)?;
        let index = Self::find_note_index(&unlocked.manifest, &relative)?;
        let object_id = unlocked.manifest.notes[index].object_id.clone();
        let object_uuid = unlocked.manifest.notes[index].object_uuid;
        verify_blob_stamp(&self.blob_path(&object_id), expected)?;
        let stamp = self.write_blob(
            &unlocked.keys,
            &object_id,
            ObjectType::InnerSnote,
            object_uuid,
            true,
            &inner,
        )?;
        let mut manifest = unlocked.manifest.clone();
        let notebook = manifest.notes[index].notebook.clone();
        manifest.notes[index].snote = true;
        manifest.notes[index].filename = new_filename.clone();
        self.write_manifest(&unlocked.keys, &manifest)?;
        unlocked.manifest = manifest;
        note.relative_path = Path::new(&notebook).join(&new_filename);
        Ok((stamp, session))
    }

    pub(crate) fn change_encrypted_password(
        &self,
        relative: &Path,
        old_password: &str,
        new_password: &str,
    ) -> Result<(Note, FileStamp, EncryptedSession)> {
        let relative = ensure_relative_note_path(relative)?;
        if relative.extension().and_then(|v| v.to_str()) != Some("snote") {
            return Err(Error::WrongNoteType);
        }
        check_password_policy(new_password)?;
        self.with_locked(|u| {
            let record = Self::find_note(&u.manifest, &relative)?.clone();
            let inner = self.read_inner_snote(&u.keys, &record)?;
            let (note, _) = crypto::decrypt(&inner, old_password, relative.clone())?;
            let (new_inner, _) = crypto::encrypt_new(&note, new_password)?;
            self.write_blob(
                &u.keys,
                &record.object_id,
                ObjectType::InnerSnote,
                record.object_uuid,
                true,
                &new_inner,
            )?;
            let written = self.read_inner_snote(&u.keys, &record)?;
            let (verified_note, verified_session) =
                crypto::decrypt(&written, new_password, relative.clone())
                    .map_err(|_| Error::Encryption("re-key verification failed".into()))?;
            if new_password != old_password
                && crypto::decrypt(&written, old_password, relative.clone()).is_ok()
            {
                return Err(Error::Encryption(
                    "re-key verification failed: the previous password still unlocks the note"
                        .into(),
                ));
            }
            let stamp = file_stamp(&self.blob_path(&record.object_id))?;
            Ok((verified_note, stamp, verified_session))
        })
    }

    pub(crate) fn remove_encryption(
        &self,
        relative: &Path,
        password: &str,
    ) -> Result<(Note, FileStamp)> {
        let relative = ensure_relative_note_path(relative)?;
        if relative.extension().and_then(|v| v.to_str()) != Some("snote") {
            return Err(Error::WrongNoteType);
        }
        let mut guard = self.inner.borrow_mut();
        let unlocked = guard.as_mut().ok_or(Error::VaultLocked)?;
        let index = Self::find_note_index(&unlocked.manifest, &relative)?;
        let record = unlocked.manifest.notes[index].clone();
        let inner = self.read_inner_snote(&unlocked.keys, &record)?;
        let (mut note, _) = crypto::decrypt(&inner, password, relative.clone())?;
        note.metadata.updated_at = Utc::now();
        let new_filename = note_filename(&note.metadata.title, note.metadata.id);
        let stamp = self.write_blob(
            &unlocked.keys,
            &record.object_id,
            ObjectType::Note,
            record.object_uuid,
            false,
            note.to_markdown()?.as_bytes(),
        )?;
        let mut manifest = unlocked.manifest.clone();
        let notebook = manifest.notes[index].notebook.clone();
        manifest.notes[index].snote = false;
        manifest.notes[index].filename = new_filename.clone();
        self.write_manifest(&unlocked.keys, &manifest)?;
        unlocked.manifest = manifest;
        note.relative_path = Path::new(&notebook).join(&new_filename);
        Ok((note, stamp))
    }

    pub(crate) fn move_note(
        &self,
        relative: &Path,
        destination_notebook: &Path,
    ) -> Result<PathBuf> {
        let relative = ensure_relative_note_path(relative)?;
        let destination = validate_notebook_path(destination_notebook)?;
        let dest = destination.to_string_lossy().to_string();
        self.create_notebook(&destination)?;
        self.with_manifest_mut(|_keys, manifest| {
            let index = Self::find_note_index(manifest, &relative)?;
            let filename = manifest.notes[index].filename.clone();
            if manifest.notes[index].notebook == dest {
                return Ok(relative.clone());
            }
            if manifest
                .notes
                .iter()
                .any(|record| record.notebook == dest && record.filename == filename)
            {
                return Err(Error::AlreadyExists(destination.join(&filename)));
            }
            // The blob is NOT re-encrypted - the path is not bound in the AAD.
            manifest.notes[index].notebook = dest.clone();
            Ok(destination.join(&filename))
        })
    }

    pub(crate) fn write_recovery(&self, note: &Note) -> Result<PathBuf> {
        if note.relative_path.extension().and_then(|v| v.to_str()) != Some("md") {
            return Err(Error::WrongNoteType);
        }
        let markdown = note.to_markdown()?;
        let object_id = Self::new_object_id()?;
        let recovery_uuid = Uuid::new_v4();
        let mut guard = self.inner.borrow_mut();
        let unlocked = guard.as_mut().ok_or(Error::VaultLocked)?;
        self.write_blob(
            &unlocked.keys,
            &object_id,
            ObjectType::Note,
            recovery_uuid,
            false,
            markdown.as_bytes(),
        )?;
        let mut manifest = unlocked.manifest.clone();
        manifest
            .recovery
            .retain(|record| record.note_id != note.metadata.id);
        manifest.recovery.push(RecoveryRecord {
            object_id: object_id.clone(),
            object_uuid: recovery_uuid,
            note_id: note.metadata.id,
        });
        self.write_manifest(&unlocked.keys, &manifest)?;
        unlocked.manifest = manifest;
        Ok(self.blob_path(&object_id))
    }

    fn remove_recovery_locked(&self, unlocked: &mut Unlocked, note_id: Uuid) -> Result<()> {
        if !unlocked
            .manifest
            .recovery
            .iter()
            .any(|r| r.note_id == note_id)
        {
            return Ok(());
        }
        let mut manifest = unlocked.manifest.clone();
        let removed: Vec<String> = manifest
            .recovery
            .iter()
            .filter(|r| r.note_id == note_id)
            .map(|r| r.object_id.clone())
            .collect();
        manifest.recovery.retain(|r| r.note_id != note_id);
        self.write_manifest(&unlocked.keys, &manifest)?;
        unlocked.manifest = manifest;
        for object_id in removed {
            let _ = fs::remove_file(self.blob_path(&object_id));
        }
        Ok(())
    }

    pub(crate) fn move_to_trash(&self, relative: &Path) -> Result<TrashEntry> {
        let relative = ensure_relative_note_path(relative)?;
        let mut guard = self.inner.borrow_mut();
        let unlocked = guard.as_mut().ok_or(Error::VaultLocked)?;
        let index = Self::find_note_index(&unlocked.manifest, &relative)?;
        let record = unlocked.manifest.notes[index].clone();
        let (note_id, title) = if record.snote {
            let inner = self.read_inner_snote(&unlocked.keys, &record)?;
            let id = crypto::inspect_header(&inner)?.note_id;
            (
                id,
                format!("Locked Note · {}", crate::model::locked_note_suffix(id)),
            )
        } else {
            let note = self.read_note_markdown(&unlocked.keys, &record)?;
            (note.metadata.id, note.metadata.title)
        };
        let trashed_at = Utc::now();
        let mut manifest = unlocked.manifest.clone();
        manifest.notes.remove(index);
        manifest.trash.push(TrashRecord {
            object_id: record.object_id.clone(),
            object_uuid: record.object_uuid,
            note_id,
            original_relative_path: relative.to_string_lossy().to_string(),
            trashed_at,
            snote: record.snote,
            title: title.clone(),
        });
        self.write_manifest(&unlocked.keys, &manifest)?;
        unlocked.manifest = manifest;
        Ok(TrashEntry {
            id: note_id,
            title,
            encrypted: record.snote,
            original_relative_path: relative,
            trashed_at,
        })
    }

    pub(crate) fn scan_trash(&self) -> Result<Vec<TrashEntry>> {
        self.with_locked(|u| {
            let mut entries: Vec<TrashEntry> = u
                .manifest
                .trash
                .iter()
                .map(|record| TrashEntry {
                    id: record.note_id,
                    title: record.title.clone(),
                    encrypted: record.snote,
                    original_relative_path: PathBuf::from(&record.original_relative_path),
                    trashed_at: record.trashed_at,
                })
                .collect();
            entries.sort_by_key(|entry| std::cmp::Reverse(entry.trashed_at));
            Ok(entries)
        })
    }

    pub(crate) fn restore_from_trash(&self, id: Uuid) -> Result<PathBuf> {
        self.with_manifest_mut(|_keys, manifest| {
            let trash_index = manifest
                .trash
                .iter()
                .position(|record| record.note_id == id)
                .ok_or_else(|| Error::NoteNotFound(PathBuf::from(id.to_string())))?;
            let record = manifest.trash[trash_index].clone();
            let original = PathBuf::from(&record.original_relative_path);
            let mut notebook = original
                .parent()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|| DEFAULT_NOTEBOOK.to_string());
            let mut filename = original
                .file_name()
                .map(|f| f.to_string_lossy().to_string())
                .unwrap_or_default();
            let collides = |manifest: &Manifest, nb: &str, fname: &str| {
                manifest
                    .notes
                    .iter()
                    .any(|r| r.notebook == nb && r.filename == fname)
            };
            if !manifest.notebooks.contains(&notebook) || collides(manifest, &notebook, &filename) {
                notebook = DEFAULT_NOTEBOOK.to_string();
                filename = if record.snote {
                    encrypted_note_filename(id)
                } else {
                    filename
                };
                if collides(manifest, &notebook, &filename) {
                    return Err(Error::AlreadyExists(Path::new(&notebook).join(&filename)));
                }
            }
            if !manifest.notebooks.contains(&notebook) {
                manifest.notebooks.push(notebook.clone());
            }
            manifest.trash.remove(trash_index);
            manifest.notes.push(NoteRecord {
                object_id: record.object_id,
                object_uuid: record.object_uuid,
                notebook: notebook.clone(),
                filename: filename.clone(),
                snote: record.snote,
            });
            Ok(Path::new(&notebook).join(&filename))
        })
    }

    pub(crate) fn permanently_delete(&self, id: Uuid) -> Result<()> {
        let object_id = self.with_manifest_mut(|_keys, manifest| {
            let index = manifest
                .trash
                .iter()
                .position(|record| record.note_id == id)
                .ok_or_else(|| Error::NoteNotFound(PathBuf::from(id.to_string())))?;
            Ok(manifest.trash.remove(index).object_id)
        })?;
        let _ = fs::remove_file(self.blob_path(&object_id));
        Ok(())
    }

    pub(crate) fn empty_trash(&self) -> Result<usize> {
        let ids: Vec<Uuid> = self.with_locked(|u| {
            Ok(u.manifest
                .trash
                .iter()
                .map(|record| record.note_id)
                .collect())
        })?;
        let count = ids.len();
        for id in ids {
            self.permanently_delete(id)?;
        }
        Ok(count)
    }

    pub(crate) fn scan_notes(&self) -> Result<Vec<NoteSummary>> {
        let mut summaries = self.with_locked(|u| {
            let mut summaries = Vec::with_capacity(u.manifest.notes.len());
            for record in &u.manifest.notes {
                if record.snote {
                    summaries.push(NoteSummary::locked(
                        record.object_uuid,
                        record.relative_path(),
                    ));
                } else {
                    let note = self.read_note_markdown(&u.keys, record)?;
                    summaries.push(NoteSummary::from(&note));
                }
            }
            Ok(summaries)
        })?;
        crate::sort::sort_notes(&mut summaries, None);
        Ok(summaries)
    }

    /// Writes an attachment for `note_id` and records it in the manifest.
    /// Attachments have no plaintext-vault UI yet; this is the encrypted
    /// `k_attachments` path, covered by the Stage D attachment tests.
    #[allow(dead_code)]
    pub(crate) fn write_attachment(
        &self,
        note_id: Uuid,
        logical_name: &str,
        bytes: &[u8],
    ) -> Result<()> {
        let object_id = Self::new_object_id()?;
        let attachment_uuid = Uuid::new_v4();
        let mut guard = self.inner.borrow_mut();
        let unlocked = guard.as_mut().ok_or(Error::VaultLocked)?;
        self.write_blob(
            &unlocked.keys,
            &object_id,
            ObjectType::Attachment,
            attachment_uuid,
            false,
            bytes,
        )?;
        let mut manifest = unlocked.manifest.clone();
        manifest.attachments.push(AttachmentRecord {
            object_id,
            object_uuid: attachment_uuid,
            note_id,
            logical_name: logical_name.to_string(),
        });
        self.write_manifest(&unlocked.keys, &manifest)?;
        unlocked.manifest = manifest;
        Ok(())
    }

    #[allow(dead_code)]
    pub(crate) fn read_attachment(&self, note_id: Uuid, logical_name: &str) -> Result<Vec<u8>> {
        self.with_locked(|u| {
            let record = u
                .manifest
                .attachments
                .iter()
                .find(|record| record.note_id == note_id && record.logical_name == logical_name)
                .ok_or_else(|| Error::NoteNotFound(PathBuf::from(logical_name)))?;
            let path = self.blob_path(&record.object_id);
            let blob = fs::read(&path).map_err(|source| io_error(&path, source))?;
            let (plaintext, _) = u
                .keys
                .open(ObjectType::Attachment, record.object_uuid, &blob)?;
            Ok(plaintext.to_vec())
        })
    }
}

/// Re-wraps the vault master key in `<state_dir>/vault.keys` under a new
/// password (fresh salt + wrap nonce), then verify-after-write. **No blob is
/// re-encrypted.** Takes only `Send` inputs so a GUI can run the Argon2id work
/// on a worker thread.
pub(crate) fn rewrap_vault_keyfile(
    state_dir: &Path,
    vault_id: Uuid,
    old_password: &str,
    new_password: &str,
) -> Result<()> {
    check_password_policy(new_password)?;
    let keyfile_path = EncryptedStore::keyfile_path(state_dir);
    let current = fs::read(&keyfile_path).map_err(|source| io_error(&keyfile_path, source))?;
    let rewrapped = rewrap_keyfile(&current, vault_id, old_password, new_password)?;
    atomic_write(&keyfile_path, &rewrapped)?;
    // Verify-after-write: the new password must open it, the old must not.
    let verified = fs::read(&keyfile_path).map_err(|source| io_error(&keyfile_path, source))?;
    open_keyfile(&verified, vault_id, new_password)
        .map_err(|_| Error::Encryption("vault password change verification failed".into()))?;
    if new_password != old_password && open_keyfile(&verified, vault_id, old_password).is_ok() {
        return Err(Error::Encryption(
            "vault password change verification failed: the old password still works".into(),
        ));
    }
    Ok(())
}

/// Reads and decrypts the sealed manifest at `<state_dir>/store/manifest` with
/// already-derived `keys`, without opening an [`EncryptedStore`] session. Used
/// by the worker-thread Secure \u{2192} Standard export, which owns its own
/// key material and never touches the live (non-`Send`) store.
pub(crate) fn read_sealed_manifest(state_dir: &Path, keys: &VaultKeys) -> Result<Manifest> {
    let path = state_dir.join(STORE_DIR).join(MANIFEST_NAME);
    let blob = fs::read(&path).map_err(|source| io_error(&path, source))?;
    let (plaintext, _) = keys.open(ObjectType::Manifest, MANIFEST_OBJECT_UUID, &blob)?;
    let manifest: Manifest = serde_json::from_slice(&plaintext)
        .map_err(|error| Error::InvalidEncryptedVault(format!("manifest: {error}")))?;
    if manifest.schema != MANIFEST_SCHEMA {
        return Err(Error::InvalidEncryptedVault(format!(
            "unsupported manifest schema {}",
            manifest.schema
        )));
    }
    Ok(manifest)
}

/// The absolute path of a store blob, for a caller (the export worker) that
/// holds a `state_dir` but no [`EncryptedStore`].
pub(crate) fn store_blob_path(state_dir: &Path, object_id: &str) -> PathBuf {
    state_dir.join(STORE_DIR).join(object_id)
}

fn verify_blob_stamp(path: &Path, expected: Option<&FileStamp>) -> Result<()> {
    if let Some(expected) = expected {
        let current = file_stamp(path)?;
        if &current != expected {
            return Err(Error::ExternalModification(path.to_path_buf()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    const PASSWORD: &str = "correct horse battery staple";

    fn new_store(dir: &Path) -> EncryptedStore {
        let vault_id = Uuid::new_v4();
        EncryptedStore::create(dir, vault_id, PASSWORD).expect("create encrypted store")
    }

    #[test]
    fn attachments_are_sealed_and_round_trip() {
        let tmp = tempdir().unwrap();
        let state_dir = tmp.path().join(".senatorial-notes");
        create_private_directory(&state_dir).unwrap();
        let store = new_store(&state_dir);
        let note = store.create_note("Host", Path::new("Inbox")).unwrap();

        let secret = b"ATTACHMENT-PLAINTEXT-PAYLOAD-7781";
        store
            .write_attachment(note.metadata.id, "diagram.png", secret)
            .expect("write attachment");

        let round_trip = store
            .read_attachment(note.metadata.id, "diagram.png")
            .expect("read attachment");
        assert_eq!(round_trip, secret);

        // Nothing in the store holds the plaintext.
        for entry in fs::read_dir(&store.store_dir).unwrap().flatten() {
            if entry.path().is_file() {
                let bytes = fs::read(entry.path()).unwrap();
                assert!(
                    !bytes.windows(secret.len()).any(|w| w == secret),
                    "attachment plaintext leaked into {}",
                    entry.path().display()
                );
            }
        }

        // A wrong logical name is a miss, not a panic.
        assert!(
            store
                .read_attachment(note.metadata.id, "other.png")
                .is_err()
        );
    }

    #[test]
    fn reconcile_moves_an_unreferenced_blob_to_orphans_without_deleting_it() {
        let tmp = tempdir().unwrap();
        let state_dir = tmp.path().join(".senatorial-notes");
        create_private_directory(&state_dir).unwrap();
        let store = new_store(&state_dir);
        store.create_note("Real", Path::new("Inbox")).unwrap();

        // A blob the manifest never learned about (crash between blob + manifest).
        let orphan = store.store_dir.join("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        fs::write(&orphan, b"orphaned ciphertext").unwrap();

        store.lock();
        store.unlock(PASSWORD).expect("unlock runs reconcile");

        assert!(
            !orphan.exists(),
            "the orphan blob is moved out of the store root"
        );
        let moved = store
            .store_dir
            .join(ORPHAN_DIR)
            .join("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        assert!(
            moved.is_file(),
            "it is preserved under orphans/, never deleted"
        );
        assert_eq!(fs::read(&moved).unwrap(), b"orphaned ciphertext");
    }
}
