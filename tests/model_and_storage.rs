use std::fs;
use std::path::Path;

use senatorial_notes::model::Note;
use senatorial_notes::paths::{note_filename, sanitize_title, validate_notebook_path};
use senatorial_notes::storage::atomic::atomic_write;
use senatorial_notes::{Error, Vault};
use tempfile::tempdir;

#[test]
fn parses_front_matter_and_preserves_unknown_fields() {
    let markdown = r#"---
id: "550e8400-e29b-41d4-a716-446655440000"
title: "Example note"
created_at: "2026-08-25T17:30:00Z"
updated_at: "2026-08-25T17:45:00Z"
tags:
  - example
pinned: false
future_field: "keep me"
---
Body text.
"#;

    let note = Note::parse(markdown, "Inbox/example.md".into()).expect("note should parse");
    assert_eq!(note.metadata.title, "Example note");
    assert_eq!(note.body, "Body text.\n");
    assert_eq!(
        note.metadata.unknown.get("future_field"),
        Some(&serde_yaml::Value::String("keep me".into()))
    );

    let round_trip = note.to_markdown().expect("note should serialize");
    let reparsed =
        Note::parse(&round_trip, "Inbox/example.md".into()).expect("round trip should parse");
    assert_eq!(reparsed.metadata.unknown, note.metadata.unknown);
    assert_eq!(reparsed.metadata.id, note.metadata.id);
}

#[test]
fn rejects_missing_front_matter() {
    let error = Note::parse("plain text", "Inbox/plain.md".into())
        .expect_err("missing metadata must be rejected");
    assert!(matches!(error, Error::InvalidFrontMatter(_)));
}

#[test]
fn sanitizes_titles_and_keeps_uuid_stable_in_filename() {
    let id = uuid::Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000")
        .expect("fixture UUID is valid");
    assert_eq!(sanitize_title("  Quarterly / Plan?!  "), "quarterly-plan");
    assert_eq!(sanitize_title("///"), "untitled");
    assert_eq!(
        note_filename("Quarterly Plan", id),
        "quarterly-plan--550e8400.md"
    );
}

#[test]
fn rejects_path_traversal_and_absolute_notebooks() {
    for invalid in ["../Secrets", "/tmp/Notes", "Inbox/../../Secrets", "."] {
        assert!(
            validate_notebook_path(Path::new(invalid)).is_err(),
            "{invalid} must be rejected"
        );
    }
    assert_eq!(
        validate_notebook_path(Path::new("Work/Meetings")).expect("nested notebook is valid"),
        Path::new("Work/Meetings")
    );
}

#[test]
fn creates_vault_notebook_and_note_as_markdown() {
    let temporary = tempdir().expect("temporary directory");
    let vault = Vault::create(temporary.path().join("My Notes")).expect("vault should be created");
    vault
        .create_notebook("Work/Meetings")
        .expect("notebook should be created");
    let note = vault
        .create_note("Planning", "Work/Meetings")
        .expect("note should be created");

    let path = vault
        .note_path(&note.relative_path)
        .expect("note path should be safe");
    let markdown = fs::read_to_string(path).expect("note should be readable");
    assert!(markdown.starts_with("---\n"));
    assert!(markdown.contains("title: Planning"));
    assert_eq!(vault.scan_notes().expect("scan should work").len(), 1);
}

#[cfg(unix)]
#[test]
fn new_vault_directories_and_notes_are_private() {
    use std::os::unix::fs::PermissionsExt;

    let temporary = tempdir().expect("temporary directory");
    let vault =
        Vault::create(temporary.path().join("Private Vault")).expect("vault should be created");
    let note = vault
        .create_note("Private", "Inbox")
        .expect("note should be created");
    let note_path = vault
        .note_path(&note.relative_path)
        .expect("note path should be safe");

    assert_eq!(
        fs::metadata(vault.root())
            .expect("vault metadata")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(note_path)
            .expect("note metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn atomically_saves_and_detects_external_modification() {
    let temporary = tempdir().expect("temporary directory");
    let vault = Vault::create(temporary.path().join("Vault")).expect("vault should be created");
    let created = vault
        .create_note("Atomic", "Inbox")
        .expect("note should be created");
    let (mut note, stamp) = vault
        .load_note(&created.relative_path)
        .expect("note should load");
    note.body = "first save".into();
    let next_stamp = vault
        .save_note(&mut note, Some(&stamp))
        .expect("unchanged file should save");

    let path = vault
        .note_path(&note.relative_path)
        .expect("note path should be safe");
    fs::write(&path, "externally replaced with different-sized content")
        .expect("fixture external edit should work");
    note.body = "editor version".into();
    let error = vault
        .save_note(&mut note, Some(&next_stamp))
        .expect_err("external edit must prevent overwrite");
    assert!(matches!(error, Error::ExternalModification(_)));
    assert_eq!(
        fs::read_to_string(path).expect("external version remains"),
        "externally replaced with different-sized content"
    );
}

#[test]
fn file_stamp_metadata_check_validates_a_cached_document_cheaply() {
    // Backs the in-memory plaintext document cache: switching back to a clean
    // note reuses the parsed copy only while this cheap stat-only check holds.
    let temporary = tempdir().expect("temporary directory");
    let vault = Vault::create(temporary.path().join("Vault")).expect("vault should be created");
    let created = vault.create_note("Cached", "Inbox").expect("note created");
    let (mut note, stamp) = vault.load_note(&created.relative_path).expect("note loads");
    let path = vault.note_path(&note.relative_path).expect("safe path");

    assert!(
        stamp.metadata_matches(&path),
        "fresh stamp matches its file"
    );

    note.body = "edited body".into();
    let new_stamp = vault
        .save_note(&mut note, Some(&stamp))
        .expect("save succeeds");
    assert!(
        !stamp.metadata_matches(&path),
        "the old stamp must no longer match after a save"
    );
    assert!(
        new_stamp.metadata_matches(&path),
        "the returned stamp matches the written file"
    );

    fs::write(&path, b"externally shortened").expect("external edit");
    assert!(
        !new_stamp.metadata_matches(&path),
        "an external write of a different length invalidates the cache entry"
    );
    assert!(!stamp.metadata_matches(Path::new("/does/not/exist")));
}

#[test]
fn recovery_copy_is_local_and_removed_after_successful_save() {
    let temporary = tempdir().expect("temporary directory");
    let vault = Vault::create(temporary.path().join("Vault")).expect("vault should be created");
    let mut note = vault
        .create_note("Recoverable", "Inbox")
        .expect("note should be created");
    note.body = "unsaved editor contents".into();
    let recovery = vault
        .write_recovery(&note)
        .expect("recovery should be written");
    assert!(recovery.starts_with(vault.root()));
    assert!(recovery.exists());

    let (_loaded, stamp) = vault
        .load_note(&note.relative_path)
        .expect("note should load");
    vault
        .save_note(&mut note, Some(&stamp))
        .expect("save should succeed");
    assert!(!recovery.exists());
}

#[test]
fn atomic_write_preserves_existing_data_when_replacement_cannot_complete() {
    let temporary = tempdir().expect("temporary directory");
    let target = temporary.path().join("note.md");
    atomic_write(&target, b"original").expect("initial write should succeed");

    let impossible_target = target.join("child.md");
    let error = atomic_write(&impossible_target, b"replacement")
        .expect_err("a file cannot be used as a parent directory");
    assert!(matches!(error, Error::Io { .. }));
    assert_eq!(fs::read(&target).expect("original remains"), b"original");
}

#[cfg(unix)]
#[test]
fn rejects_symbolic_links_inside_managed_note_paths() {
    use std::os::unix::fs::symlink;

    let temporary = tempdir().expect("temporary directory");
    let outside = temporary.path().join("Outside");
    fs::create_dir(&outside).expect("outside fixture directory");
    let vault = Vault::create(temporary.path().join("Vault")).expect("vault should be created");
    symlink(&outside, vault.notes_dir().join("Linked")).expect("fixture symlink");

    let error = vault
        .create_note("Must stay inside", "Linked")
        .expect_err("managed paths must reject symlink components");
    assert!(matches!(error, Error::InvalidPath(_)));
    assert!(
        fs::read_dir(outside)
            .expect("outside remains readable")
            .next()
            .is_none()
    );
}

#[test]
fn runtime_manifest_has_no_http_client_dependencies() {
    let manifest = include_str!("../Cargo.toml");
    for forbidden in ["reqwest", "ureq", "hyper", "curl"] {
        assert!(
            !manifest.lines().any(|line| {
                let trimmed = line.trim_start();
                trimmed.starts_with(&format!("{forbidden} ="))
                    || trimmed.starts_with(&format!("[dependencies.{forbidden}]"))
            }),
            "runtime dependency {forbidden} is forbidden"
        );
    }
}

#[test]
fn body_autosave_keeps_title_draft_and_path_stable_until_title_commit() {
    let temporary = tempdir().expect("temporary directory");
    let vault = Vault::create(temporary.path().join("Vault")).expect("vault should be created");
    let created = vault
        .create_note("Original", "Inbox")
        .expect("note should be created");
    let original_path = created.relative_path.clone();
    let original_id = created.metadata.id;
    let (mut note, stamp) = vault.load_note(&original_path).expect("note should load");

    // The UI keeps "Draft title" in its entry/draft state. A body autosave
    // updates only the model body and cannot observe or commit that draft.
    let title_draft = "Draft title";
    note.body = "body typed while title editing".into();
    let next_stamp = vault
        .save_note(&mut note, Some(&stamp))
        .expect("body autosave should succeed");
    assert_eq!(note.relative_path, original_path);
    assert_eq!(note.metadata.title, "Original");
    assert_eq!(note.metadata.id, original_id);

    let final_stamp = vault
        .commit_title(&mut note, Some(&next_stamp), title_draft)
        .expect("explicit title commit should succeed");
    assert_eq!(note.metadata.title, title_draft);
    assert_eq!(note.metadata.id, original_id);
    let expected_filename = note_filename(title_draft, original_id);
    assert_eq!(
        note.relative_path
            .file_name()
            .and_then(|name| name.to_str()),
        Some(expected_filename.as_str())
    );
    assert!(!vault.note_path(&original_path).expect("safe path").exists());
    vault
        .save_note(&mut note, Some(&final_stamp))
        .expect("renamed note should continue saving");
}

#[test]
fn title_commit_never_overwrites_a_filename_collision() {
    let temporary = tempdir().expect("temporary directory");
    let vault = Vault::create(temporary.path().join("Vault")).expect("vault should be created");
    let created = vault
        .create_note("Original", "Inbox")
        .expect("note should be created");
    let (mut note, stamp) = vault
        .load_note(&created.relative_path)
        .expect("note should load");
    let colliding_relative = Path::new("Inbox").join(note_filename("Renamed", note.metadata.id));
    let colliding_path = vault
        .note_path(&colliding_relative)
        .expect("collision fixture path should be safe");
    fs::write(&colliding_path, "do not overwrite").expect("collision fixture should write");

    let error = vault
        .commit_title(&mut note, Some(&stamp), "Renamed")
        .expect_err("title commit must not overwrite a collision");
    assert!(matches!(error, Error::AlreadyExists(_)));
    assert_eq!(note.metadata.title, "Original");
    assert_eq!(note.relative_path, created.relative_path);
    assert_eq!(
        fs::read_to_string(colliding_path).expect("fixture remains"),
        "do not overwrite"
    );
}

#[test]
fn trash_restore_and_permanent_delete_preserve_original_notebook() {
    let temporary = tempdir().expect("temporary directory");
    let vault = Vault::create(temporary.path().join("Vault")).expect("vault should be created");
    let note = vault
        .create_note("Disposable", "Work/Plans")
        .expect("note should be created");
    let original = note.relative_path.clone();
    let id = note.metadata.id;

    let trashed = vault
        .move_to_trash(&original)
        .expect("ordinary delete should move to trash");
    assert_eq!(trashed.original_relative_path, original);
    assert!(!vault.note_path(&original).expect("safe path").exists());
    assert_eq!(vault.scan_trash().expect("trash scan").len(), 1);

    let restored = vault
        .restore_from_trash(id)
        .expect("restore should use original notebook");
    assert_eq!(restored, original);
    assert!(vault.note_path(&restored).expect("safe path").exists());
    assert!(vault.scan_trash().expect("trash scan").is_empty());

    vault
        .move_to_trash(&restored)
        .expect("note can be trashed again");
    vault
        .permanently_delete(id)
        .expect("permanent deletion should remove trash entry");
    assert!(vault.scan_trash().expect("trash scan").is_empty());
}

#[test]
fn empty_trash_removes_all_entries() {
    let temporary = tempdir().expect("temporary directory");
    let vault = Vault::create(temporary.path().join("Vault")).expect("vault should be created");
    for title in ["One", "Two"] {
        let note = vault.create_note(title, "Inbox").expect("create note");
        vault
            .move_to_trash(&note.relative_path)
            .expect("move note to trash");
    }
    assert_eq!(vault.empty_trash().expect("empty trash"), 2);
    assert!(vault.scan_trash().expect("trash scan").is_empty());
}

#[test]
fn encrypted_note_is_unreadable_at_rest_and_detects_tampering() {
    let temporary = tempdir().expect("temporary directory");
    let vault = Vault::create(temporary.path().join("Vault")).expect("vault should be created");
    let created = vault
        .create_note("SECRET-TITLE-7C91", "Inbox")
        .expect("note should be created");
    let (mut note, stamp) = vault
        .load_note(&created.relative_path)
        .expect("note should load");
    note.body = "DISTINCTIVE-SECRET-BODY-44A2".into();
    let stamp = vault
        .save_note(&mut note, Some(&stamp))
        .expect("plaintext save");
    let (encrypted_stamp, _session) = vault
        .encrypt_note(&mut note, Some(&stamp), "correct horse battery staple")
        .expect("encryption should succeed");
    assert_eq!(
        note.relative_path
            .extension()
            .and_then(|value| value.to_str()),
        Some("snote")
    );
    assert!(
        note.relative_path
            .file_name()
            .and_then(|value| value.to_str())
            .is_some_and(|name| name.starts_with("encrypted--"))
    );

    let encrypted_path = vault
        .note_path(&note.relative_path)
        .expect("encrypted path should be safe");
    let bytes = fs::read(&encrypted_path).expect("container should be readable");
    assert!(
        !bytes
            .windows("SECRET-TITLE-7C91".len())
            .any(|window| window == b"SECRET-TITLE-7C91")
    );
    assert!(
        !bytes
            .windows("DISTINCTIVE-SECRET-BODY-44A2".len())
            .any(|window| window == b"DISTINCTIVE-SECRET-BODY-44A2")
    );
    assert!(
        !vault
            .note_path(&created.relative_path)
            .expect("old path should be safe")
            .exists()
    );

    let wrong = vault.load_encrypted_note(&note.relative_path, "incorrect password");
    assert!(matches!(wrong, Err(Error::DecryptionFailed)));
    let (unlocked, loaded_stamp, _loaded_session) = vault
        .load_encrypted_note(&note.relative_path, "correct horse battery staple")
        .expect("correct password should unlock");
    assert_eq!(unlocked.metadata.title, "SECRET-TITLE-7C91");
    assert_eq!(unlocked.body, "DISTINCTIVE-SECRET-BODY-44A2");
    assert_eq!(loaded_stamp, encrypted_stamp);

    let mut tampered = bytes;
    let last = tampered.last_mut().expect("ciphertext has a tag");
    *last ^= 0x80;
    fs::write(&encrypted_path, tampered).expect("tamper fixture should write");
    let result = vault.load_encrypted_note(&note.relative_path, "correct horse battery staple");
    assert!(matches!(result, Err(Error::DecryptionFailed)));
}

#[test]
fn locked_encrypted_notes_do_not_leak_into_summaries_recovery_or_indexes() {
    let temporary = tempdir().expect("temporary directory");
    let vault = Vault::create(temporary.path().join("Vault")).expect("vault should be created");
    let created = vault
        .create_note("INDEX-SECRET-TITLE", "Inbox")
        .expect("note should be created");
    let (mut note, stamp) = vault
        .load_note(&created.relative_path)
        .expect("note should load");
    note.body = "INDEX-SECRET-BODY".into();
    let stamp = vault.save_note(&mut note, Some(&stamp)).expect("save note");
    let (_stamp, _session) = vault
        .encrypt_note(&mut note, Some(&stamp), "long unique passphrase")
        .expect("encrypt note");

    let summaries = vault.scan_notes().expect("scan notes");
    assert_eq!(summaries.len(), 1);
    assert!(summaries[0].encrypted);
    assert!(summaries[0].title.starts_with("Locked Note"));
    assert!(!summaries[0].title.contains("INDEX-SECRET-TITLE"));
    assert!(!summaries[0].preview.contains("INDEX-SECRET"));
    assert!(matches!(
        vault.write_recovery(&note),
        Err(Error::WrongNoteType)
    ));
    assert!(
        fs::read_dir(vault.recovery_dir())
            .expect("recovery dir")
            .next()
            .is_none()
    );
    assert!(!vault.root().join("search.sqlite").exists());

    let mut files = Vec::new();
    collect_files(vault.root(), &mut files);
    assert!(!files.iter().any(|path| {
        path.extension()
            .and_then(|value| value.to_str())
            .is_some_and(|extension| matches!(extension, "tmp" | "sqlite" | "db"))
    }));
}

#[test]
fn encrypted_password_change_and_plaintext_conversion_are_explicit() {
    let temporary = tempdir().expect("temporary directory");
    let vault = Vault::create(temporary.path().join("Vault")).expect("vault should be created");
    let created = vault
        .create_note("Convertible Secret", "Inbox")
        .expect("note should be created");
    let (mut note, stamp) = vault
        .load_note(&created.relative_path)
        .expect("note should load");
    note.body = "convertible secret body".into();
    let stamp = vault.save_note(&mut note, Some(&stamp)).expect("save note");
    let (_stamp, _session) = vault
        .encrypt_note(&mut note, Some(&stamp), "first strong passphrase")
        .expect("encrypt note");
    let encrypted_relative = note.relative_path.clone();

    vault
        .change_encrypted_password(
            &encrypted_relative,
            "first strong passphrase",
            "second stronger passphrase",
        )
        .expect("password change should succeed");
    assert!(matches!(
        vault.load_encrypted_note(&encrypted_relative, "first strong passphrase"),
        Err(Error::DecryptionFailed)
    ));
    let (unlocked, _stamp, _session) = vault
        .load_encrypted_note(&encrypted_relative, "second stronger passphrase")
        .expect("new password should unlock");
    assert_eq!(unlocked.body, "convertible secret body");

    let (plaintext, _stamp) = vault
        .remove_encryption(&encrypted_relative, "second stronger passphrase")
        .expect("explicit conversion should create Markdown");
    assert_eq!(
        plaintext
            .relative_path
            .extension()
            .and_then(|value| value.to_str()),
        Some("md")
    );
    let markdown = fs::read_to_string(
        vault
            .note_path(&plaintext.relative_path)
            .expect("plaintext path"),
    )
    .expect("plaintext note should be readable");
    assert!(markdown.contains("Convertible Secret"));
    assert!(markdown.contains("convertible secret body"));
    assert!(
        !vault
            .note_path(&encrypted_relative)
            .expect("old encrypted path")
            .exists()
    );
}

#[test]
fn scanned_summaries_search_body_text_but_not_locked_encrypted_content() {
    use senatorial_notes::search::summary_matches;

    let temporary = tempdir().expect("temporary directory");
    let vault = Vault::create(temporary.path().join("Vault")).expect("vault should be created");

    let plain = vault
        .create_note("Groceries", "Inbox")
        .expect("create plain");
    let (mut plain_note, stamp) = vault.load_note(&plain.relative_path).expect("load plain");
    plain_note.body = "remember to buy DISTINCTIVE-BODY-TOKEN today".into();
    vault
        .save_note(&mut plain_note, Some(&stamp))
        .expect("save plain body");

    let secret = vault.create_note("Secret", "Inbox").expect("create secret");
    let (mut secret_note, stamp) = vault.load_note(&secret.relative_path).expect("load secret");
    secret_note.body = "DISTINCTIVE-BODY-TOKEN must stay encrypted".into();
    let stamp = vault
        .save_note(&mut secret_note, Some(&stamp))
        .expect("save secret body");
    vault
        .encrypt_note(
            &mut secret_note,
            Some(&stamp),
            "a sufficiently long passphrase",
        )
        .expect("encrypt secret");

    let summaries = vault.scan_notes().expect("scan notes");
    let matches: Vec<&str> = summaries
        .iter()
        .filter(|summary| summary_matches(summary, "distinctive-body-token"))
        .map(|summary| summary.title.as_str())
        .collect();

    assert_eq!(
        matches,
        vec!["Groceries"],
        "body text matches for plaintext notes, and the locked note never does"
    );
    // The locked note is present but contributes nothing searchable but its
    // placeholder title.
    let locked = summaries
        .iter()
        .find(|summary| summary.encrypted)
        .expect("encrypted summary present");
    assert!(locked.body.is_empty());
    assert!(locked.tags.is_empty());
}

fn collect_files(directory: &Path, output: &mut Vec<std::path::PathBuf>) {
    for entry in fs::read_dir(directory).expect("fixture directory should be readable") {
        let path = entry.expect("directory entry").path();
        if path.is_dir() {
            collect_files(&path, output);
        } else {
            output.push(path);
        }
    }
}
