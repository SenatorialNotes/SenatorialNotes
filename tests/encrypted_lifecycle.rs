//! Stage E: encrypted-vault lifecycle stress + hardening, driven through the
//! public [`Vault`] API exactly as the GUI would.
//!
//! Focus: repeated lock/unlock cycles interleaved with every kind of mutation,
//! crash-consistency of the blob/manifest write order, behaviour at scale, a
//! password change while the vault is busy, and the full nested-`.snote`
//! lifecycle.

use std::fs;
use std::path::{Path, PathBuf};

use senatorial_notes::{Error, Vault};
use tempfile::tempdir;

const PW: &str = "correct horse battery staple";
const PW2: &str = "an entirely different long passphrase";
const NOTE_PW: &str = "inner note passphrase";

fn secure(root: &Path) -> Vault {
    let v = Vault::create_encrypted(root, PW).unwrap();
    v.unlock(PW).unwrap();
    v
}

fn store_files(root: &Path) -> Vec<PathBuf> {
    let store = root.join(".senatorial-notes/store");
    let mut out: Vec<_> = fs::read_dir(&store)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    out.sort();
    out
}

fn assert_no_plaintext(root: &Path) {
    for d in ["Notes", "Trash", "Attachments"] {
        assert!(!root.join(d).exists(), "grew a plaintext {d}/");
    }
    fn walk(dir: &Path, state: &Path) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, state);
            } else {
                assert!(
                    path.starts_with(state),
                    "file outside state dir: {}",
                    path.display()
                );
                let ext = path.extension().and_then(|e| e.to_str());
                assert!(
                    ext != Some("md") && ext != Some("snote"),
                    "plaintext: {}",
                    path.display()
                );
            }
        }
    }
    walk(root, &root.join(".senatorial-notes"));
}

#[test]
fn many_lock_unlock_cycles_interleaved_with_every_mutation_stay_consistent() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("Secure");
    let vault = secure(&root);
    vault.create_notebook("Work").unwrap();

    for round in 0..8 {
        let mut note = vault
            .create_note(&format!("Note {round}"), "Inbox")
            .unwrap();
        note.body = format!("body {round}");
        vault.save_note(&mut note, None).unwrap();
        vault
            .commit_title(&mut note, None, &format!("Renamed {round}"))
            .unwrap();
        let moved = vault.move_note(&note.relative_path, "Work").unwrap();

        // Trash + restore straddling a lock/unlock on some rounds.
        let restore = if round % 3 == 0 {
            Some(vault.move_to_trash(&moved).unwrap().id)
        } else {
            None
        };

        vault.lock();
        assert!(matches!(vault.scan_notes(), Err(Error::VaultLocked)));
        vault.unlock(PW).unwrap();

        if let Some(id) = restore {
            vault.restore_from_trash(id).unwrap();
        }

        let summaries = vault.scan_notes().unwrap();
        assert_eq!(summaries.len(), round + 1);
        assert_no_plaintext(&root);

        // No blob is referenced by nothing: reconcile on unlock never leaves a
        // stray outside orphans/.
        let orphans = root.join(".senatorial-notes/store/orphans");
        assert!(!orphans.exists() || fs::read_dir(&orphans).unwrap().count() == 0);
    }
}

#[test]
fn a_crash_between_blob_and_manifest_loses_no_note_and_quarantines_the_orphan() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("Secure");
    let vault = secure(&root);

    let mut keep = vault.create_note("Keep", "Inbox").unwrap();
    keep.body = "important".into();
    vault.save_note(&mut keep, None).unwrap();

    let before: Vec<_> = store_files(&root);
    // Simulate a crash after a content blob was fsynced but before the manifest
    // was re-sealed: an extra store file the manifest does not reference.
    let orphan = root
        .join(".senatorial-notes/store")
        .join("cccccccccccccccccccccccccccccccc");
    fs::copy(&before[0], &orphan).unwrap();

    vault.lock();
    vault.unlock(PW).unwrap();

    // Every real note survives.
    let summaries = vault.scan_notes().unwrap();
    assert_eq!(summaries.len(), 1);
    let (note, _) = vault.load_note(&summaries[0].relative_path).unwrap();
    assert_eq!(note.body, "important");

    // The orphan was MOVED to orphans/, never deleted.
    assert!(!orphan.exists());
    let orphans = root.join(".senatorial-notes/store/orphans");
    assert_eq!(fs::read_dir(&orphans).unwrap().count(), 1);
}

#[test]
fn a_password_change_while_the_vault_is_full_rewraps_only_the_keyfile() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("Secure");
    let vault = secure(&root);
    for i in 0..20 {
        let mut n = vault.create_note(&format!("N{i}"), "Inbox").unwrap();
        n.body = "x".repeat(2000);
        vault.save_note(&mut n, None).unwrap();
    }
    let blobs_before: Vec<_> = store_files(&root)
        .into_iter()
        .map(|p| (p.clone(), fs::read(&p).unwrap()))
        .collect();

    vault.change_vault_password(PW, PW2).unwrap();

    // No blob re-encrypted.
    let blobs_after: Vec<_> = store_files(&root)
        .into_iter()
        .map(|p| (p.clone(), fs::read(&p).unwrap()))
        .collect();
    assert_eq!(
        blobs_before, blobs_after,
        "a password change re-encrypts no blob"
    );

    // Session still valid (VMK unchanged); relocking needs the new password.
    assert_eq!(vault.scan_notes().unwrap().len(), 20);
    vault.lock();
    assert!(matches!(vault.unlock(PW), Err(Error::DecryptionFailed)));
    vault.unlock(PW2).unwrap();
    assert_eq!(vault.scan_notes().unwrap().len(), 20);
}

#[test]
fn scale_hundreds_of_notes_scan_then_lock_drops_everything() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("Secure");
    let vault = secure(&root);
    for i in 0..120 {
        let mut n = vault.create_note(&format!("Note {i}"), "Inbox").unwrap();
        n.body = format!("content number {i} ").repeat(20);
        vault.save_note(&mut n, None).unwrap();
    }
    assert_eq!(vault.scan_notes().unwrap().len(), 120);
    assert_no_plaintext(&root);

    vault.lock();
    assert!(matches!(vault.scan_notes(), Err(Error::VaultLocked)));
    assert!(matches!(
        vault.load_note(Path::new("Inbox/x.md")),
        Err(Error::VaultLocked)
    ));
}

#[test]
fn the_full_nested_snote_lifecycle_never_leaves_plaintext() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("Secure");
    let vault = secure(&root);

    let mut note = vault.create_note("Diary", "Inbox").unwrap();
    note.body = "top secret".into();
    vault.save_note(&mut note, None).unwrap();

    // Encrypt the note (inner .snote), then lock/unlock the whole vault.
    let (_, _session) = vault.encrypt_note(&mut note, None, NOTE_PW).unwrap();
    vault.lock();
    vault.unlock(PW).unwrap();
    assert_no_plaintext(&root);

    // Listed as a locked note; opening needs the note password.
    let summaries = vault.scan_notes().unwrap();
    assert!(summaries[0].encrypted && summaries[0].locked);
    assert!(matches!(
        vault.load_encrypted_note(&summaries[0].relative_path, "wrong"),
        Err(Error::DecryptionFailed)
    ));
    let (opened, _stamp, session) = vault
        .load_encrypted_note(&summaries[0].relative_path, NOTE_PW)
        .unwrap();
    assert_eq!(opened.body, "top secret");

    // Edit + save (re-encrypts inner then outer), change the note password,
    // then remove per-note encryption.
    let mut opened = opened;
    opened.body = "still secret".into();
    vault
        .save_encrypted_note(&mut opened, &session, None)
        .unwrap();
    let snote_path = opened.relative_path.clone();
    let (rekeyed, _stamp, _new_session) = vault
        .change_encrypted_password(&snote_path, NOTE_PW, "brand new note passphrase")
        .unwrap();
    let rekeyed_path = rekeyed.relative_path.clone();
    let (plain, _stamp) = vault
        .remove_encryption(&rekeyed_path, "brand new note passphrase")
        .unwrap();
    assert_no_plaintext(&root);
    assert_eq!(plain.body, "still secret");
    assert_eq!(
        plain.relative_path.extension().and_then(|e| e.to_str()),
        Some("md")
    );
}

#[test]
fn r18_quarantine_then_export_carries_only_the_real_encrypted_notes() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("Secure");
    {
        let vault = secure(&root);
        let mut n = vault.create_note("Real", "Inbox").unwrap();
        n.body = "genuine".into();
        vault.save_note(&mut n, None).unwrap();
        vault.lock();
    }

    // An old binary drops a plaintext note into the vault root.
    fs::create_dir_all(root.join("Notes/Inbox")).unwrap();
    fs::write(
        root.join("Notes/Inbox/intruder.md"),
        b"---\nid: 00000000-0000-0000-0000-000000000000\ntitle: Intruder\n---\nnope\n",
    )
    .unwrap();

    let flagged = Vault::open(&root).unwrap();
    let report = flagged.quarantine_plaintext().unwrap();
    assert_eq!(report.file_count, 1);

    let clean = Vault::open(&root).unwrap();
    clean.unlock(PW).unwrap();

    let dest = dir.path().join("Exported");
    let export = senatorial_notes::vault_export::export_secure_vault_to_standard(
        senatorial_notes::vault_export::ExportParams {
            source_root: clean.root().to_path_buf(),
            source_state_dir: clean.state_dir(),
            vault_id: clean.vault_id(),
            keyfile_bytes: fs::read(clean.state_dir().join("vault.keys")).unwrap(),
            password: zeroize::Zeroizing::new(PW.to_string()),
            destination: dest.clone(),
        },
        senatorial_notes::vault_export::ExportProgress::new(),
    )
    .unwrap();
    assert_eq!(export.notes, 1);

    let exported = Vault::open(&dest).unwrap();
    let names: Vec<_> = exported
        .scan_notes()
        .unwrap()
        .into_iter()
        .map(|s| s.title)
        .collect();
    assert_eq!(names, vec!["Real"]);
    assert!(!dest.join("Notes/Inbox/intruder.md").exists());
}
