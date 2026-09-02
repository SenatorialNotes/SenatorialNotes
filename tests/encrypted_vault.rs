//! Stage D: whole-vault encryption engine, exercised through the public
//! [`Vault`] API (an encrypted vault is created, locked, unlocked, and edited
//! exactly as the GUI would drive it).
//!
//! Container-level guarantees (AEAD, AAD binding, HKDF domain separation, VMK
//! wrap/unwrap, cross-object / object-type substitution) are covered by the
//! unit tests in `src/crypto/vault.rs`; the `.snote` container itself by
//! `tests/encryption_regressions.rs`. This file covers the vault engine that
//! sits on top: the keyfile, the sealed manifest, the opaque ciphertext store,
//! the lock lifecycle, and byte-for-byte compatibility of an ordinary vault.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use senatorial_notes::vault_manifest::{ENCRYPTED_MANIFEST_VERSION, ORDINARY_MANIFEST_VERSION};
use senatorial_notes::{Error, Vault, VaultKind};
use tempfile::tempdir;

const PASSWORD: &str = "correct horse battery staple";

fn store_dir(vault: &Vault) -> PathBuf {
    vault.state_dir().join("store")
}

/// Every regular file under the vault state directory (ciphertext blobs, the
/// manifest, the keyfile, `vault.toml`) as raw bytes.
fn state_files(vault: &Vault) -> Vec<(PathBuf, Vec<u8>)> {
    fn walk(dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if let Ok(bytes) = fs::read(&path) {
                out.push((path, bytes));
            }
        }
    }
    let mut out = Vec::new();
    walk(&vault.state_dir(), &mut out);
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn blob_files(vault: &Vault) -> Vec<(PathBuf, Vec<u8>)> {
    let store = store_dir(vault);
    let mut out: Vec<_> = fs::read_dir(&store)
        .expect("store dir")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .map(|path| (path.clone(), fs::read(&path).expect("blob")))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn make_encrypted_vault(root: &Path) -> Vault {
    Vault::create_encrypted(root, PASSWORD).expect("encrypted vault should be created")
}

/// Every regular file anywhere under the vault root.
fn all_files(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
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
    let mut out = Vec::new();
    walk(root, &mut out);
    out.sort();
    out
}

/// Fails if an encrypted vault has grown any ordinary-storage artifact: a
/// top-level `Notes/` / `Trash/` / `Attachments/` directory, or any `.md` /
/// `.snote` file, or any file at all outside `.senatorial-notes/`.
fn assert_no_plaintext_artifacts(root: &Path) {
    for dir in ["Notes", "Trash", "Attachments"] {
        assert!(
            !root.join(dir).exists(),
            "encrypted vault grew an ordinary {dir}/ directory"
        );
    }
    let state = root.join(".senatorial-notes");
    for file in all_files(root) {
        assert!(
            file.starts_with(&state),
            "file outside .senatorial-notes/ in an encrypted vault: {}",
            file.display()
        );
        let ext = file.extension().and_then(|e| e.to_str());
        assert!(
            ext != Some("md") && ext != Some("snote"),
            "plaintext note file in an encrypted vault: {}",
            file.display()
        );
    }
    // Note content lives only under the encrypted store.
    let store = state.join("store");
    for file in all_files(&state) {
        let name = file.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let allowed_outside_store =
            matches!(name, "vault.toml" | "vault.keys" | "vault.lock") || file.starts_with(&store);
        assert!(
            allowed_outside_store,
            "unexpected file outside the encrypted store: {}",
            file.display()
        );
    }
}

// ---------------------------------------------------------------------------
// Format identity
// ---------------------------------------------------------------------------

#[test]
fn an_encrypted_vault_is_format_3_with_kind_encrypted_and_a_keyfile() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("Vault");
    let vault = make_encrypted_vault(&root);

    assert!(vault.is_encrypted());
    assert_eq!(vault.kind(), VaultKind::Encrypted);
    assert_eq!(
        vault.manifest().format_version,
        ENCRYPTED_MANIFEST_VERSION,
        "an encrypted vault is format_version 3"
    );
    assert_eq!(ENCRYPTED_MANIFEST_VERSION, 3);
    assert!(
        vault.state_dir().join("vault.keys").is_file(),
        "the keyfile must exist"
    );
    assert!(
        store_dir(&vault).join("manifest").is_file(),
        "the sealed manifest must live under the structurally separate store/"
    );
    assert!(
        !root.join("Notes").exists(),
        "an encrypted vault has no plaintext Notes/ tree an old binary could read"
    );
}

#[test]
fn an_ordinary_vault_is_untouched_by_stage_d() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("Vault");
    let vault = Vault::create(&root).expect("ordinary vault");

    assert!(!vault.is_encrypted());
    assert!(!vault.is_locked());
    assert_eq!(vault.kind(), VaultKind::Ordinary);
    assert_eq!(vault.manifest().format_version, ORDINARY_MANIFEST_VERSION);
    assert_eq!(ORDINARY_MANIFEST_VERSION, 2);

    let created = vault.create_note("Plain", "Inbox").expect("create note");
    let path = vault.note_path(&created.relative_path).expect("note path");
    assert!(
        path.starts_with(root.join("Notes")),
        "an ordinary vault still stores notes as plaintext Markdown under Notes/"
    );
    let on_disk = fs::read_to_string(&path).expect("read note");
    assert!(on_disk.contains("Plain"), "the title is plaintext on disk");
    assert!(
        !root.join(".senatorial-notes").join("vault.keys").exists(),
        "an ordinary vault has no keyfile"
    );
}

// ---------------------------------------------------------------------------
// Unlock / lock lifecycle
// ---------------------------------------------------------------------------

#[test]
fn a_reopened_encrypted_vault_starts_locked_then_unlocks_with_the_right_password() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("Vault");
    {
        let vault = make_encrypted_vault(&root);
        vault.create_note("First", "Inbox").expect("create note");
    }

    let vault = Vault::open(&root).expect("reopen encrypted vault");
    assert!(vault.is_encrypted());
    assert!(vault.is_locked(), "an encrypted vault opens locked");

    assert!(
        matches!(vault.scan_notes(), Err(Error::VaultLocked)),
        "no note access while locked"
    );

    vault.unlock(PASSWORD).expect("correct password unlocks");
    assert!(!vault.is_locked());
    let notes = vault.scan_notes().expect("scan after unlock");
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].title, "First");
}

#[test]
fn a_wrong_password_changes_nothing_and_a_later_correct_one_still_works() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("Vault");
    {
        let vault = make_encrypted_vault(&root);
        vault.create_note("Secret", "Inbox").expect("create note");
    }
    let before = state_files(&Vault::open(&root).unwrap());

    let vault = Vault::open(&root).expect("reopen");
    for _ in 0..3 {
        assert!(
            vault.unlock("definitely not the password").is_err(),
            "wrong password is rejected"
        );
        assert!(vault.is_locked(), "a failed unlock leaves the vault locked");
    }

    let after = state_files(&Vault::open(&root).unwrap());
    assert_eq!(before, after, "failed unlocks must not touch any file");

    vault
        .unlock(PASSWORD)
        .expect("the correct password still works");
    assert_eq!(vault.scan_notes().expect("scan").len(), 1);
}

#[test]
fn locking_clears_decrypted_state_and_blocks_every_read() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("Vault");
    let vault = make_encrypted_vault(&root);
    let created = vault.create_note("Body Holder", "Inbox").expect("create");
    {
        let (mut note, stamp) = vault.load_note(&created.relative_path).expect("load");
        note.body = "TOP-SECRET-CONTENT-Z9".into();
        vault.save_note(&mut note, Some(&stamp)).expect("save");
    }

    // While unlocked the summary carries the decrypted body for local search.
    let unlocked = vault.scan_notes().expect("scan while unlocked");
    assert!(unlocked[0].body.contains("TOP-SECRET-CONTENT-Z9"));

    vault.lock();
    assert!(vault.is_locked());
    assert!(matches!(vault.scan_notes(), Err(Error::VaultLocked)));
    assert!(matches!(
        vault.load_note(&created.relative_path),
        Err(Error::VaultLocked)
    ));
    assert!(matches!(vault.list_notebooks(), Err(Error::VaultLocked)));
    assert!(matches!(vault.scan_trash(), Err(Error::VaultLocked)));

    vault.unlock(PASSWORD).expect("unlock again");
    assert_eq!(vault.scan_notes().expect("scan").len(), 1);
}

// ---------------------------------------------------------------------------
// Keyfile / ciphertext tamper-evidence
// ---------------------------------------------------------------------------

#[test]
fn a_tampered_keyfile_is_rejected() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("Vault");
    {
        make_encrypted_vault(&root);
    }
    let keyfile = root.join(".senatorial-notes").join("vault.keys");
    let mut bytes = fs::read(&keyfile).expect("read keyfile");
    let last = bytes.len() - 1;
    bytes[last] ^= 0x01;
    fs::write(&keyfile, &bytes).expect("write tampered keyfile");

    let vault = Vault::open(&root).expect("a tampered keyfile still opens the (locked) vault");
    assert!(
        vault.unlock(PASSWORD).is_err(),
        "a tampered keyfile must not unlock, even with the right password"
    );
}

#[test]
fn a_tampered_ciphertext_blob_fails_safely_with_no_plaintext() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("Vault");
    let vault = make_encrypted_vault(&root);
    let created = vault.create_note("Tamper", "Inbox").expect("create");
    {
        let (mut note, stamp) = vault.load_note(&created.relative_path).expect("load");
        note.body = "PLAINTEXT-THAT-MUST-NEVER-SURFACE".into();
        vault.save_note(&mut note, Some(&stamp)).expect("save");
    }

    // Flip a byte in the single note blob (everything but the manifest).
    let blob_path = blob_files(&vault)
        .into_iter()
        .map(|(path, _)| path)
        .find(|path| path.file_name().and_then(|n| n.to_str()) != Some("manifest"))
        .expect("a note blob exists");
    let mut bytes = fs::read(&blob_path).expect("read blob");
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xff;
    fs::write(&blob_path, &bytes).expect("write tampered blob");

    match vault.scan_notes() {
        Err(_) => {}
        Ok(summaries) => {
            for summary in summaries {
                assert!(
                    !summary.body.contains("PLAINTEXT-THAT-MUST-NEVER-SURFACE")
                        && !summary.preview.contains("PLAINTEXT"),
                    "tampered ciphertext must never yield plaintext"
                );
            }
        }
    }
    assert!(
        vault.load_note(&created.relative_path).is_err(),
        "loading a tampered note must fail, not return unauthenticated plaintext"
    );
}

#[test]
fn a_blob_from_another_encrypted_vault_fails_authentication() {
    let dir = tempdir().unwrap();
    let vault_a = make_encrypted_vault(&dir.path().join("A"));
    vault_a.create_note("A note", "Inbox").expect("create");
    let (_, a_blob) = blob_files(&vault_a)
        .into_iter()
        .find(|(path, _)| path.file_name().and_then(|n| n.to_str()) != Some("manifest"))
        .expect("a note blob in vault A");

    let vault_b = make_encrypted_vault(&dir.path().join("B"));
    vault_b.create_note("B note", "Inbox").expect("create");
    let (b_blob_path, _) = blob_files(&vault_b)
        .into_iter()
        .find(|(path, _)| path.file_name().and_then(|n| n.to_str()) != Some("manifest"))
        .expect("a note blob in vault B");

    // Drop vault A's ciphertext into vault B's store under B's own blob name.
    fs::write(&b_blob_path, &a_blob).expect("cross-vault splice");

    assert!(
        vault_b.scan_notes().is_err(),
        "a ciphertext bound to vault A's UUID must not authenticate in vault B"
    );
}

// ---------------------------------------------------------------------------
// Nonce uniqueness / no plaintext at rest
// ---------------------------------------------------------------------------

#[test]
fn every_encryption_uses_a_fresh_nonce() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("Vault");
    let vault = make_encrypted_vault(&root);

    // Same content, many objects + many re-saves of one object.
    let mut created = Vec::new();
    for i in 0..6 {
        created.push(
            vault
                .create_note(&format!("Note {i}"), "Inbox")
                .expect("create"),
        );
    }
    let target = &created[0].relative_path;
    for _ in 0..6 {
        let (mut note, stamp) = vault.load_note(target).expect("load");
        note.body = "IDENTICAL BODY EVERY TIME".into();
        vault.save_note(&mut note, Some(&stamp)).expect("save");
    }

    // The SNENC / SNVLT nonce occupies bytes [56..80] of every container
    // header. No two encrypted objects in the vault may share one.
    let mut seen = HashSet::new();
    for (path, bytes) in state_files(&vault) {
        if path.extension().and_then(|e| e.to_str()) == Some("toml") {
            continue;
        }
        let nonce = bytes
            .get(56..80)
            .unwrap_or_else(|| panic!("{} is too short to be a container", path.display()))
            .to_vec();
        assert!(
            seen.insert(nonce),
            "two encrypted objects share a nonce: {}",
            path.display()
        );
    }
}

#[test]
fn no_plaintext_title_body_tag_or_notebook_name_is_written_to_disk() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("Vault");
    let vault = make_encrypted_vault(&root);

    vault
        .create_notebook("SicilianDefence")
        .expect("create notebook");
    let created = vault
        .create_note("MyReallyDistinctiveTitle", "SicilianDefence")
        .expect("create note");
    let (mut note, stamp) = vault.load_note(&created.relative_path).expect("load");
    note.body = "DISTINCTIVE-BODY-TOKEN-8842 and a paragraph of secrets".into();
    note.metadata.tags = vec!["distinctive-tag-token".into()];
    vault.save_note(&mut note, Some(&stamp)).expect("save");

    let needles = [
        "MyReallyDistinctiveTitle",
        "DISTINCTIVE-BODY-TOKEN-8842",
        "distinctive-tag-token",
        "SicilianDefence",
    ];
    for (path, bytes) in state_files(&vault) {
        for needle in needles {
            assert!(
                !bytes
                    .windows(needle.len())
                    .any(|window| window == needle.as_bytes()),
                "{needle:?} appears in plaintext in {}",
                path.display()
            );
        }
    }
}

#[test]
fn recovery_blobs_are_encrypted() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("Vault");
    let vault = make_encrypted_vault(&root);
    let created = vault.create_note("Draft", "Inbox").expect("create");
    let (mut note, _stamp) = vault.load_note(&created.relative_path).expect("load");
    note.body = "UNSAVED-RECOVERY-SECRET-5150".into();
    vault.write_recovery(&note).expect("write recovery");

    for (path, bytes) in state_files(&vault) {
        assert!(
            !bytes
                .windows("UNSAVED-RECOVERY-SECRET-5150".len())
                .any(|w| w == b"UNSAVED-RECOVERY-SECRET-5150"),
            "recovery plaintext leaked into {}",
            path.display()
        );
    }
}

// ---------------------------------------------------------------------------
// Password change: re-wrap the VMK only
// ---------------------------------------------------------------------------

#[test]
fn a_password_change_rewraps_the_vmk_without_re_encrypting_any_payload() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("Vault");
    let vault = make_encrypted_vault(&root);
    vault.create_note("Keep me", "Inbox").expect("create");

    let blobs_before = blob_files(&vault);
    let keyfile_before = fs::read(vault.state_dir().join("vault.keys")).expect("keyfile");

    let new_password = "an entirely different long passphrase";
    vault
        .change_vault_password(PASSWORD, new_password)
        .expect("password change");

    assert_eq!(
        blobs_before,
        blob_files(&vault),
        "no note blob (and not the manifest) may be rewritten by a password change"
    );
    assert_ne!(
        keyfile_before,
        fs::read(vault.state_dir().join("vault.keys")).expect("keyfile"),
        "the keyfile's wrapped VMK must change"
    );

    // The session stays usable (the VMK did not change).
    assert_eq!(vault.scan_notes().expect("scan").len(), 1);

    // A fresh open: only the new password works.
    let reopened = Vault::open(&root).expect("reopen");
    assert!(
        reopened.unlock(PASSWORD).is_err(),
        "the old password is dead"
    );
    reopened
        .unlock(new_password)
        .expect("the new password works");
    assert_eq!(reopened.scan_notes().expect("scan").len(), 1);
}

// ---------------------------------------------------------------------------
// Move / rename never re-encrypts
// ---------------------------------------------------------------------------

#[test]
fn moving_a_note_between_notebooks_rewrites_only_the_manifest() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("Vault");
    let vault = make_encrypted_vault(&root);
    vault.create_notebook("Archive").expect("create notebook");
    let created = vault.create_note("Movable", "Inbox").expect("create");

    let note_blob_before: Vec<u8> = {
        let (mut note, stamp) = vault.load_note(&created.relative_path).expect("load");
        note.body = "STABLE BODY".into();
        vault.save_note(&mut note, Some(&stamp)).expect("save");
        blob_files(&vault)
            .into_iter()
            .find(|(path, _)| path.file_name().and_then(|n| n.to_str()) != Some("manifest"))
            .map(|(_, bytes)| bytes)
            .expect("note blob")
    };

    let moved = vault
        .move_note(&created.relative_path, Path::new("Archive"))
        .expect("move note");
    assert!(moved.starts_with("Archive"));

    let note_blob_after = blob_files(&vault)
        .into_iter()
        .find(|(path, _)| path.file_name().and_then(|n| n.to_str()) != Some("manifest"))
        .map(|(_, bytes)| bytes)
        .expect("note blob after move");
    assert_eq!(
        note_blob_before, note_blob_after,
        "a move must not re-encrypt the payload (AAD binds identity, not path)"
    );

    let notes = vault.scan_notes().expect("scan");
    assert_eq!(notes.len(), 1);
    assert!(notes[0].body.contains("STABLE BODY"));
    assert!(notes[0].relative_path.starts_with("Archive"));
}

// ---------------------------------------------------------------------------
// Atomic encrypted writes
// ---------------------------------------------------------------------------

#[test]
fn an_encrypted_save_is_stamp_checked_and_atomic() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("Vault");
    let vault = make_encrypted_vault(&root);
    let created = vault.create_note("Atomic", "Inbox").expect("create");
    let (mut note, stamp) = vault.load_note(&created.relative_path).expect("load");
    note.body = "first".into();
    let next = vault
        .save_note(&mut note, Some(&stamp))
        .expect("first save");

    // A stale stamp must be refused - the on-disk ciphertext is left intact.
    let blob_before = blob_files(&vault);
    let mut stale = note.clone();
    stale.body = "should not be written".into();
    assert!(
        vault.save_note(&mut stale, Some(&stamp)).is_err(),
        "a stale stamp must be rejected"
    );
    assert_eq!(
        blob_before,
        blob_files(&vault),
        "a rejected save must not have touched the ciphertext"
    );

    note.body = "second".into();
    vault
        .save_note(&mut note, Some(&next))
        .expect("second save");
    assert_eq!(
        vault.scan_notes().expect("scan")[0].body,
        "second".to_string()
    );
}

#[test]
fn a_leftover_temp_file_in_the_store_does_not_break_the_vault() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("Vault");
    {
        let vault = make_encrypted_vault(&root);
        vault.create_note("Survivor", "Inbox").expect("create");
        // Simulate a crash mid-write: a partial sibling temp file.
        let junk = store_dir(&vault).join("deadbeefdeadbeefdeadbeefdeadbeef.tmp");
        fs::write(&junk, b"partial ciphertext that never got renamed").expect("write junk");
    }

    let vault = Vault::open(&root).expect("reopen");
    vault
        .unlock(PASSWORD)
        .expect("unlock ignores the stray temp file");
    let notes = vault.scan_notes().expect("scan");
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].title, "Survivor");
}

// ---------------------------------------------------------------------------
// .snote inside an encrypted vault
// ---------------------------------------------------------------------------

#[test]
fn a_per_note_snote_inside_an_encrypted_vault_round_trips() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("Vault");
    let vault = make_encrypted_vault(&root);
    let created = vault
        .create_note("Double Wrapped", "Inbox")
        .expect("create");
    let (mut note, stamp) = vault.load_note(&created.relative_path).expect("load");
    note.body = "INNER-SNOTE-SECRET-3720".into();
    let stamp = vault.save_note(&mut note, Some(&stamp)).expect("save");

    let (_stamp, _session) = vault
        .encrypt_note(&mut note, Some(&stamp), "per note passphrase alpha")
        .expect("per-note encryption inside the vault");
    let snote_relative = note.relative_path.clone();
    assert_eq!(
        snote_relative.extension().and_then(|e| e.to_str()),
        Some("snote")
    );

    // Wrong per-note password is refused even though the vault is unlocked.
    assert!(
        vault
            .load_encrypted_note(&snote_relative, "the wrong per-note password")
            .is_err()
    );

    let (opened, _s, _sess) = vault
        .load_encrypted_note(&snote_relative, "per note passphrase alpha")
        .expect("correct per-note password");
    assert_eq!(opened.body, "INNER-SNOTE-SECRET-3720");

    // Survives a vault lock/unlock cycle.
    vault.lock();
    vault.unlock(PASSWORD).expect("re-unlock the vault");
    let (reopened, _s, _sess) = vault
        .load_encrypted_note(&snote_relative, "per note passphrase alpha")
        .expect("per-note note still opens after a vault lock cycle");
    assert_eq!(reopened.body, "INNER-SNOTE-SECRET-3720");

    // In the sidebar it shows as a locked note, never its title or body.
    let summaries = vault.scan_notes().expect("scan");
    let summary = summaries
        .iter()
        .find(|s| s.relative_path == snote_relative)
        .expect("the .snote note is listed");
    assert!(summary.locked);
    assert!(summary.body.is_empty());
    assert!(!summary.title.contains("Double Wrapped"));
}

// ---------------------------------------------------------------------------
// Notebooks / trash
// ---------------------------------------------------------------------------

#[test]
fn notebooks_and_trash_work_in_an_encrypted_vault() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("Vault");
    let vault = make_encrypted_vault(&root);

    vault.create_notebook("Projects").expect("create notebook");
    let names: HashSet<_> = vault
        .list_notebooks()
        .expect("list notebooks")
        .into_iter()
        .map(|entry| entry.relative_path.to_string_lossy().to_string())
        .collect();
    assert!(names.contains("Inbox"));
    assert!(names.contains("Projects"));

    let created = vault.create_note("Trash me", "Projects").expect("create");
    let entry = vault
        .move_to_trash(&created.relative_path)
        .expect("move to trash");
    assert_eq!(vault.scan_notes().expect("scan").len(), 0);

    let trash = vault.scan_trash().expect("scan trash");
    assert_eq!(trash.len(), 1);
    assert_eq!(trash[0].id, entry.id);

    vault.restore_from_trash(entry.id).expect("restore");
    assert_eq!(vault.scan_notes().expect("scan").len(), 1);

    let created2 = vault.create_note("Delete me", "Inbox").expect("create");
    let entry2 = vault
        .move_to_trash(&created2.relative_path)
        .expect("move to trash");
    vault
        .permanently_delete(entry2.id)
        .expect("permanent delete");
    assert_eq!(vault.scan_trash().expect("scan trash").len(), 0);
}

#[test]
fn a_locked_vault_rejects_every_mutation() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("Vault");
    {
        make_encrypted_vault(&root);
    }
    let vault = Vault::open(&root).expect("reopen locked");

    assert!(matches!(
        vault.create_note("nope", "Inbox"),
        Err(Error::VaultLocked)
    ));
    assert!(matches!(
        vault.create_notebook("nope"),
        Err(Error::VaultLocked)
    ));
}

// ---------------------------------------------------------------------------
// Regression: an encrypted vault must never grow plaintext storage
// ---------------------------------------------------------------------------

#[test]
fn a_full_lifecycle_leaves_no_plaintext_note_artifact_on_disk() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("Encrypted");
    let vault = make_encrypted_vault(&root);

    // initial empty-vault state
    assert_eq!(vault.scan_notes().expect("scan").len(), 0);
    assert_no_plaintext_artifacts(&root);

    // notebook create
    vault.create_notebook("Research").expect("create notebook");
    assert_no_plaintext_artifacts(&root);

    // new note
    let created = vault.create_note("Alpha", "Inbox").expect("create note");
    assert_no_plaintext_artifacts(&root);

    // edit + save (with tags)
    let (mut note, stamp) = vault.load_note(&created.relative_path).expect("load");
    note.body = "SEARCHABLE-BODY-ALPHA-01".into();
    note.metadata.tags = vec!["quarterly".into(), "draft".into()];
    let stamp = vault.save_note(&mut note, Some(&stamp)).expect("save");
    assert_no_plaintext_artifacts(&root);

    // rename (title commit -> filename change in the manifest)
    let stamp = vault
        .commit_title(&mut note, Some(&stamp), "Alpha Renamed")
        .expect("commit title");
    assert_no_plaintext_artifacts(&root);

    // move between notebooks
    vault
        .move_note(&note.relative_path, std::path::Path::new("Research"))
        .expect("move note");
    assert_no_plaintext_artifacts(&root);
    let moved = vault
        .scan_notes()
        .expect("scan")
        .into_iter()
        .find(|s| s.id == note.metadata.id)
        .expect("moved note listed");
    assert!(moved.relative_path.starts_with("Research"));
    assert!(moved.body.contains("SEARCHABLE-BODY-ALPHA-01"));
    assert_eq!(
        moved.tags,
        vec!["quarterly".to_string(), "draft".to_string()]
    );

    // per-note encryption (.snote inner layer) inside the encrypted vault
    let (mut note2, stamp2) = vault.load_note(&moved.relative_path).expect("reload moved");
    let _ = stamp; // superseded by the move
    let (_s, _sess) = vault
        .encrypt_note(&mut note2, Some(&stamp2), "a per note passphrase")
        .expect("per-note encrypt");
    assert_eq!(
        note2.relative_path.extension().and_then(|e| e.to_str()),
        Some("snote")
    );
    assert_no_plaintext_artifacts(&root);

    // trash + restore
    let second = vault.create_note("Beta", "Inbox").expect("create second");
    let entry = vault
        .move_to_trash(&second.relative_path)
        .expect("move to trash");
    assert_no_plaintext_artifacts(&root);
    vault.restore_from_trash(entry.id).expect("restore");
    assert_no_plaintext_artifacts(&root);

    // recovery draft
    let gamma = vault.create_note("Gamma", "Inbox").expect("create gamma");
    let (mut recover_me, _s) = vault.load_note(&gamma.relative_path).expect("load gamma");
    recover_me.body = "UNSAVED-RECOVERY-XYZ".into();
    vault.write_recovery(&recover_me).expect("write recovery");
    assert_no_plaintext_artifacts(&root);

    // lock / unlock cycle then re-check
    vault.lock();
    vault.unlock(PASSWORD).expect("re-unlock");
    assert_no_plaintext_artifacts(&root);

    // and after a fresh reopen
    drop(vault);
    let reopened = Vault::open(&root).expect("reopen");
    reopened.unlock(PASSWORD).expect("unlock reopened");
    reopened.scan_notes().expect("scan reopened");
    assert_no_plaintext_artifacts(&root);
}

#[test]
fn creating_an_encrypted_vault_refuses_a_folder_that_is_already_an_ordinary_vault() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("Existing");
    let ordinary = Vault::create(&root).expect("ordinary vault");
    ordinary
        .create_note("Plain Secret", "Inbox")
        .expect("a plaintext note");

    let before = all_files(&root);
    let error = Vault::create_encrypted(&root, PASSWORD)
        .expect_err("encrypting on top of an existing vault must be refused");
    assert!(matches!(error, Error::EncryptedVaultTargetNotEmpty(_)));

    // Nothing was written: no keyfile, vault.toml still ordinary, files unchanged.
    assert!(!root.join(".senatorial-notes/vault.keys").exists());
    assert!(!root.join(".senatorial-notes/store").exists());
    assert_eq!(
        before,
        all_files(&root),
        "a refused create must touch nothing"
    );
    let reopened = Vault::open(&root).expect("the ordinary vault still opens");
    assert_eq!(reopened.kind(), VaultKind::Ordinary);
    assert_eq!(reopened.scan_notes().expect("scan").len(), 1);
}

#[test]
fn creating_an_encrypted_vault_refuses_a_folder_containing_plaintext_notes() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("Dirty");
    fs::create_dir_all(root.join("Notes/Inbox")).unwrap();
    fs::write(
        root.join("Notes/Inbox/untitled--de3ed30f.md"),
        "---\ntitle: Untitled\n---\n",
    )
    .unwrap();

    let err = Vault::create_encrypted(&root, PASSWORD)
        .expect_err("a folder with plaintext notes must be refused");
    assert!(matches!(err, Error::EncryptedVaultTargetNotEmpty(_)));
    assert!(!root.join(".senatorial-notes").exists());
}

#[test]
fn finish_create_encrypted_also_guards_the_target() {
    // The GUI derives key material on a worker thread and calls
    // `finish_create_encrypted` directly, so the guard must live there too, not
    // only in `create_encrypted`.
    let dir = tempdir().unwrap();
    let root = dir.path().join("Dirty2");
    let ordinary = Vault::create(&root).expect("ordinary vault");
    ordinary.create_note("Leak me", "Inbox").expect("note");

    Vault::check_encrypted_target(&root).expect_err("check must reject");

    let vault_id = uuid::Uuid::new_v4();
    let (keyfile_bytes, keys) =
        senatorial_notes::crypto::vault::create_keyfile(vault_id, PASSWORD).expect("keyfile");
    let err = Vault::finish_create_encrypted(&root, vault_id, &keyfile_bytes, keys)
        .expect_err("finish_create_encrypted must refuse a non-empty target");
    assert!(matches!(err, Error::EncryptedVaultTargetNotEmpty(_)));
    assert!(!root.join(".senatorial-notes/vault.keys").exists());
}

#[test]
fn creating_an_encrypted_vault_in_a_fresh_or_absent_folder_is_allowed() {
    let dir = tempdir().unwrap();
    // absent
    let absent = dir.path().join("brand/new/path");
    Vault::create_encrypted(&absent, PASSWORD).expect("absent path is fine");
    // existing but empty
    let empty = dir.path().join("empty");
    fs::create_dir_all(&empty).unwrap();
    Vault::create_encrypted(&empty, PASSWORD).expect("empty folder is fine");
    // existing with an unrelated non-note file
    let with_readme = dir.path().join("with-readme");
    fs::create_dir_all(&with_readme).unwrap();
    fs::write(with_readme.join("README.txt"), "hello").unwrap();
    Vault::create_encrypted(&with_readme, PASSWORD).expect("an unrelated file is fine");
}
