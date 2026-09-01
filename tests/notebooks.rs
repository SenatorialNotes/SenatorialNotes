//! Storage-layer regression coverage for v0.2 notebook and tag operations:
//! create/rename/delete safety, nested notebooks, note moves (including
//! encrypted notes, where a move must never require re-encryption), and tag
//! normalization. UI-layer runtime-state behavior (active note, caches,
//! selection, watcher baseline) is covered separately once the UI wiring
//! exists.

use std::fs;
use std::os::unix::fs::symlink;

use senatorial_notes::model::NoteMetadata;
use senatorial_notes::{Error, Vault};
use tempfile::tempdir;

#[test]
fn list_notebooks_reports_nested_notebooks_with_direct_counts() {
    let temporary = tempdir().expect("temporary directory");
    let vault = Vault::create(temporary.path().join("Vault")).expect("vault should be created");
    vault
        .create_notebook("Work/Projects")
        .expect("nested notebook should be created");
    vault
        .create_note("Top level", "Work")
        .expect("note should be created");
    vault
        .create_note("Nested", "Work/Projects")
        .expect("note should be created");

    let notebooks = vault.list_notebooks().expect("notebooks should list");
    let inbox = notebooks
        .iter()
        .find(|entry| entry.relative_path == std::path::Path::new("Inbox"))
        .expect("Inbox is always present");
    assert_eq!(inbox.direct_note_count, 0);
    let work = notebooks
        .iter()
        .find(|entry| entry.relative_path == std::path::Path::new("Work"))
        .expect("Work notebook should be listed");
    assert_eq!(work.direct_note_count, 1, "direct count excludes children");
    let projects = notebooks
        .iter()
        .find(|entry| entry.relative_path == std::path::Path::new("Work/Projects"))
        .expect("nested notebook should be listed");
    assert_eq!(projects.direct_note_count, 1);
}

#[test]
fn inbox_cannot_be_renamed_or_deleted() {
    let temporary = tempdir().expect("temporary directory");
    let vault = Vault::create(temporary.path().join("Vault")).expect("vault should be created");

    let rename_error = vault
        .rename_notebook(std::path::Path::new("Inbox"), "Renamed")
        .expect_err("Inbox rename must be refused");
    assert!(matches!(rename_error, Error::ReservedNotebook { .. }));

    let delete_error = vault
        .delete_notebook(std::path::Path::new("Inbox"))
        .expect_err("Inbox delete must be refused");
    assert!(matches!(delete_error, Error::ReservedNotebook { .. }));

    // A nested notebook under Inbox is not reserved.
    vault
        .create_notebook("Inbox/Drafts")
        .expect("nested notebook under Inbox should be created");
    vault
        .rename_notebook(std::path::Path::new("Inbox/Drafts"), "Scratch")
        .expect("non-reserved nested notebook should rename");
}

#[test]
fn renaming_a_notebook_refuses_a_colliding_sibling_name() {
    let temporary = tempdir().expect("temporary directory");
    let vault = Vault::create(temporary.path().join("Vault")).expect("vault should be created");
    vault
        .create_notebook("Personal")
        .expect("notebook should be created");
    vault
        .create_notebook("Work")
        .expect("notebook should be created");

    let error = vault
        .rename_notebook(std::path::Path::new("Personal"), "Work")
        .expect_err("colliding rename must be refused");
    assert!(matches!(error, Error::AlreadyExists(_)));
}

#[test]
fn empty_notebook_deletes_successfully_including_empty_nested_subtree() {
    let temporary = tempdir().expect("temporary directory");
    let vault = Vault::create(temporary.path().join("Vault")).expect("vault should be created");
    vault
        .create_notebook("Archive/Old/Empty")
        .expect("nested notebook should be created");

    vault
        .delete_notebook(std::path::Path::new("Archive"))
        .expect("empty nested notebook subtree should delete");
    assert!(!vault.notes_dir().join("Archive").exists());
}

#[test]
fn non_empty_notebook_refuses_deletion_and_names_the_note_count() {
    let temporary = tempdir().expect("temporary directory");
    let vault = Vault::create(temporary.path().join("Vault")).expect("vault should be created");
    vault
        .create_notebook("Work/Projects")
        .expect("notebook should be created");
    vault
        .create_note("Deep note", "Work/Projects")
        .expect("note should be created");

    let error = vault
        .delete_notebook(std::path::Path::new("Work"))
        .expect_err("non-empty notebook must refuse deletion");
    match error {
        Error::NotebookNotEmpty {
            relative_path,
            note_count,
        } => {
            assert_eq!(relative_path, std::path::Path::new("Work"));
            assert_eq!(note_count, 1);
        }
        other => panic!("expected NotebookNotEmpty, got {other:?}"),
    }
    assert!(
        vault.notes_dir().join("Work/Projects").exists(),
        "nothing must be deleted when the operation is refused"
    );
}

#[test]
fn notebook_with_an_unmanaged_file_refuses_deletion_without_touching_it() {
    let temporary = tempdir().expect("temporary directory");
    let vault = Vault::create(temporary.path().join("Vault")).expect("vault should be created");
    vault
        .create_notebook("Archive")
        .expect("notebook should be created");
    let stray = vault.notes_dir().join("Archive/notes.bak");
    fs::write(&stray, b"not managed by SenatorialNotes").expect("fixture file should write");

    let error = vault
        .delete_notebook(std::path::Path::new("Archive"))
        .expect_err("a notebook with unrecognised content must refuse deletion");
    assert!(matches!(error, Error::NotebookHasUnmanagedContent { .. }));
    assert!(stray.exists(), "the unmanaged file must never be touched");
}

#[test]
fn notebook_with_a_symlink_refuses_deletion_without_touching_it() {
    let temporary = tempdir().expect("temporary directory");
    let vault = Vault::create(temporary.path().join("Vault")).expect("vault should be created");
    vault
        .create_notebook("Archive")
        .expect("notebook should be created");
    let target = temporary.path().join("outside.txt");
    fs::write(&target, b"outside the vault").expect("fixture file should write");
    let link = vault.notes_dir().join("Archive/link.txt");
    symlink(&target, &link).expect("fixture symlink should be created");

    let error = vault
        .delete_notebook(std::path::Path::new("Archive"))
        .expect_err("a notebook containing a symlink must refuse deletion");
    assert!(matches!(error, Error::NotebookHasUnmanagedContent { .. }));
    assert!(link.exists(), "the symlink must never be touched");
    assert!(target.exists(), "the symlink target must never be touched");
}

#[test]
fn moving_a_plaintext_note_preserves_uuid_body_and_updated_at() {
    let temporary = tempdir().expect("temporary directory");
    let vault = Vault::create(temporary.path().join("Vault")).expect("vault should be created");
    vault
        .create_notebook("Work")
        .expect("notebook should be created");
    let created = vault
        .create_note("Moving day", "Inbox")
        .expect("note should be created");
    let (mut note, stamp) = vault
        .load_note(&created.relative_path)
        .expect("note should load");
    note.body = "content that must survive the move".into();
    vault
        .save_note(&mut note, Some(&stamp))
        .expect("note should save");
    let before_move = vault
        .load_note(&created.relative_path)
        .expect("note should reload")
        .0;

    let next_relative = vault
        .move_note(&created.relative_path, "Work")
        .expect("move should succeed");
    assert_eq!(
        next_relative,
        std::path::Path::new("Work").join(
            created
                .relative_path
                .file_name()
                .expect("filename should exist"),
        )
    );
    assert!(!vault.note_path(&created.relative_path).unwrap().exists());

    let (moved, _stamp) = vault.load_note(&next_relative).expect("moved note loads");
    assert_eq!(moved.metadata.id, before_move.metadata.id);
    assert_eq!(moved.body, before_move.body);
    assert_eq!(moved.metadata.updated_at, before_move.metadata.updated_at);
}

#[test]
fn moving_a_note_into_a_notebook_that_does_not_exist_yet_creates_it() {
    let temporary = tempdir().expect("temporary directory");
    let vault = Vault::create(temporary.path().join("Vault")).expect("vault should be created");
    let created = vault
        .create_note("New destination", "Inbox")
        .expect("note should be created");

    let next_relative = vault
        .move_note(&created.relative_path, "Personal/Finance")
        .expect("move should create the destination notebook");
    assert!(vault.notes_dir().join("Personal/Finance").is_dir());
    assert!(vault.note_path(&next_relative).unwrap().is_file());
}

#[test]
fn moving_a_note_onto_a_colliding_destination_filename_refuses_without_overwriting() {
    let temporary = tempdir().expect("temporary directory");
    let vault = Vault::create(temporary.path().join("Vault")).expect("vault should be created");
    vault
        .create_notebook("Work")
        .expect("notebook should be created");
    let moving = vault
        .create_note("Duplicate title", "Inbox")
        .expect("note should be created");
    // Fabricate a real filename collision at the destination: same title
    // slug and the *moving* note's own short UUID, exactly the filename
    // `move_note` will compute as its destination.
    let destination_name = moving
        .relative_path
        .file_name()
        .expect("filename should exist")
        .to_owned();
    let colliding_path = vault.notes_dir().join("Work").join(&destination_name);
    fs::write(&colliding_path, b"pre-existing file at the destination")
        .expect("fixture file should write");

    let error = vault
        .move_note(&moving.relative_path, "Work")
        .expect_err("a real destination collision must be refused");
    assert!(matches!(error, Error::AlreadyExists(_)));
    assert_eq!(
        fs::read_to_string(&colliding_path).expect("destination should be unchanged"),
        "pre-existing file at the destination",
        "the existing file at the destination must never be silently overwritten"
    );
    assert!(
        vault.note_path(&moving.relative_path).unwrap().exists(),
        "the note being moved must remain at its original location on refusal"
    );
}

#[test]
fn moving_a_note_to_its_current_notebook_is_a_harmless_no_op() {
    let temporary = tempdir().expect("temporary directory");
    let vault = Vault::create(temporary.path().join("Vault")).expect("vault should be created");
    let created = vault
        .create_note("Already home", "Inbox")
        .expect("note should be created");

    let next_relative = vault
        .move_note(&created.relative_path, "Inbox")
        .expect("moving into the same notebook should succeed as a no-op");
    assert_eq!(next_relative, created.relative_path);
}

#[test]
fn moving_an_encrypted_note_preserves_decryption_under_the_same_password() {
    // This is the empirical proof for the architecture finding that the
    // `.snote` container's authenticated header does not include the file
    // path: a plain filesystem move must not require re-encryption, and the
    // moved file must still decrypt correctly with the original password.
    let temporary = tempdir().expect("temporary directory");
    let vault = Vault::create(temporary.path().join("Vault")).expect("vault should be created");
    vault
        .create_notebook("Personal")
        .expect("notebook should be created");
    let mut note = vault
        .create_note("Secret plan", "Inbox")
        .expect("note should be created");
    note.body = "the launch codes are hidden here".into();
    let password = "correct horse battery staple";
    let (_stamp, _session) = vault
        .encrypt_note(&mut note, None, password)
        .expect("note should encrypt");
    let encrypted_relative = note.relative_path.clone();

    let next_relative = vault
        .move_note(&encrypted_relative, "Personal")
        .expect("encrypted note should move like any other file");
    assert_ne!(next_relative, encrypted_relative);

    let (decrypted, _stamp, _session) = vault
        .load_encrypted_note(&next_relative, password)
        .expect("moved encrypted note should still decrypt with the same password");
    assert_eq!(decrypted.metadata.id, note.metadata.id);
    assert_eq!(decrypted.body, "the launch codes are hidden here");
}

#[test]
fn tag_helpers_dedupe_case_insensitively_and_keep_first_casing() {
    let mut metadata = NoteMetadata::new("Note with tags");
    assert!(metadata.add_tag("Errands"));
    assert!(!metadata.add_tag("errands"), "case-insensitive duplicate");
    assert!(!metadata.add_tag("ERRANDS"), "case-insensitive duplicate");
    assert!(!metadata.add_tag("  "), "whitespace-only tag is rejected");
    assert_eq!(metadata.tags, vec!["Errands".to_string()]);

    assert!(metadata.add_tag("home"));
    assert_eq!(
        metadata.tags,
        vec!["Errands".to_string(), "home".to_string()]
    );

    assert!(metadata.remove_tag("ERRANDS"));
    assert_eq!(metadata.tags, vec!["home".to_string()]);
    assert!(!metadata.remove_tag("not-present"));
}
