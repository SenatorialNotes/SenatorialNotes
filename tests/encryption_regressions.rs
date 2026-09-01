//! Regression coverage for encrypted-note key management, focused on the
//! change-password release blocker: after a password change, the old password
//! must fail, the new password must succeed, the payload must be preserved, and
//! the session handed back to the caller must be the freshly verified one so a
//! later autosave cannot silently re-encrypt the note under the old key.

use std::fs;

use senatorial_notes::{Error, Vault};
use tempfile::tempdir;

const PW_A: &str = "first-strong-passphrase";
const PW_B: &str = "second-stronger-passphrase";

fn encrypted_note(vault: &Vault, title: &str, body: &str, password: &str) -> std::path::PathBuf {
    let created = vault.create_note(title, "Inbox").expect("create note");
    let (mut note, stamp) = vault.load_note(&created.relative_path).expect("load note");
    note.body = body.into();
    let stamp = vault.save_note(&mut note, Some(&stamp)).expect("save body");
    vault
        .encrypt_note(&mut note, Some(&stamp), password)
        .expect("encrypt note");
    note.relative_path
}

#[test]
fn change_password_makes_old_fail_and_new_succeed_and_preserves_payload() {
    let temporary = tempdir().expect("temp dir");
    let vault = Vault::create(temporary.path().join("Vault")).expect("vault");
    let relative = encrypted_note(&vault, "Rekey Target", "distinctive rekey body", PW_A);

    let (note, _stamp, _session) = vault
        .change_encrypted_password(&relative, PW_A, PW_B)
        .expect("password change should succeed");
    assert_eq!(note.metadata.title, "Rekey Target");
    assert_eq!(note.body, "distinctive rekey body");

    assert!(matches!(
        vault.load_encrypted_note(&relative, PW_A),
        Err(Error::DecryptionFailed)
    ));
    let (reopened, _stamp, _session) = vault
        .load_encrypted_note(&relative, PW_B)
        .expect("new password should unlock");
    assert_eq!(reopened.body, "distinctive rekey body");
}

#[test]
fn returned_session_saves_under_the_new_key_not_the_old_one() {
    // This is the exact acceptance failure: the UI kept a stale session after a
    // password change, and the next autosave re-encrypted the note under the
    // old key. The re-key now returns a verified session; saving with it must
    // keep the note readable only with the new password.
    let temporary = tempdir().expect("temp dir");
    let vault = Vault::create(temporary.path().join("Vault")).expect("vault");
    let relative = encrypted_note(&vault, "Session Target", "body v1", PW_A);

    let (mut note, stamp, session) = vault
        .change_encrypted_password(&relative, PW_A, PW_B)
        .expect("password change");

    note.body = "body v2 saved after re-key".into();
    vault
        .save_encrypted_note(&mut note, &session, Some(&stamp))
        .expect("autosave with the returned session should succeed");

    assert!(matches!(
        vault.load_encrypted_note(&relative, PW_A),
        Err(Error::DecryptionFailed)
    ));
    let (reopened, _stamp, _session) = vault
        .load_encrypted_note(&relative, PW_B)
        .expect("new password still unlocks after autosave");
    assert_eq!(reopened.body, "body v2 saved after re-key");
}

#[test]
fn change_password_after_editing_an_unlocked_note_keeps_the_edit() {
    let temporary = tempdir().expect("temp dir");
    let vault = Vault::create(temporary.path().join("Vault")).expect("vault");
    let relative = encrypted_note(&vault, "Edited Target", "original body", PW_A);

    let (mut note, stamp, session) = vault.load_encrypted_note(&relative, PW_A).expect("unlock");
    note.body = "edited before password change".into();
    vault
        .save_encrypted_note(&mut note, &session, Some(&stamp))
        .expect("persist edit");

    let (rekeyed, _stamp, _session) = vault
        .change_encrypted_password(&relative, PW_A, PW_B)
        .expect("password change after edit");
    assert_eq!(rekeyed.body, "edited before password change");

    let (reopened, _stamp, _session) = vault
        .load_encrypted_note(&relative, PW_B)
        .expect("new password unlocks the edited note");
    assert_eq!(reopened.body, "edited before password change");
}

#[test]
fn weak_new_password_is_rejected_with_no_side_effects() {
    let temporary = tempdir().expect("temp dir");
    let vault = Vault::create(temporary.path().join("Vault")).expect("vault");
    let relative = encrypted_note(&vault, "Weak Guard", "protected body", PW_A);
    let path = vault.note_path(&relative).expect("safe path");

    let before_bytes = fs::read(&path).expect("read container");
    let before_modified = fs::metadata(&path)
        .and_then(|m| m.modified())
        .expect("mtime");

    let error = vault
        .change_encrypted_password(&relative, PW_A, "short")
        .expect_err("a short new password must be rejected");
    assert!(matches!(error, Error::WeakPassword(_)));

    assert_eq!(fs::read(&path).expect("unchanged container"), before_bytes);
    assert_eq!(
        fs::metadata(&path)
            .and_then(|m| m.modified())
            .expect("mtime"),
        before_modified
    );
    // The old password still works because nothing was written.
    vault
        .load_encrypted_note(&relative, PW_A)
        .expect("old password still unlocks the untouched note");
}

#[test]
fn wrong_current_password_is_rejected_with_no_side_effects() {
    let temporary = tempdir().expect("temp dir");
    let vault = Vault::create(temporary.path().join("Vault")).expect("vault");
    let relative = encrypted_note(&vault, "Auth Guard", "protected body", PW_A);
    let path = vault.note_path(&relative).expect("safe path");
    let before_bytes = fs::read(&path).expect("read container");

    let error = vault
        .change_encrypted_password(&relative, "not-the-current-password", PW_B)
        .expect_err("a wrong current password must be rejected");
    assert!(matches!(error, Error::DecryptionFailed));
    assert_eq!(fs::read(&path).expect("unchanged container"), before_bytes);
}

#[test]
fn tampering_after_rekey_still_fails_closed() {
    let temporary = tempdir().expect("temp dir");
    let vault = Vault::create(temporary.path().join("Vault")).expect("vault");
    let relative = encrypted_note(&vault, "Tamper Guard", "protected body", PW_A);
    vault
        .change_encrypted_password(&relative, PW_A, PW_B)
        .expect("password change");

    let path = vault.note_path(&relative).expect("safe path");
    let mut bytes = fs::read(&path).expect("read container");
    *bytes.last_mut().expect("tag byte") ^= 0x80;
    fs::write(&path, bytes).expect("write tampered container");

    assert!(matches!(
        vault.load_encrypted_note(&relative, PW_B),
        Err(Error::DecryptionFailed)
    ));
}

#[test]
fn encrypting_with_a_weak_password_is_rejected_before_any_write() {
    let temporary = tempdir().expect("temp dir");
    let vault = Vault::create(temporary.path().join("Vault")).expect("vault");
    let created = vault.create_note("Weak Encrypt", "Inbox").expect("create");
    let (mut note, stamp) = vault.load_note(&created.relative_path).expect("load");
    note.body = "still plaintext".into();
    let stamp = vault.save_note(&mut note, Some(&stamp)).expect("save");

    let error = vault
        .encrypt_note(&mut note, Some(&stamp), "tiny")
        .expect_err("weak password must be rejected");
    assert!(matches!(error, Error::WeakPassword(_)));

    // The note is untouched and still an ordinary Markdown file.
    assert_eq!(
        note.relative_path
            .extension()
            .and_then(|value| value.to_str()),
        Some("md")
    );
    let markdown = fs::read_to_string(vault.note_path(&note.relative_path).expect("path"))
        .expect("still readable markdown");
    assert!(markdown.contains("still plaintext"));
}
