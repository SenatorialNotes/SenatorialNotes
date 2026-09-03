use std::fs;
use std::hash::Hasher;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::constants::VAULT_STATE_DIR;
use crate::crypto::vault::VaultKeys;
use crate::crypto::{self, EncryptedSession};
use crate::error::io_error;
use crate::model::{Note, NoteSummary, locked_note_suffix};
use crate::paths::{
    encrypted_note_filename, ensure_relative_note_path, note_filename, validate_notebook_name,
    validate_notebook_path,
};
use crate::storage::atomic::{atomic_write, rename_no_replace};
use crate::vault_encrypted::{self, EncryptedStore};
use crate::vault_manifest::{self, Migration, VaultKind, VaultManifest};
use crate::vault_quarantine::{self, PendingQuarantine, QuarantineReport};
use crate::{Error, Result};

const NOTES_DIR: &str = "Notes";
const ATTACHMENTS_DIR: &str = "Attachments";
const TRASH_DIR: &str = "Trash";
pub(crate) const DEFAULT_NOTEBOOK: &str = "Inbox";

#[derive(Clone, Debug)]
enum Backend {
    /// Plaintext Markdown / `.snote` files under `Notes/` (v0.1–v0.2 layout).
    Plain,
    /// Whole-vault encryption (Stage D): opaque `SNENC` blobs + an encrypted
    /// manifest under `.senatorial-notes/store/`. Starts **locked**.
    Encrypted(EncryptedStore),
}

#[derive(Clone, Debug)]
pub struct Vault {
    root: PathBuf,
    manifest: VaultManifest,
    migration: Migration,
    /// `true` when this vault must not be modified during the session. Set when
    /// a v1 → v2 manifest migration could not be persisted (a read-only vault):
    /// every mutating operation then fails with [`Error::VaultReadOnly`] before
    /// touching the filesystem, so the on-disk manifest version and the note
    /// tree can never drift apart. Stage B's vault lock will reuse this flag for
    /// "Open read-only".
    read_only: bool,
    /// R18: plaintext storage artifacts an old / incompatible binary wrote into
    /// this Secure Vault's root, detected on open and **not** yet moved. While
    /// this is `Some`, `read_only` is forced `true` (no mutating operation may
    /// run) and the UI must resolve the conflict — the user explicitly choosing
    /// [`Vault::quarantine_plaintext`] or "open read-only" — before a normal
    /// writable session is allowed. Always `None` for a Standard Vault.
    pending_quarantine: Option<PendingQuarantine>,
    backend: Backend,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileStamp {
    modified: SystemTime,
    length: u64,
    fingerprint: u64,
}

impl FileStamp {
    /// Cheap validity check: compares only the modification time and length
    /// without reading or hashing the file. Used to confirm an in-memory
    /// document cache entry is still current before reusing it.
    pub fn metadata_matches(&self, path: &Path) -> bool {
        match fs::metadata(path) {
            Ok(metadata) => {
                metadata.len() == self.length
                    && metadata
                        .modified()
                        .is_ok_and(|modified| modified == self.modified)
            }
            Err(_) => false,
        }
    }
}

/// A notebook discovered under the vault's `Notes` directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotebookEntry {
    /// Path relative to the vault's `Notes` directory, e.g. `Work/Projects`.
    pub relative_path: PathBuf,
    /// Notes (`.md`/`.snote`) directly inside this notebook - does not count
    /// notes in nested child notebooks.
    pub direct_note_count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrashEntry {
    pub id: Uuid,
    pub title: String,
    pub encrypted: bool,
    pub original_relative_path: PathBuf,
    pub trashed_at: chrono::DateTime<Utc>,
}

#[derive(Debug, Serialize, Deserialize)]
struct TrashRecord {
    format_version: u32,
    note_id: Uuid,
    original_relative_path: PathBuf,
    trashed_at: chrono::DateTime<Utc>,
    encrypted: bool,
}

impl Vault {
    /// Opens the vault at `root`, creating a fresh one if `root` has no
    /// manifest yet.
    ///
    /// An existing manifest is validated and, if it is a v1 manifest, migrated
    /// in place to v2 `Ordinary` **before** any directory or note file is
    /// touched (see [`VaultManifest::load`]). A `format_version` newer than this
    /// build supports, a corrupt manifest, or a `kind = "encrypted"` vault are
    /// all refused here without modifying anything on disk.
    pub fn create(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        create_private_directory(root)?;
        let state_dir = root.join(VAULT_STATE_DIR);

        if vault_manifest::manifest_path(&state_dir).exists() {
            let outcome = VaultManifest::load(&state_dir)?;

            if outcome.manifest.kind == VaultKind::Encrypted {
                // An encrypted vault must be a v3 manifest with an [encryption]
                // table; a v2 `kind = "encrypted"` is a malformed hand-edit.
                if outcome.manifest.format_version != vault_manifest::ENCRYPTED_MANIFEST_VERSION {
                    return Err(Error::UnsupportedVaultKind);
                }
                // R18: look for plaintext an old binary may have written into
                // the vault root. Detection only — nothing is moved here. If
                // anything is found the vault opens forced-read-only and the UI
                // must resolve the conflict before a writable session.
                let pending_quarantine = vault_quarantine::detect(root);
                let store = EncryptedStore::open(&state_dir, outcome.manifest.vault_id)?;
                return Ok(Self {
                    root: root.to_path_buf(),
                    manifest: outcome.manifest,
                    migration: outcome.migration,
                    read_only: pending_quarantine.is_some(),
                    pending_quarantine,
                    backend: Backend::Encrypted(store),
                });
            }

            // A migration that could not be written back means the vault is
            // read-only: it opens, but no mutating operation may run this
            // session (see `read_only` and `ensure_writable`).
            let read_only = matches!(outcome.migration, Migration::InMemoryOnly { .. });
            let vault = Self {
                root: root.to_path_buf(),
                manifest: outcome.manifest,
                migration: outcome.migration,
                read_only,
                pending_quarantine: None,
                backend: Backend::Plain,
            };
            // Never create or repair the directory tree for a read-only vault -
            // that would be a partial, mixed on-disk change the session is not
            // allowed to make. A genuine read-only vault (a backup, a read-only
            // mount) already has its tree.
            if !read_only {
                vault.ensure_directories()?;
            }
            Ok(vault)
        } else {
            let vault = Self {
                root: root.to_path_buf(),
                manifest: VaultManifest::new_ordinary(),
                migration: Migration::NotNeeded,
                read_only: false,
                pending_quarantine: None,
                backend: Backend::Plain,
            };
            vault.ensure_directories()?;
            vault_manifest::write(&vault.state_dir(), &vault.manifest)?;
            Ok(vault)
        }
    }

    /// Creates a fresh **encrypted** vault protected by `password`. The returned
    /// vault is unlocked. (The Argon2id work happens in `create_keyfile`; a GUI
    /// caller should run it on a worker thread via
    /// [`crypto::vault::create_keyfile`](crate::crypto::vault::create_keyfile)
    /// and then [`Vault::finish_create_encrypted`].)
    pub fn create_encrypted(root: impl AsRef<Path>, password: &str) -> Result<Self> {
        let root = root.as_ref();
        ensure_encrypted_target_is_clean(root)?;
        check_password_policy(password)?;
        let vault_id = Uuid::new_v4();
        let (keyfile_bytes, keys) = crate::crypto::vault::create_keyfile(vault_id, password)?;
        Self::finish_create_encrypted(root, vault_id, &keyfile_bytes, keys)
    }

    /// Validates that `root` can host a *new* encrypted vault: it must not
    /// already be a vault of either kind and must not contain plaintext note
    /// storage (`Notes/` / `Trash/` / `Attachments/`, a `.senatorial-notes/`
    /// directory, or any `.md` / `.snote` file). Callers that build an
    /// encrypted vault outside `create_encrypted` (the GUI runs key derivation
    /// on a worker thread and calls [`Vault::finish_create_encrypted`]
    /// directly) can run this check up front.
    ///
    /// In-place conversion of an existing vault is intentionally not supported
    /// in this release; stamping an encrypted keyfile onto an existing
    /// ordinary vault would leave its notes in plaintext.
    pub fn check_encrypted_target(root: impl AsRef<Path>) -> Result<()> {
        ensure_encrypted_target_is_clean(root.as_ref())
    }

    /// Finishes creating an encrypted vault with key material derived elsewhere
    /// (off the GUI main thread).
    pub fn finish_create_encrypted(
        root: impl AsRef<Path>,
        vault_id: Uuid,
        keyfile_bytes: &[u8],
        keys: VaultKeys,
    ) -> Result<Self> {
        let root = root.as_ref();
        // Enforced here too, not only in `create_encrypted`: the GUI derives
        // the key material on a worker thread and calls this directly, so this
        // is the real guard against encrypting on top of an existing vault.
        ensure_encrypted_target_is_clean(root)?;
        create_private_directory(root)?;
        let state_dir = root.join(VAULT_STATE_DIR);
        create_private_directory(&state_dir)?;

        let store = EncryptedStore::create_from(&state_dir, vault_id, keyfile_bytes, keys)?;
        let manifest = VaultManifest::new_encrypted(vault_id, vault_encrypted::KEYFILE_NAME);
        vault_manifest::write(&state_dir, &manifest)?;

        Ok(Self {
            root: root.to_path_buf(),
            manifest,
            migration: Migration::NotNeeded,
            read_only: false,
            pending_quarantine: None,
            backend: Backend::Encrypted(store),
        })
    }

    /// Whether this vault uses whole-vault encryption.
    pub fn is_encrypted(&self) -> bool {
        matches!(self.backend, Backend::Encrypted(_))
    }

    /// Whether this is an encrypted vault that is not currently unlocked.
    pub fn is_locked(&self) -> bool {
        match &self.backend {
            Backend::Plain => false,
            Backend::Encrypted(store) => !store.is_unlocked(),
        }
    }

    /// The raw `vault.keys` bytes, for deriving key material on a worker thread.
    pub fn encrypted_keyfile(&self) -> Result<Vec<u8>> {
        match &self.backend {
            Backend::Encrypted(store) => store.keyfile_bytes(),
            Backend::Plain => Err(Error::InvalidEncryptedVault(
                "not an encrypted vault".into(),
            )),
        }
    }

    /// Completes an unlock with key material derived off the main thread.
    pub fn finish_unlock(&self, keys: VaultKeys) -> Result<()> {
        match &self.backend {
            Backend::Encrypted(store) => store.finish_unlock(keys),
            Backend::Plain => Err(Error::InvalidEncryptedVault(
                "not an encrypted vault".into(),
            )),
        }
    }

    /// Unlocks an encrypted vault (derives the KEK inline — tests / non-GUI).
    pub fn unlock(&self, password: &str) -> Result<()> {
        match &self.backend {
            Backend::Encrypted(store) => store.unlock(password),
            Backend::Plain => Err(Error::InvalidEncryptedVault(
                "not an encrypted vault".into(),
            )),
        }
    }

    /// Drops all in-memory key material and cached plaintext for an encrypted
    /// vault. A no-op for an ordinary vault.
    pub fn lock(&self) {
        if let Backend::Encrypted(store) = &self.backend {
            store.lock();
        }
    }

    /// Changes an encrypted vault's password by re-wrapping the VMK only (no
    /// object blob is re-encrypted).
    pub fn change_vault_password(&self, old_password: &str, new_password: &str) -> Result<()> {
        self.ensure_writable()?;
        match &self.backend {
            Backend::Encrypted(store) => store.change_password(old_password, new_password),
            Backend::Plain => Err(Error::InvalidEncryptedVault(
                "not an encrypted vault".into(),
            )),
        }
    }

    /// Re-wraps the vault master key in an encrypted vault's keyfile under a new
    /// password. An associated function taking only `Send` values so the GUI
    /// can run the Argon2id work on a worker thread; the in-memory unlocked
    /// session stays valid afterwards (the VMK does not change). `state_dir` is
    /// [`Vault::state_dir`] of an encrypted, unlocked vault.
    pub fn rewrap_encrypted_keyfile(
        state_dir: &Path,
        vault_id: Uuid,
        old_password: &str,
        new_password: &str,
    ) -> Result<()> {
        vault_encrypted::rewrap_vault_keyfile(state_dir, vault_id, old_password, new_password)
    }

    /// The persisted per-vault session state for a Secure Vault (sealed inside
    /// its manifest). `None` for an ordinary vault (its session state lives in
    /// the app config) or a locked one.
    pub fn encrypted_session_state(&self) -> Option<crate::config::VaultSessionState> {
        match &self.backend {
            Backend::Encrypted(store) => store.session_state().ok(),
            Backend::Plain => None,
        }
    }

    /// Persists per-vault session state into a Secure Vault's sealed manifest.
    /// A no-op for an ordinary vault (nothing plaintext is written).
    pub fn set_encrypted_session_state(
        &self,
        session: crate::config::VaultSessionState,
    ) -> Result<()> {
        match &self.backend {
            Backend::Encrypted(store) => store.set_session_state(session),
            Backend::Plain => Ok(()),
        }
    }

    /// The directories the filesystem watcher should snapshot for this vault.
    pub fn watch_paths(&self) -> Vec<PathBuf> {
        match &self.backend {
            Backend::Plain => vec![self.notes_dir(), self.trash_dir()],
            Backend::Encrypted(store) => store.watch_paths(),
        }
    }

    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        if !root.is_dir() {
            return Err(Error::InvalidPath(root.display().to_string()));
        }
        Self::create(root)
    }

    /// Creates the vault's standard directory tree if any part is missing.
    /// Existing directories (and their permissions) are left untouched.
    fn ensure_directories(&self) -> Result<()> {
        for directory in [
            self.notes_dir(),
            self.notes_dir().join(DEFAULT_NOTEBOOK),
            self.attachments_dir(),
            self.trash_dir(),
            self.state_dir(),
            self.history_dir(),
            self.recovery_dir(),
        ] {
            create_private_directory(&directory)?;
        }
        Ok(())
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// This vault's stable UUID (from `vault.toml`).
    pub fn vault_id(&self) -> Uuid {
        self.manifest.vault_id
    }

    /// Whether this vault is ordinary or encrypted. Always `Ordinary` in this
    /// build (an encrypted vault is refused at open time).
    pub fn kind(&self) -> VaultKind {
        self.manifest.kind
    }

    /// The parsed manifest.
    pub fn manifest(&self) -> &VaultManifest {
        &self.manifest
    }

    /// What opening this vault did to its manifest (a v1 → v2 upgrade, or
    /// nothing). [`Migration::warning`] yields a user-facing message when the
    /// upgrade could not be persisted.
    pub fn migration(&self) -> &Migration {
        &self.migration
    }

    /// Whether this vault is open read-only. `true` when a manifest migration
    /// could not be persisted, or (for a Secure Vault) while an unresolved R18
    /// plaintext conflict stands ([`Vault::pending_quarantine`]). Every mutating
    /// method returns [`Error::VaultReadOnly`] while this holds.
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// R18: plaintext storage artifacts an old / incompatible binary left in
    /// this Secure Vault's root, detected on open and not yet moved. `Some`
    /// means the vault is forced read-only until the user resolves the conflict
    /// (an explicit [`Vault::quarantine_plaintext`], or a deliberate read-only
    /// open). `None` for a clean vault or any Standard Vault.
    pub fn pending_quarantine(&self) -> Option<&PendingQuarantine> {
        self.pending_quarantine.as_ref()
    }

    /// Moves the detected R18 plaintext artifacts, unchanged, into
    /// `.senatorial-notes/quarantine/<timestamp>/` (same-filesystem rename;
    /// never deleted, parsed, merged, or overwritten). Only a UI caller acting
    /// on an explicit user choice calls this. On success the caller must
    /// re-open the vault to get a clean, writable session.
    pub fn quarantine_plaintext(&self) -> Result<QuarantineReport> {
        match &self.pending_quarantine {
            Some(pending) => pending.quarantine(),
            None => Err(Error::Quarantine(
                "there is nothing to quarantine in this vault".into(),
            )),
        }
    }

    /// Guard at the top of every mutating operation.
    fn ensure_writable(&self) -> Result<()> {
        if self.read_only {
            return Err(Error::VaultReadOnly);
        }
        if self.is_locked() {
            return Err(Error::VaultLocked);
        }
        Ok(())
    }

    pub fn notes_dir(&self) -> PathBuf {
        self.root.join(NOTES_DIR)
    }

    pub fn attachments_dir(&self) -> PathBuf {
        self.root.join(ATTACHMENTS_DIR)
    }

    pub fn trash_dir(&self) -> PathBuf {
        self.root.join(TRASH_DIR)
    }

    pub fn state_dir(&self) -> PathBuf {
        self.root.join(VAULT_STATE_DIR)
    }

    pub fn history_dir(&self) -> PathBuf {
        self.state_dir().join("history")
    }

    pub fn recovery_dir(&self) -> PathBuf {
        self.state_dir().join("recovery")
    }

    pub fn create_notebook(&self, relative: impl AsRef<Path>) -> Result<PathBuf> {
        self.ensure_writable()?;
        if let Backend::Encrypted(store) = &self.backend {
            return store.create_notebook(relative.as_ref());
        }
        let relative = validate_notebook_path(relative.as_ref())?;
        reject_symlink_components(&self.notes_dir(), &relative)?;
        let path = self.notes_dir().join(relative);
        create_private_directory(&path)?;
        Ok(path)
    }

    /// `Inbox` is created for every vault and is the fallback destination for
    /// new notes and restored notes, so it cannot be renamed or deleted.
    /// Nested notebooks under it (e.g. `Inbox/Drafts`) are not reserved.
    pub fn is_reserved_notebook(relative: &Path) -> bool {
        relative == Path::new(DEFAULT_NOTEBOOK)
    }

    /// Lists every notebook under `Notes`, including `Inbox`, as a flat list
    /// of relative paths with their direct (non-recursive) note counts.
    /// Symbolic links are skipped, matching `scan_notes`.
    pub fn list_notebooks(&self) -> Result<Vec<NotebookEntry>> {
        if let Backend::Encrypted(store) = &self.backend {
            return store.list_notebooks();
        }
        let mut notebooks = Vec::new();
        collect_notebooks(&self.notes_dir(), Path::new(""), &mut notebooks)?;
        notebooks.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        Ok(notebooks)
    }

    /// Renames a notebook in place. The new name is a single path component,
    /// not a full path - the notebook stays where it is in the hierarchy,
    /// only its own name changes. Refuses on `Inbox`, on a name that would
    /// collide with an existing sibling, and on a name that would collide
    /// with the reserved `Inbox` name.
    pub fn rename_notebook(&self, relative: &Path, new_name: &str) -> Result<PathBuf> {
        self.ensure_writable()?;
        if let Backend::Encrypted(store) = &self.backend {
            return store.rename_notebook(relative, new_name);
        }
        let relative = validate_notebook_path(relative)?;
        if Self::is_reserved_notebook(&relative) {
            return Err(Error::ReservedNotebook {
                relative_path: relative,
            });
        }
        reject_symlink_components(&self.notes_dir(), &relative)?;
        let old_path = self.notes_dir().join(&relative);
        if !old_path.is_dir() {
            return Err(Error::InvalidPath(format!(
                "notebook not found: {}",
                old_path.display()
            )));
        }

        let sanitized_name = validate_notebook_name(new_name)?;
        let parent_relative = relative.parent().unwrap_or_else(|| Path::new(""));
        let next_relative = parent_relative.join(&sanitized_name);
        if next_relative == relative {
            return Ok(relative);
        }
        if Self::is_reserved_notebook(&next_relative) {
            return Err(Error::ReservedNotebook {
                relative_path: next_relative,
            });
        }
        let next_path = self.notes_dir().join(&next_relative);
        if next_path.exists() {
            return Err(Error::AlreadyExists(next_path));
        }
        fs::rename(&old_path, &next_path).map_err(|source| io_error(&next_path, source))?;
        sync_directory(
            next_path
                .parent()
                .ok_or_else(|| Error::InvalidPath(next_path.display().to_string()))?,
        )?;
        Ok(next_relative)
    }

    /// Deletes a notebook. Refuses on `Inbox`. Refuses, naming the note
    /// count, if any `.md`/`.snote` file exists anywhere in the subtree.
    /// Refuses, without deleting anything, if any *other* file or symbolic
    /// link exists anywhere in the subtree - SenatorialNotes never
    /// recursively destroys content it does not manage. Only when the whole
    /// subtree is nothing but empty directories does it remove them,
    /// leaf-first, one `fs::remove_dir` at a time (never `remove_dir_all`),
    /// which gives a second safety net for free: `fs::remove_dir` itself
    /// fails on anything that is not empty.
    pub fn delete_notebook(&self, relative: &Path) -> Result<()> {
        self.ensure_writable()?;
        if let Backend::Encrypted(store) = &self.backend {
            return store.delete_notebook(relative);
        }
        let relative = validate_notebook_path(relative)?;
        if Self::is_reserved_notebook(&relative) {
            return Err(Error::ReservedNotebook {
                relative_path: relative,
            });
        }
        reject_symlink_components(&self.notes_dir(), &relative)?;
        let root = self.notes_dir().join(&relative);
        if !root.is_dir() {
            return Err(Error::InvalidPath(format!(
                "notebook not found: {}",
                root.display()
            )));
        }

        let mut note_count = 0_usize;
        let mut has_unmanaged = false;
        scan_notebook_contents(&root, &mut note_count, &mut has_unmanaged)?;
        if note_count > 0 {
            return Err(Error::NotebookNotEmpty {
                relative_path: relative,
                note_count,
            });
        }
        if has_unmanaged {
            return Err(Error::NotebookHasUnmanagedContent {
                relative_path: relative,
            });
        }

        remove_empty_subtree(&root)?;
        sync_directory(
            root.parent()
                .ok_or_else(|| Error::InvalidPath(root.display().to_string()))?,
        )
    }

    /// Moves a note into `destination_notebook`, creating it if needed. The
    /// note's filename (and therefore its UUID suffix) is unchanged, only its
    /// containing directory moves. Never rewrites the note's content or
    /// `updated_at` - this is a filesystem rename, nothing else - and works
    /// identically for plaintext `.md` and encrypted `.snote` notes: the
    /// encrypted container's authenticated header does not include the path,
    /// so a location change never affects decryption (see
    /// `docs/ENCRYPTED_NOTE_FORMAT.md`). Uses [`rename_no_replace`] so a
    /// destination collision fails atomically instead of silently
    /// overwriting an existing note.
    pub fn move_note(
        &self,
        relative: &Path,
        destination_notebook: impl AsRef<Path>,
    ) -> Result<PathBuf> {
        self.ensure_writable()?;
        if let Backend::Encrypted(store) = &self.backend {
            return store.move_note(relative, destination_notebook.as_ref());
        }
        let relative = ensure_relative_note_path(relative)?;
        let source = self.note_path(&relative)?;
        if !source.is_file() {
            return Err(Error::NoteNotFound(source));
        }

        let destination_notebook = validate_notebook_path(destination_notebook.as_ref())?;
        self.create_notebook(&destination_notebook)?;

        let file_name = relative
            .file_name()
            .ok_or_else(|| Error::InvalidPath(relative.display().to_string()))?;
        let next_relative = destination_notebook.join(file_name);
        if next_relative == relative {
            return Ok(relative);
        }
        let destination = self.note_path(&next_relative)?;

        rename_no_replace(&source, &destination)?;
        sync_directory(
            destination
                .parent()
                .ok_or_else(|| Error::InvalidPath(destination.display().to_string()))?,
        )?;
        sync_directory(
            source
                .parent()
                .ok_or_else(|| Error::InvalidPath(source.display().to_string()))?,
        )?;
        Ok(next_relative)
    }

    pub fn create_note(&self, title: &str, notebook: impl AsRef<Path>) -> Result<Note> {
        self.ensure_writable()?;
        if let Backend::Encrypted(store) = &self.backend {
            return store.create_note(title, notebook.as_ref());
        }
        let notebook = validate_notebook_path(notebook.as_ref())?;
        self.create_notebook(&notebook)?;
        let metadata = crate::model::NoteMetadata::new(title);
        let relative_path = notebook.join(note_filename(title, metadata.id));
        let note = Note {
            metadata,
            body: String::new(),
            relative_path,
        };
        let path = self.note_path(&note.relative_path)?;
        if path.exists() {
            return Err(Error::AlreadyExists(path));
        }
        atomic_write(&path, note.to_markdown()?.as_bytes())?;
        Ok(note)
    }

    pub fn load_note(&self, relative: impl AsRef<Path>) -> Result<(Note, FileStamp)> {
        if let Backend::Encrypted(store) = &self.backend {
            return store.load_note(relative.as_ref());
        }
        let relative = ensure_relative_note_path(relative.as_ref())?;
        if relative.extension().and_then(|value| value.to_str()) != Some("md") {
            return Err(Error::WrongNoteType);
        }
        let path = self.note_path(&relative)?;
        let markdown = fs::read_to_string(&path).map_err(|source| io_error(&path, source))?;
        let note = Note::parse(&markdown, relative)?;
        let stamp = file_stamp(&path)?;
        Ok((note, stamp))
    }

    pub fn load_encrypted_note(
        &self,
        relative: impl AsRef<Path>,
        password: &str,
    ) -> Result<(Note, FileStamp, EncryptedSession)> {
        if let Backend::Encrypted(store) = &self.backend {
            return store.load_encrypted_note(relative.as_ref(), password);
        }
        let relative = ensure_relative_note_path(relative.as_ref())?;
        if relative.extension().and_then(|value| value.to_str()) != Some("snote") {
            return Err(Error::WrongNoteType);
        }
        let path = self.note_path(&relative)?;
        let bytes = fs::read(&path).map_err(|source| io_error(&path, source))?;
        let (note, session) = crypto::decrypt(&bytes, password, relative)?;
        let stamp = file_stamp(&path)?;
        Ok((note, stamp, session))
    }

    /// Saves body/metadata to the existing Markdown path without renaming it.
    /// Title commits are deliberately handled by `commit_title`.
    pub fn save_note(&self, note: &mut Note, expected: Option<&FileStamp>) -> Result<FileStamp> {
        self.ensure_writable()?;
        if let Backend::Encrypted(store) = &self.backend {
            return store.save_note(note, expected);
        }
        if note
            .relative_path
            .extension()
            .and_then(|value| value.to_str())
            != Some("md")
        {
            return Err(Error::WrongNoteType);
        }
        let path = self.note_path(&note.relative_path)?;
        verify_stamp(&path, expected)?;
        note.metadata.updated_at = Utc::now();
        atomic_write(&path, note.to_markdown()?.as_bytes())?;
        self.remove_recovery(note.metadata.id)?;
        file_stamp(&path)
    }

    /// Commits a title once, then safely renames the backing Markdown file.
    /// The UUID is never regenerated and an existing destination is never
    /// overwritten.
    pub fn commit_title(
        &self,
        note: &mut Note,
        expected: Option<&FileStamp>,
        title: &str,
    ) -> Result<FileStamp> {
        self.ensure_writable()?;
        if let Backend::Encrypted(store) = &self.backend {
            return store.commit_title(note, expected, title);
        }
        if note
            .relative_path
            .extension()
            .and_then(|value| value.to_str())
            != Some("md")
        {
            return Err(Error::WrongNoteType);
        }
        let old_path = self.note_path(&note.relative_path)?;
        verify_stamp(&old_path, expected)?;
        let normalized = normalized_title(title);
        let parent = note
            .relative_path
            .parent()
            .ok_or_else(|| Error::InvalidPath(note.relative_path.display().to_string()))?;
        let next_relative = parent.join(note_filename(&normalized, note.metadata.id));
        if next_relative != note.relative_path {
            let next_path = self.note_path(&next_relative)?;
            if next_path.exists() {
                return Err(Error::AlreadyExists(next_path));
            }
        }
        note.metadata.title = normalized;
        note.metadata.updated_at = Utc::now();
        atomic_write(&old_path, note.to_markdown()?.as_bytes())?;

        if next_relative != note.relative_path {
            let next_path = self.note_path(&next_relative)?;
            fs::rename(&old_path, &next_path).map_err(|source| io_error(&next_path, source))?;
            sync_directory(
                next_path
                    .parent()
                    .ok_or_else(|| Error::InvalidPath(next_path.display().to_string()))?,
            )?;
            note.relative_path = next_relative;
        }
        self.remove_recovery(note.metadata.id)?;
        file_stamp(&self.note_path(&note.relative_path)?)
    }

    pub fn encrypt_note(
        &self,
        note: &mut Note,
        expected: Option<&FileStamp>,
        password: &str,
    ) -> Result<(FileStamp, EncryptedSession)> {
        self.ensure_writable()?;
        if let Backend::Encrypted(store) = &self.backend {
            return store.encrypt_note(note, expected, password);
        }
        if note
            .relative_path
            .extension()
            .and_then(|value| value.to_str())
            != Some("md")
        {
            return Err(Error::WrongNoteType);
        }
        check_password_policy(password)?;
        let old_path = self.note_path(&note.relative_path)?;
        verify_stamp(&old_path, expected)?;
        let parent = note
            .relative_path
            .parent()
            .ok_or_else(|| Error::InvalidPath(note.relative_path.display().to_string()))?;
        let next_relative = parent.join(encrypted_note_filename(note.metadata.id));
        let next_path = self.note_path(&next_relative)?;
        if next_path.exists() {
            return Err(Error::AlreadyExists(next_path));
        }
        note.metadata.updated_at = Utc::now();
        let (container, session) = crypto::encrypt_new(note, password)?;

        // Replace plaintext at its existing path first. A crash between this
        // write and the extension rename can leave an encrypted `.md`, but it
        // cannot leave a second plaintext copy behind.
        atomic_write(&old_path, &container)?;
        fs::rename(&old_path, &next_path).map_err(|source| io_error(&next_path, source))?;
        sync_directory(
            next_path
                .parent()
                .ok_or_else(|| Error::InvalidPath(next_path.display().to_string()))?,
        )?;
        note.relative_path = next_relative;
        self.remove_recovery(note.metadata.id)?;
        Ok((file_stamp(&next_path)?, session))
    }

    pub fn save_encrypted_note(
        &self,
        note: &mut Note,
        session: &EncryptedSession,
        expected: Option<&FileStamp>,
    ) -> Result<FileStamp> {
        self.ensure_writable()?;
        if let Backend::Encrypted(store) = &self.backend {
            return store.save_encrypted_note(note, session, expected);
        }
        if note
            .relative_path
            .extension()
            .and_then(|value| value.to_str())
            != Some("snote")
        {
            return Err(Error::WrongNoteType);
        }
        let path = self.note_path(&note.relative_path)?;
        verify_stamp(&path, expected)?;
        note.metadata.updated_at = Utc::now();
        let container = crypto::encrypt_with_session(note, session)?;
        atomic_write(&path, &container)?;
        // Encrypted notes never use plaintext recovery files.
        self.remove_recovery(note.metadata.id)?;
        file_stamp(&path)
    }

    /// Re-keys an encrypted note under a fresh salt, key, and nonce.
    ///
    /// The current password is verified by fully decrypting the payload before
    /// anything is written. After the atomic replacement the new container is
    /// re-read and verified: it must open with the new password and, when the
    /// password actually changed, must no longer open with the old one. The
    /// caller receives a freshly verified note, stamp, and session so no stale
    /// encrypted-session state can survive the change.
    pub fn change_encrypted_password(
        &self,
        relative: impl AsRef<Path>,
        old_password: &str,
        new_password: &str,
    ) -> Result<(Note, FileStamp, EncryptedSession)> {
        self.ensure_writable()?;
        if let Backend::Encrypted(store) = &self.backend {
            return store.change_encrypted_password(relative.as_ref(), old_password, new_password);
        }
        let relative = ensure_relative_note_path(relative.as_ref())?;
        if relative.extension().and_then(|value| value.to_str()) != Some("snote") {
            return Err(Error::WrongNoteType);
        }
        check_password_policy(new_password)?;
        let path = self.note_path(&relative)?;
        let bytes = fs::read(&path).map_err(|source| io_error(&path, source))?;
        let (note, _old_session) = crypto::decrypt(&bytes, old_password, relative.clone())?;
        let (container, _new_session) = crypto::encrypt_new(&note, new_password)?;
        atomic_write(&path, &container)?;

        let written = fs::read(&path).map_err(|source| io_error(&path, source))?;
        let (verified_note, verified_session) =
            crypto::decrypt(&written, new_password, relative.clone())
                .map_err(|_| Error::Encryption("re-key verification failed".into()))?;
        if new_password != old_password && crypto::decrypt(&written, old_password, relative).is_ok()
        {
            return Err(Error::Encryption(
                "re-key verification failed: the previous password still unlocks the note".into(),
            ));
        }
        let stamp = file_stamp(&path)?;
        Ok((verified_note, stamp, verified_session))
    }

    pub fn remove_encryption(
        &self,
        relative: impl AsRef<Path>,
        password: &str,
    ) -> Result<(Note, FileStamp)> {
        self.ensure_writable()?;
        if let Backend::Encrypted(store) = &self.backend {
            return store.remove_encryption(relative.as_ref(), password);
        }
        let relative = ensure_relative_note_path(relative.as_ref())?;
        if relative.extension().and_then(|value| value.to_str()) != Some("snote") {
            return Err(Error::WrongNoteType);
        }
        let old_path = self.note_path(&relative)?;
        let bytes = fs::read(&old_path).map_err(|source| io_error(&old_path, source))?;
        let (mut note, _session) = crypto::decrypt(&bytes, password, relative.clone())?;
        let parent = relative
            .parent()
            .ok_or_else(|| Error::InvalidPath(relative.display().to_string()))?;
        let next_relative = parent.join(note_filename(&note.metadata.title, note.metadata.id));
        let next_path = self.note_path(&next_relative)?;
        if next_path.exists() {
            return Err(Error::AlreadyExists(next_path));
        }
        note.metadata.updated_at = Utc::now();
        atomic_write(&old_path, note.to_markdown()?.as_bytes())?;
        fs::rename(&old_path, &next_path).map_err(|source| io_error(&next_path, source))?;
        sync_directory(
            next_path
                .parent()
                .ok_or_else(|| Error::InvalidPath(next_path.display().to_string()))?,
        )?;
        note.relative_path = next_relative;
        let stamp = file_stamp(&next_path)?;
        Ok((note, stamp))
    }

    pub fn write_recovery(&self, note: &Note) -> Result<PathBuf> {
        self.ensure_writable()?;
        if let Backend::Encrypted(store) = &self.backend {
            return store.write_recovery(note);
        }
        if note
            .relative_path
            .extension()
            .and_then(|value| value.to_str())
            != Some("md")
        {
            return Err(Error::WrongNoteType);
        }
        create_private_directory(&self.recovery_dir())?;
        let path = self.recovery_path(note.metadata.id);
        atomic_write(&path, note.to_markdown()?.as_bytes())?;
        Ok(path)
    }

    pub fn scan_notes(&self) -> Result<Vec<NoteSummary>> {
        if let Backend::Encrypted(store) = &self.backend {
            return store.scan_notes();
        }
        let mut notes = Vec::new();
        self.scan_directory(&self.notes_dir(), &mut notes)?;
        // `None` is the shared default comparator (pinned-first, then
        // recency, then title, then UUID) - see `sort::sort_notes`. Callers
        // that want an explicit user-chosen order re-sort afterward.
        crate::sort::sort_notes(&mut notes, None);
        Ok(notes)
    }

    pub fn move_to_trash(&self, relative: impl AsRef<Path>) -> Result<TrashEntry> {
        self.ensure_writable()?;
        if let Backend::Encrypted(store) = &self.backend {
            return store.move_to_trash(relative.as_ref());
        }
        let relative = ensure_relative_note_path(relative.as_ref())?;
        let source = self.note_path(&relative)?;
        let encrypted = relative.extension().and_then(|value| value.to_str()) == Some("snote");
        let (id, title) = if encrypted {
            let bytes = fs::read(&source).map_err(|error| io_error(&source, error))?;
            let note_id = crypto::inspect_header(&bytes)?.note_id;
            (
                note_id,
                format!("Locked Note · {}", locked_note_suffix(note_id)),
            )
        } else {
            let (note, _stamp) = self.load_note(&relative)?;
            (note.metadata.id, note.metadata.title)
        };
        let extension = if encrypted { "snote" } else { "md" };
        let trashed_file = self.trash_dir().join(format!("{id}.{extension}"));
        let record_path = self.trash_record_path(id);
        if trashed_file.exists() || record_path.exists() {
            return Err(Error::AlreadyExists(trashed_file));
        }
        let record = TrashRecord {
            format_version: 1,
            note_id: id,
            original_relative_path: relative.clone(),
            trashed_at: Utc::now(),
            encrypted,
        };
        let record_text = toml::to_string_pretty(&record)
            .map_err(|error| Error::Configuration(error.to_string()))?;
        atomic_write(&record_path, record_text.as_bytes())?;
        if let Err(source_error) = fs::rename(&source, &trashed_file) {
            let _ignored = fs::remove_file(&record_path);
            return Err(io_error(&source, source_error));
        }
        sync_directory(&self.trash_dir())?;
        self.remove_recovery(id)?;
        Ok(TrashEntry {
            id,
            title,
            encrypted,
            original_relative_path: relative,
            trashed_at: record.trashed_at,
        })
    }

    pub fn scan_trash(&self) -> Result<Vec<TrashEntry>> {
        if let Backend::Encrypted(store) = &self.backend {
            return store.scan_trash();
        }
        let mut entries = Vec::new();
        for directory_entry in
            fs::read_dir(self.trash_dir()).map_err(|error| io_error(self.trash_dir(), error))?
        {
            let directory_entry =
                directory_entry.map_err(|error| io_error(self.trash_dir(), error))?;
            let path = directory_entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("toml")
                || !path
                    .file_name()
                    .and_then(|value| value.to_str())
                    .is_some_and(|name| name.ends_with(".trash.toml"))
            {
                continue;
            }
            let source = fs::read_to_string(&path).map_err(|error| io_error(&path, error))?;
            let record: TrashRecord =
                toml::from_str(&source).map_err(|error| Error::Configuration(error.to_string()))?;
            let note_path = self.trashed_note_path(record.note_id, record.encrypted);
            if !note_path.is_file() {
                continue;
            }
            let title = if record.encrypted {
                format!("Locked Note · {}", locked_note_suffix(record.note_id))
            } else {
                let markdown =
                    fs::read_to_string(&note_path).map_err(|error| io_error(&note_path, error))?;
                Note::parse(&markdown, record.original_relative_path.clone())?
                    .metadata
                    .title
            };
            entries.push(TrashEntry {
                id: record.note_id,
                title,
                encrypted: record.encrypted,
                original_relative_path: record.original_relative_path,
                trashed_at: record.trashed_at,
            });
        }
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.trashed_at));
        Ok(entries)
    }

    pub fn restore_from_trash(&self, id: Uuid) -> Result<PathBuf> {
        self.ensure_writable()?;
        if let Backend::Encrypted(store) = &self.backend {
            return store.restore_from_trash(id);
        }
        let record = self.load_trash_record(id)?;
        let source = self.trashed_note_path(id, record.encrypted);
        let mut destination_relative = record.original_relative_path.clone();
        let original_parent = destination_relative
            .parent()
            .ok_or_else(|| Error::InvalidPath(destination_relative.display().to_string()))?;
        self.create_notebook(original_parent)?;
        let mut destination = self.note_path(&destination_relative)?;
        if destination.exists() {
            destination_relative = if record.encrypted {
                Path::new(DEFAULT_NOTEBOOK).join(encrypted_note_filename(id))
            } else {
                let markdown =
                    fs::read_to_string(&source).map_err(|error| io_error(&source, error))?;
                let note = Note::parse(&markdown, destination_relative.clone())?;
                Path::new(DEFAULT_NOTEBOOK).join(note_filename(&note.metadata.title, id))
            };
            destination = self.note_path(&destination_relative)?;
            if destination.exists() {
                return Err(Error::AlreadyExists(destination));
            }
        }
        fs::rename(&source, &destination).map_err(|error| io_error(&destination, error))?;
        let record_path = self.trash_record_path(id);
        fs::remove_file(&record_path).map_err(|error| io_error(&record_path, error))?;
        sync_directory(
            destination
                .parent()
                .ok_or_else(|| Error::InvalidPath(destination.display().to_string()))?,
        )?;
        Ok(destination_relative)
    }

    pub fn permanently_delete(&self, id: Uuid) -> Result<()> {
        self.ensure_writable()?;
        if let Backend::Encrypted(store) = &self.backend {
            return store.permanently_delete(id);
        }
        let record = self.load_trash_record(id)?;
        let note_path = self.trashed_note_path(id, record.encrypted);
        fs::remove_file(&note_path).map_err(|error| io_error(&note_path, error))?;
        let record_path = self.trash_record_path(id);
        fs::remove_file(&record_path).map_err(|error| io_error(&record_path, error))?;
        sync_directory(&self.trash_dir())
    }

    pub fn empty_trash(&self) -> Result<usize> {
        self.ensure_writable()?;
        if let Backend::Encrypted(store) = &self.backend {
            return store.empty_trash();
        }
        let entries = self.scan_trash()?;
        let count = entries.len();
        for entry in entries {
            self.permanently_delete(entry.id)?;
        }
        Ok(count)
    }

    pub fn note_path(&self, relative: &Path) -> Result<PathBuf> {
        if let Backend::Encrypted(store) = &self.backend {
            return store.note_path(relative);
        }
        let relative = ensure_relative_note_path(relative)?;
        reject_symlink_components(&self.notes_dir(), &relative)?;
        Ok(self.notes_dir().join(relative))
    }

    pub fn current_stamp(&self, relative: &Path) -> Result<FileStamp> {
        if let Backend::Encrypted(store) = &self.backend {
            return store.current_stamp(relative);
        }
        let path = self.note_path(relative)?;
        file_stamp(&path)
    }

    fn scan_directory(&self, directory: &Path, notes: &mut Vec<NoteSummary>) -> Result<()> {
        let entries = fs::read_dir(directory).map_err(|source| io_error(directory, source))?;
        for entry in entries {
            let entry = entry.map_err(|source| io_error(directory, source))?;
            let file_type = entry
                .file_type()
                .map_err(|source| io_error(entry.path(), source))?;
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                self.scan_directory(&entry.path(), notes)?;
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let path = entry.path();
            let extension = path.extension().and_then(|value| value.to_str());
            if !matches!(extension, Some("md" | "snote")) {
                continue;
            }
            let relative = path
                .strip_prefix(self.notes_dir())
                .map_err(|_| Error::InvalidPath(path.display().to_string()))?
                .to_path_buf();
            if extension == Some("snote") {
                let bytes = fs::read(&path).map_err(|error| io_error(&path, error))?;
                let header = crypto::inspect_header(&bytes)?;
                notes.push(NoteSummary::locked(header.note_id, relative));
            } else {
                let (note, _stamp) = self.load_note(relative)?;
                notes.push(NoteSummary::from(&note));
            }
        }
        Ok(())
    }

    fn recovery_path(&self, id: Uuid) -> PathBuf {
        self.recovery_dir().join(format!("{id}.md"))
    }

    fn remove_recovery(&self, id: Uuid) -> Result<()> {
        let recovery = self.recovery_path(id);
        if recovery.exists() {
            fs::remove_file(&recovery).map_err(|source| io_error(&recovery, source))?;
        }
        Ok(())
    }

    fn trash_record_path(&self, id: Uuid) -> PathBuf {
        self.trash_dir().join(format!("{id}.trash.toml"))
    }

    fn trashed_note_path(&self, id: Uuid, encrypted: bool) -> PathBuf {
        self.trash_dir()
            .join(format!("{id}.{}", if encrypted { "snote" } else { "md" }))
    }

    fn load_trash_record(&self, id: Uuid) -> Result<TrashRecord> {
        let path = self.trash_record_path(id);
        if !path.is_file() {
            return Err(Error::NoteNotFound(path));
        }
        let source = fs::read_to_string(&path).map_err(|error| io_error(&path, error))?;
        let record: TrashRecord =
            toml::from_str(&source).map_err(|error| Error::Configuration(error.to_string()))?;
        if record.note_id != id {
            return Err(Error::Configuration(
                "trash record UUID does not match its filename".into(),
            ));
        }
        Ok(record)
    }
}

/// Refuses `root` as the target of a *new* encrypted vault when it already
/// holds a vault or plaintext note data. See [`Vault::check_encrypted_target`].
///
/// A non-existent folder, or an existing folder holding only unrelated files,
/// is fine. Rejected: a `vault.toml` (a vault of either kind), a
/// `.senatorial-notes/` directory (a partial/crashed vault), a `Notes/` /
/// `Trash/` / `Attachments/` directory, or any `.md` / `.snote` file anywhere
/// in the tree.
fn ensure_encrypted_target_is_clean(root: &Path) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }
    let reject = || Err(Error::EncryptedVaultTargetNotEmpty(root.to_path_buf()));

    for marker in [VAULT_STATE_DIR, NOTES_DIR, TRASH_DIR, ATTACHMENTS_DIR] {
        if root.join(marker).exists() {
            return reject();
        }
    }
    if contains_markdown_or_snote(root) {
        return reject();
    }
    Ok(())
}

/// Whether any `.md` or `.snote` file exists anywhere under `dir` (symlinks are
/// not followed). Used only to keep encrypted-vault creation off an existing
/// plaintext vault.
fn contains_markdown_or_snote(dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        if file_type.is_dir() {
            if contains_markdown_or_snote(&path) {
                return true;
            }
        } else if matches!(
            path.extension().and_then(|value| value.to_str()),
            Some("md" | "snote")
        ) {
            return true;
        }
    }
    false
}

/// Writes a Standard-Vault trash record (`<trash_dir>/<note_id>.trash.toml`)
/// for a note copied out of a Secure Vault by the Secure \u{2192} Standard
/// export. The matching note file is written separately by the caller. This
/// only ever touches the export's own fresh destination tree.
pub(crate) fn write_exported_trash_record(
    trash_dir: &Path,
    note_id: Uuid,
    original_relative_path: &Path,
    trashed_at: chrono::DateTime<Utc>,
    encrypted: bool,
) -> Result<()> {
    let record = TrashRecord {
        format_version: 1,
        note_id,
        original_relative_path: original_relative_path.to_path_buf(),
        trashed_at,
        encrypted,
    };
    let text =
        toml::to_string_pretty(&record).map_err(|error| Error::Configuration(error.to_string()))?;
    atomic_write(
        &trash_dir.join(format!("{note_id}.trash.toml")),
        text.as_bytes(),
    )
}

pub(crate) fn check_password_policy(password: &str) -> Result<()> {
    let length = password.chars().count();
    if length < crate::constants::MIN_PASSWORD_LENGTH {
        return Err(Error::WeakPassword(format!(
            "SenatorialNotes requires at least {} characters",
            crate::constants::MIN_PASSWORD_LENGTH
        )));
    }
    Ok(())
}

pub(crate) fn normalized_title(title: &str) -> String {
    let title = title.trim();
    if title.is_empty() {
        "Untitled".into()
    } else {
        title.into()
    }
}

fn verify_stamp(path: &Path, expected: Option<&FileStamp>) -> Result<()> {
    if let Some(expected) = expected {
        let current = file_stamp(path)?;
        if &current != expected {
            return Err(Error::ExternalModification(path.to_path_buf()));
        }
    }
    Ok(())
}

pub(crate) fn file_stamp(path: &Path) -> Result<FileStamp> {
    let metadata = fs::metadata(path).map_err(|source| io_error(path, source))?;
    let modified = metadata
        .modified()
        .map_err(|source| io_error(path, source))?;
    let contents = fs::read(path).map_err(|source| io_error(path, source))?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    hasher.write(&contents);
    Ok(FileStamp {
        modified,
        length: metadata.len(),
        fingerprint: hasher.finish(),
    })
}

pub(crate) fn create_private_directory(path: &Path) -> Result<()> {
    let existed = path.exists();
    fs::create_dir_all(path).map_err(|source| io_error(path, source))?;
    #[cfg(unix)]
    if !existed {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|source| io_error(path, source))?;
    }
    Ok(())
}

fn reject_symlink_components(base: &Path, relative: &Path) -> Result<()> {
    let mut current = base.to_path_buf();
    for component in relative.components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(Error::InvalidPath(format!(
                    "symbolic links are not allowed in vault paths: {}",
                    current.display()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(source) => return Err(io_error(&current, source)),
        }
    }
    Ok(())
}

fn collect_notebooks(
    absolute_dir: &Path,
    relative_dir: &Path,
    notebooks: &mut Vec<NotebookEntry>,
) -> Result<()> {
    let entries = fs::read_dir(absolute_dir).map_err(|source| io_error(absolute_dir, source))?;
    let mut direct_note_count = 0_usize;
    let mut subdirectories = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| io_error(absolute_dir, source))?;
        let file_type = entry
            .file_type()
            .map_err(|source| io_error(entry.path(), source))?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            subdirectories.push(entry.file_name());
        } else if file_type.is_file()
            && matches!(
                entry.path().extension().and_then(|value| value.to_str()),
                Some("md" | "snote")
            )
        {
            direct_note_count += 1;
        }
    }
    if !relative_dir.as_os_str().is_empty() {
        notebooks.push(NotebookEntry {
            relative_path: relative_dir.to_path_buf(),
            direct_note_count,
        });
    }
    for name in subdirectories {
        let child_relative = relative_dir.join(&name);
        let child_absolute = absolute_dir.join(&name);
        collect_notebooks(&child_absolute, &child_relative, notebooks)?;
    }
    Ok(())
}

/// Scans a notebook subtree for anything that would make deletion unsafe.
/// `note_count` accumulates every `.md`/`.snote` file found anywhere in the
/// subtree; `has_unmanaged` is set if any other file or symbolic link is
/// found anywhere in the subtree. Neither is ever destroyed by the caller.
fn scan_notebook_contents(
    absolute: &Path,
    note_count: &mut usize,
    has_unmanaged: &mut bool,
) -> Result<()> {
    let entries = fs::read_dir(absolute).map_err(|source| io_error(absolute, source))?;
    let mut subdirectories = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| io_error(absolute, source))?;
        let file_type = entry
            .file_type()
            .map_err(|source| io_error(entry.path(), source))?;
        if file_type.is_symlink() {
            *has_unmanaged = true;
        } else if file_type.is_dir() {
            subdirectories.push(entry.path());
        } else if file_type.is_file() {
            let is_note = matches!(
                entry.path().extension().and_then(|value| value.to_str()),
                Some("md" | "snote")
            );
            if is_note {
                *note_count += 1;
            } else {
                *has_unmanaged = true;
            }
        } else {
            *has_unmanaged = true;
        }
    }
    for child in subdirectories {
        scan_notebook_contents(&child, note_count, has_unmanaged)?;
    }
    Ok(())
}

/// Removes `root` and every directory beneath it, leaf-first, using
/// `fs::remove_dir` (never `remove_dir_all`). `fs::remove_dir` fails on any
/// non-empty directory, so even a race between the safety scan above and
/// this call cannot destroy something that appeared in between.
fn remove_empty_subtree(root: &Path) -> Result<()> {
    let entries = fs::read_dir(root).map_err(|source| io_error(root, source))?;
    let mut subdirectories = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| io_error(root, source))?;
        if entry
            .file_type()
            .map_err(|source| io_error(entry.path(), source))?
            .is_dir()
        {
            subdirectories.push(entry.path());
        }
    }
    for child in subdirectories {
        remove_empty_subtree(&child)?;
    }
    fs::remove_dir(root).map_err(|source| io_error(root, source))
}

fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let directory = fs::File::open(path).map_err(|source| io_error(path, source))?;
        directory
            .sync_all()
            .map_err(|source| io_error(path, source))?;
    }
    Ok(())
}
