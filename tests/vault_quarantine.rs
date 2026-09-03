//! Stage E / R18: detection and explicit, user-consented quarantine of
//! plaintext storage artifacts an old or incompatible binary wrote into a
//! Secure Vault's root.
//!
//! Rules under test:
//! * detection is read-only — opening an affected vault moves nothing;
//! * an affected Secure Vault opens **forced read-only** (no mutation);
//! * quarantine only ever *renames* into `.senatorial-notes/quarantine/<ts>/`;
//! * nothing is deleted, merged, parsed, or overwritten;
//! * empty legacy directories and unrelated files (`README.txt`) never trigger;
//! * `.senatorial-notes/` is never inspected or moved.

use std::fs;
use std::path::Path;

use senatorial_notes::vault_quarantine::{self, ArtifactCategory};
use senatorial_notes::{Error, Vault};
use tempfile::tempdir;

const PASSWORD: &str = "correct horse battery staple";

fn secure_vault(root: &Path) -> Vault {
    Vault::create_encrypted(root, PASSWORD).expect("encrypted vault")
}

/// Simulates an old binary: it never reads `vault.toml`, creates the standard
/// directory tree, and writes a plaintext note into a top-level `Notes/`.
fn old_binary_wrote_a_note(root: &Path) {
    fs::create_dir_all(root.join("Notes/Inbox")).unwrap();
    fs::write(
        root.join("Notes/Inbox/hello-1a2b3c4d.md"),
        b"---\nid: 1a2b3c4d-0000-0000-0000-000000000000\ntitle: Hello\n---\nbody\n",
    )
    .unwrap();
}

fn quarantine_root(root: &Path) -> std::path::PathBuf {
    root.join(".senatorial-notes").join("quarantine")
}

fn files_under(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    fn walk(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else {
                out.push(path);
            }
        }
    }
    walk(dir, &mut out);
    out.sort();
    out
}

#[test]
fn a_clean_secure_vault_has_no_pending_quarantine() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("Vault");
    let vault = secure_vault(&root);
    assert!(vault.pending_quarantine().is_none());
    assert!(!vault.is_read_only());
    assert!(vault_quarantine::detect(&root).is_none());
}

#[test]
fn empty_legacy_directories_alone_do_not_trigger() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("Vault");
    secure_vault(&root);
    for name in ["Notes", "Notes/Inbox", "Trash", "Attachments"] {
        fs::create_dir_all(root.join(name)).unwrap();
    }
    assert!(
        vault_quarantine::detect(&root).is_none(),
        "empty standard directories are not a conflict"
    );
    assert!(Vault::open(&root).unwrap().pending_quarantine().is_none());
}

#[test]
fn an_unrelated_file_does_not_trigger() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("Vault");
    secure_vault(&root);
    fs::write(root.join("README.txt"), b"my vault").unwrap();
    fs::write(root.join("notes.txt"), b"scratch").unwrap();
    assert!(vault_quarantine::detect(&root).is_none());
}

#[test]
fn detection_is_read_only_and_forces_the_session_read_only() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("Vault");
    secure_vault(&root);
    old_binary_wrote_a_note(&root);
    let before = files_under(&root);

    let vault = Vault::open(&root).expect("opens, but read-only");
    let pending = vault.pending_quarantine().expect("conflict detected");
    assert!(
        vault.is_read_only(),
        "an affected Secure Vault opens read-only"
    );
    assert_eq!(pending.file_count(), 1);
    assert!(
        pending
            .categories()
            .contains(&ArtifactCategory::NotesDirectory)
    );
    assert_eq!(
        before,
        files_under(&root),
        "opening the vault must not move or delete anything"
    );

    // Mutation is refused while the conflict stands.
    let err = vault.create_note("x", "Inbox").unwrap_err();
    assert!(matches!(err, Error::VaultReadOnly));
    // The vault can still be unlocked for a read-only look.
    assert!(vault.unlock(PASSWORD).is_ok());
}

#[test]
fn quarantine_moves_everything_unchanged_then_the_vault_opens_clean() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("Vault");
    secure_vault(&root);

    old_binary_wrote_a_note(&root);
    fs::create_dir_all(root.join("Trash")).unwrap();
    fs::write(root.join("Trash/99.md"), b"trashed").unwrap();
    fs::write(root.join("loose-note.md"), b"loose").unwrap();
    fs::write(root.join("secret.snote"), b"SNOTE\0\0\0rest").unwrap();
    fs::create_dir_all(root.join("Attachments")).unwrap();
    fs::write(root.join("Attachments/pic.png"), b"\x89PNG").unwrap();
    fs::write(root.join("README.txt"), b"keep me").unwrap();

    let note_bytes = fs::read(root.join("Notes/Inbox/hello-1a2b3c4d.md")).unwrap();

    let vault = Vault::open(&root).unwrap();
    let report = vault.quarantine_plaintext().expect("quarantine succeeds");

    assert_eq!(
        report.file_count, 5,
        "1 note + 1 trash + 1 .md + 1 .snote + 1 attachment"
    );
    assert!(report.quarantine_path.starts_with(quarantine_root(&root)));

    // Originals are gone from the vault root...
    assert!(!root.join("Notes").exists());
    assert!(!root.join("Trash").exists());
    assert!(!root.join("Attachments").exists());
    assert!(!root.join("loose-note.md").exists());
    assert!(!root.join("secret.snote").exists());
    // ...unrelated files untouched...
    assert_eq!(fs::read(root.join("README.txt")).unwrap(), b"keep me");
    // ...and the encrypted store untouched.
    assert!(root.join(".senatorial-notes/store/manifest").is_file());

    // Every moved file is byte-identical in quarantine.
    let moved = files_under(&report.quarantine_path);
    assert_eq!(moved.len(), 5);
    let moved_note = moved
        .iter()
        .find(|p| p.file_name().unwrap() == "hello-1a2b3c4d.md")
        .unwrap();
    assert_eq!(fs::read(moved_note).unwrap(), note_bytes);

    // Re-open: clean, writable.
    let reopened = Vault::open(&root).unwrap();
    assert!(reopened.pending_quarantine().is_none());
    assert!(!reopened.is_read_only());
    reopened.unlock(PASSWORD).unwrap();
    assert!(reopened.create_note("now works", "Inbox").is_ok());
}

#[test]
fn a_second_quarantine_gets_its_own_timestamped_folder() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("Vault");
    secure_vault(&root);

    old_binary_wrote_a_note(&root);
    let first = Vault::open(&root).unwrap().quarantine_plaintext().unwrap();

    old_binary_wrote_a_note(&root);
    let second = Vault::open(&root).unwrap().quarantine_plaintext().unwrap();

    assert_ne!(first.quarantine_path, second.quarantine_path);
    assert!(first.quarantine_path.is_dir());
    assert!(second.quarantine_path.is_dir());
}

#[test]
fn an_ordinary_vault_is_never_flagged() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("Vault");
    let vault = Vault::create(&root).unwrap();
    vault.create_note("real note", "Inbox").unwrap();
    // A normal ordinary vault has a top-level Notes/ full of .md files; the
    // quarantine check is only ever run for a Secure Vault, so an ordinary
    // vault is never forced read-only by it.
    let reopened = Vault::open(&root).unwrap();
    assert!(reopened.pending_quarantine().is_none());
    assert!(!reopened.is_read_only());
}
