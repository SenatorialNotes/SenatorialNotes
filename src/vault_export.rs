//! Secure \u{2192} Standard **safe export**: builds a brand-new, unencrypted
//! Standard Vault containing plaintext copies of every note in an unlocked
//! Secure Vault. The source Secure Vault is only ever read.
//!
//! This is **not** in-place conversion. The result is a separate vault with its
//! own `vault_id`.
//!
//! Design constraints (Stage E):
//!
//! * **Worker-thread safe.** Every input is owned and `Send`; the live
//!   (non-`Send`) [`EncryptedStore`](crate::vault_encrypted) is never touched.
//!   The worker re-derives its own [`VaultKeys`](crate::crypto::vault::VaultKeys)
//!   from a password the user re-enters, and drops them (`Zeroizing`) when it
//!   returns.
//! * **Directory-transactional.** The whole vault is built inside an
//!   application-owned `<dest-parent>/.senatorial-export-<rand>.tmp/` and made
//!   the destination by a **single atomic rename** once complete and validated
//!   — never a recursive copy. Before finalization the requested destination
//!   does not exist; after it, it appears atomically as the complete validated
//!   vault. On any failure (including an unexpected cross-filesystem rename) the
//!   requested destination never appears, the source is untouched, and the
//!   staging directory is removed (or its exact path is reported).
//! * **Attachments fail closed.** If the encrypted manifest carries any
//!   attachment record the export is refused before anything is written — the
//!   Standard Vault has no representation for attachments yet and silent loss is
//!   not acceptable.
//! * **No source mutation.** Nothing here calls a mutating `Vault` /
//!   `EncryptedStore` method on the source.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use uuid::Uuid;
use zeroize::Zeroizing;

use crate::crypto::vault::{ObjectType, open_keyfile};
use crate::error::io_error;
use crate::vault::Vault;
use crate::{Error, Result, vault_encrypted};

const TEMP_PREFIX: &str = ".senatorial-export-";
const TEMP_SUFFIX: &str = ".tmp";
const EXDEV: i32 = 18;

/// Everything the worker needs. All fields are owned and `Send`.
pub struct ExportParams {
    /// The Secure Vault's root directory.
    pub source_root: PathBuf,
    /// `<source_root>/.senatorial-notes` — the Secure Vault's state directory.
    pub source_state_dir: PathBuf,
    /// The Secure Vault's id (from its `vault.toml`).
    pub vault_id: Uuid,
    /// The raw `vault.keys` bytes.
    pub keyfile_bytes: Vec<u8>,
    /// The Vault Password, re-entered by the user for this export. Used only to
    /// derive the worker's key material; never stored, never logged.
    pub password: Zeroizing<String>,
    /// Where the finished Standard Vault should end up.
    pub destination: PathBuf,
}

/// A thread-safe progress + cancellation handle shared between the worker and
/// the UI. Cheap to clone (`Arc`).
#[derive(Clone, Default)]
pub struct ExportProgress(Arc<ProgressInner>);

#[derive(Default)]
struct ProgressInner {
    done: AtomicUsize,
    total: AtomicUsize,
    cancelled: AtomicBool,
}

impl ExportProgress {
    pub fn new() -> Self {
        Self::default()
    }

    /// Objects processed so far.
    pub fn done(&self) -> usize {
        self.0.done.load(Ordering::Relaxed)
    }

    /// Total objects to process (0 until the worker has read the manifest).
    pub fn total(&self) -> usize {
        self.0.total.load(Ordering::Relaxed)
    }

    /// Ask the worker to stop at the next object boundary.
    pub fn request_cancel(&self) {
        self.0.cancelled.store(true, Ordering::Relaxed);
    }

    /// Whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Relaxed)
    }

    fn set_total(&self, total: usize) {
        self.0.total.store(total, Ordering::Relaxed);
    }

    fn tick(&self) {
        self.0.done.fetch_add(1, Ordering::Relaxed);
    }
}

/// What a completed export produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportReport {
    pub destination: PathBuf,
    /// Live plaintext `.md` notes copied.
    pub notes: usize,
    /// Live per-note-encrypted `.snote` notes copied (byte-identical inner
    /// container).
    pub snotes: usize,
    /// Notebooks recreated (including empty ones and `Inbox`).
    pub notebooks: usize,
    /// Trashed notes copied into the Standard Vault's `Trash/`.
    pub trashed: usize,
}

/// Exports the unlocked Secure Vault described by `params` to a new Standard
/// Vault at `params.destination`. Blocking; call from a worker thread.
pub fn export_secure_vault_to_standard(
    params: ExportParams,
    progress: ExportProgress,
) -> Result<ExportReport> {
    let destination = params.destination.clone();
    validate_destination(&params.source_root, &destination)?;

    // Re-derive key material from the re-entered password (wrong password ->
    // DecryptionFailed). The worker owns this `VaultKeys` and drops it on
    // return.
    let keys = open_keyfile(&params.keyfile_bytes, params.vault_id, &params.password)?;

    let manifest = vault_encrypted::read_sealed_manifest(&params.source_state_dir, &keys)?;

    // Attachments fail closed, before any destination is created.
    if !manifest.attachments.is_empty() {
        return Err(Error::ExportUnsupportedContent {
            attachments: manifest.attachments.len(),
        });
    }

    progress.set_total(manifest.notes.len() + manifest.trash.len());

    let temp = make_temp_dir(&destination)?;
    match build_and_finalize(&temp, &destination, &params, &keys, &manifest, &progress) {
        Ok(report) => Ok(report),
        Err(error) => {
            // The destination must never be left looking like a finished vault.
            if destination.exists() {
                let _ = fs::remove_dir_all(&destination);
            }
            if temp.exists() && fs::remove_dir_all(&temp).is_err() {
                return Err(Error::ExportCleanupFailed { temp });
            }
            Err(error)
        }
    }
}

fn build_and_finalize(
    temp: &Path,
    destination: &Path,
    params: &ExportParams,
    keys: &crate::crypto::vault::VaultKeys,
    manifest: &vault_encrypted::Manifest,
    progress: &ExportProgress,
) -> Result<ExportReport> {
    let dest_vault = Vault::create(temp)?;
    let notes_root = temp.join("Notes");
    let trash_root = temp.join("Trash");

    // Notebooks first (recreates empty ones and Inbox).
    for notebook in &manifest.notebooks {
        if notebook.is_empty() {
            continue;
        }
        dest_vault.create_notebook(notebook)?;
    }

    let mut notes = 0_usize;
    let mut snotes = 0_usize;
    for record in &manifest.notes {
        cancel_point(progress)?;
        let blob_path =
            vault_encrypted::store_blob_path(&params.source_state_dir, &record.object_id);
        let blob = fs::read(&blob_path).map_err(|source| io_error(&blob_path, source))?;
        let object_type = if record.snote {
            ObjectType::InnerSnote
        } else {
            ObjectType::Note
        };
        // Peel the OUTER (vault) layer only. For a `.snote` this yields the
        // byte-identical v1 `.snote` container, which still opens with its own
        // per-note password — no note password is needed or asked for.
        let (plaintext, _) = keys.open(object_type, record.object_uuid, &blob)?;

        let relative = record.relative_path();
        let target = notes_root.join(&relative);
        if let Some(parent) = target.parent() {
            crate::vault::create_private_directory(parent)?;
        }
        crate::storage::atomic::atomic_write(&target, &plaintext)?;

        if record.snote {
            snotes += 1;
        } else {
            notes += 1;
        }
        progress.tick();
    }

    let mut trashed = 0_usize;
    if !manifest.trash.is_empty() {
        crate::vault::create_private_directory(&trash_root)?;
    }
    for record in &manifest.trash {
        cancel_point(progress)?;
        let blob_path =
            vault_encrypted::store_blob_path(&params.source_state_dir, &record.object_id);
        let blob = fs::read(&blob_path).map_err(|source| io_error(&blob_path, source))?;
        let object_type = if record.snote {
            ObjectType::InnerSnote
        } else {
            ObjectType::Note
        };
        let (plaintext, _) = keys.open(object_type, record.object_uuid, &blob)?;

        let extension = if record.snote { "snote" } else { "md" };
        let note_file = trash_root.join(format!("{}.{extension}", record.note_id));
        crate::storage::atomic::atomic_write(&note_file, &plaintext)?;
        crate::vault::write_exported_trash_record(
            &trash_root,
            record.note_id,
            Path::new(&record.original_relative_path),
            record.trashed_at,
            record.snote,
        )?;
        trashed += 1;
        progress.tick();
    }

    let notebooks = manifest.notebooks.iter().filter(|n| !n.is_empty()).count();

    // Validate the built vault before it is allowed to become the destination.
    drop(dest_vault);
    let check = Vault::open(temp)?;
    let scanned = check.scan_notes()?.len();
    if scanned != manifest.notes.len() {
        return Err(Error::ExportFailed(format!(
            "built vault holds {scanned} notes but the source has {}",
            manifest.notes.len()
        )));
    }
    let scanned_trash = check.scan_trash()?.len();
    if scanned_trash != manifest.trash.len() {
        return Err(Error::ExportFailed(format!(
            "built vault holds {scanned_trash} trashed notes but the source has {}",
            manifest.trash.len()
        )));
    }
    drop(check);

    finalize_move(temp, destination)?;

    Ok(ExportReport {
        destination: destination.to_path_buf(),
        notes,
        snotes,
        notebooks,
        trashed,
    })
}

fn cancel_point(progress: &ExportProgress) -> Result<()> {
    if progress.is_cancelled() {
        Err(Error::ExportCancelled)
    } else {
        Ok(())
    }
}

/// Rejects a destination that could clobber the source or an existing vault /
/// non-empty directory. Canonical-path aliases of the source and of the
/// destination's parent are resolved so a symlinked alias cannot slip past.
fn validate_destination(source_root: &Path, destination: &Path) -> Result<()> {
    let invalid = |message: String| Err(Error::ExportTargetInvalid(message));

    if destination.as_os_str().is_empty() {
        return invalid("no destination folder was chosen".into());
    }

    let source_canon = source_root
        .canonicalize()
        .unwrap_or_else(|_| source_root.to_path_buf());

    // Resolve the destination as far as it exists: canonicalize the deepest
    // existing ancestor and re-append the rest.
    let dest_resolved = resolve_partial(destination);

    if dest_resolved == source_canon {
        return invalid("the destination is the Secure Vault itself".into());
    }
    if dest_resolved.starts_with(&source_canon) {
        return invalid("the destination is inside the Secure Vault".into());
    }
    if source_canon.starts_with(&dest_resolved) {
        return invalid("the Secure Vault is inside the destination".into());
    }

    if destination.exists() {
        let mut entries =
            fs::read_dir(destination).map_err(|source| io_error(destination, source))?;
        if entries.next().is_some() {
            return invalid(format!(
                "{} already exists and is not empty",
                destination.display()
            ));
        }
    } else {
        // The parent must exist and be a directory we can write into.
        let parent = destination.parent().unwrap_or(Path::new("."));
        if !parent.is_dir() {
            return invalid(format!("{} is not an existing folder", parent.display()));
        }
    }

    // Never export onto an existing vault of either kind.
    if Vault::check_encrypted_target(destination).is_err() {
        return invalid(format!(
            "{} already contains a vault or note files",
            destination.display()
        ));
    }
    Ok(())
}

/// Canonicalizes the deepest existing ancestor of `path` and re-appends the
/// non-existent tail, so path relationships can be compared without following a
/// dangling final component.
fn resolve_partial(path: &Path) -> PathBuf {
    let mut existing = path.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    while !existing.exists() {
        match existing.file_name() {
            Some(name) => {
                tail.push(name.to_os_string());
                if !existing.pop() {
                    break;
                }
            }
            None => break,
        }
    }
    let mut resolved = existing.canonicalize().unwrap_or(existing);
    for name in tail.into_iter().rev() {
        resolved.push(name);
    }
    resolved
}

fn make_temp_dir(destination: &Path) -> Result<PathBuf> {
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    for _ in 0..50 {
        let mut bytes = [0_u8; 8];
        getrandom::fill(&mut bytes).map_err(|error| Error::Encryption(error.to_string()))?;
        let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let candidate = parent.join(format!("{TEMP_PREFIX}{token}{TEMP_SUFFIX}"));
        if !candidate.exists() {
            crate::vault::create_private_directory(&candidate)?;
            return Ok(candidate);
        }
    }
    Err(Error::ExportTargetInvalid(
        "could not create a temporary export directory".into(),
    ))
}

/// Moves the fully built, validated staging directory onto the requested
/// destination with a **single atomic rename** — never a recursive copy.
///
/// The staging directory is created in the destination's own parent
/// (`make_temp_dir`), so this rename is same-filesystem. If it still fails —
/// `EXDEV` or anything else — that is an export failure: the destination never
/// appears, so it can never gradually materialise as a "successful-looking"
/// partial vault. The caller cleans the staging directory.
fn finalize_move(temp: &Path, destination: &Path) -> Result<()> {
    fs::rename(temp, destination).map_err(|source| {
        if source.raw_os_error() == Some(EXDEV) {
            Error::ExportFailed(format!(
                "the destination {} is on a different filesystem than its parent, so the export \
                 could not be finalized with an atomic move; nothing was written there",
                destination.display()
            ))
        } else {
            io_error(destination, source)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault_encrypted::EncryptedStore;
    use std::path::Path;
    use tempfile::tempdir;

    const PW: &str = "correct horse battery staple";

    fn export_params(source: &Vault, destination: &Path) -> ExportParams {
        ExportParams {
            source_root: source.root().to_path_buf(),
            source_state_dir: source.state_dir(),
            vault_id: source.vault_id(),
            keyfile_bytes: fs::read(source.state_dir().join("vault.keys")).unwrap(),
            password: Zeroizing::new(PW.to_string()),
            destination: destination.to_path_buf(),
        }
    }

    #[test]
    fn an_attachment_bearing_vault_fails_closed_before_any_destination_is_created() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("Secure");
        let source = Vault::create_encrypted(&root, PW).unwrap();

        // Attachments have no public UI; reach the dormant `k_attachments` path
        // directly to prove the export refuses rather than silently drops it.
        let store = EncryptedStore::open(&source.state_dir(), source.vault_id()).unwrap();
        store.unlock(PW).unwrap();
        let note = store.create_note("Has file", Path::new("Inbox")).unwrap();
        store
            .write_attachment(note.metadata.id, "diagram.png", b"\x89PNG payload")
            .unwrap();

        let dest = dir.path().join("Exported");
        let err =
            export_secure_vault_to_standard(export_params(&source, &dest), ExportProgress::new())
                .unwrap_err();
        assert!(matches!(
            err,
            Error::ExportUnsupportedContent { attachments: 1 }
        ));
        assert!(
            !dest.exists(),
            "nothing is written when attachments are present"
        );
        assert!(
            fs::read_dir(dir.path()).unwrap().flatten().all(|e| !e
                .file_name()
                .to_string_lossy()
                .starts_with(".senatorial-export-")),
            "no temp directory is created either"
        );
    }

    #[test]
    fn resolve_partial_handles_a_dangling_final_component() {
        let dir = tempdir().unwrap();
        let real = dir.path().canonicalize().unwrap();
        let resolved = resolve_partial(&dir.path().join("does/not/exist"));
        assert_eq!(resolved, real.join("does/not/exist"));
    }

    /// A failed finalization must never leave a partial "successful-looking"
    /// vault at the requested destination — and must never fall back to a
    /// recursive copy. `finalize_move` is a bare atomic rename; here its rename
    /// fails (unwritable destination parent) and we assert the destination
    /// stays absent while the built staging directory is left intact for the
    /// caller to clean.
    #[test]
    #[cfg(unix)]
    fn a_finalization_failure_never_creates_a_partial_destination() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let staging = dir.path().join(".senatorial-export-deadbeef.tmp");
        fs::create_dir_all(staging.join("Notes/Inbox")).unwrap();
        fs::write(staging.join("Notes/Inbox/a.md"), b"built vault content").unwrap();
        fs::write(staging.join(".senatorial-notes-marker"), b"x").unwrap();

        let locked_parent = dir.path().join("locked-parent");
        fs::create_dir(&locked_parent).unwrap();
        let destination = locked_parent.join("Exported");
        fs::set_permissions(&locked_parent, fs::Permissions::from_mode(0o500)).unwrap();

        let result = finalize_move(&staging, &destination);

        // Restore so tempdir cleanup can run.
        fs::set_permissions(&locked_parent, fs::Permissions::from_mode(0o700)).unwrap();

        assert!(result.is_err(), "a failed finalize must return an error");
        assert!(
            !destination.exists(),
            "the requested destination must never partially appear"
        );
        assert!(
            staging.join("Notes/Inbox/a.md").is_file(),
            "the staging directory is untouched, left for the caller to clean"
        );
    }
}
