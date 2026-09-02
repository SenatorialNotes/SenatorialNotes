//! Stage B: multi-vault data-safety invariants that can be checked without a
//! display. The GUI-level switch lifecycle (session generation, stale-callback
//! inertness, read-only UI) is covered by `tests/ui_source_invariants.rs` and
//! the `SessionRegistry` unit tests in `src/ui_state.rs`.

use std::fs;
use std::path::Path;

use senatorial_notes::config::{AppConfig, VaultSessionState};
use senatorial_notes::{Error, Vault};
use tempfile::tempdir;
use uuid::Uuid;

fn note_bytes(vault: &Vault, relative: &Path) -> Vec<u8> {
    fs::read(vault.root().join("Notes").join(relative)).unwrap()
}

// ---------------------------------------------------------------------------
// Recent-vault list: ordering, de-duplication, removal
// ---------------------------------------------------------------------------

#[test]
fn recent_vaults_are_most_recent_first_and_de_duplicated() {
    let dir = tempdir().unwrap();
    let a = dir.path().join("A");
    let b = dir.path().join("B");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();

    let mut config = AppConfig::default();
    config.remember_vault(&a);
    config.remember_vault(&b);
    config.remember_vault(&a); // re-using A must move it to the front, not add it

    let mru = config.recent_vaults_mru();
    assert_eq!(mru.len(), 2, "A must not appear twice");
    assert_eq!(
        std::fs::canonicalize(&mru[0]).unwrap(),
        std::fs::canonicalize(&a).unwrap()
    );
    assert_eq!(
        std::fs::canonicalize(&mru[1]).unwrap(),
        std::fs::canonicalize(&b).unwrap()
    );
    assert_eq!(
        config.last_vault.map(|p| std::fs::canonicalize(p).unwrap()),
        Some(std::fs::canonicalize(&a).unwrap())
    );
}

#[test]
fn recent_vaults_mru_collapses_legacy_duplicate_entries() {
    // A config written by an older build could contain the same path twice.
    let config = AppConfig {
        recent_vaults: vec![
            "/x/one".into(),
            "/x/two".into(),
            "/x/one".into(),
            "/x/two".into(),
        ],
        ..AppConfig::default()
    };
    let mru = config.recent_vaults_mru();
    assert_eq!(
        mru,
        vec![
            std::path::PathBuf::from("/x/one"),
            std::path::PathBuf::from("/x/two")
        ]
    );
}

#[test]
fn forget_vault_removes_only_the_named_entry_and_clears_last_vault() {
    let dir = tempdir().unwrap();
    let a = dir.path().join("A");
    let b = dir.path().join("B");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();

    let mut config = AppConfig::default();
    config.remember_vault(&a);
    config.remember_vault(&b); // b is now last_vault
    config.forget_vault(&b);

    let mru = config.recent_vaults_mru();
    assert_eq!(mru.len(), 1);
    assert_eq!(
        std::fs::canonicalize(&mru[0]).unwrap(),
        std::fs::canonicalize(&a).unwrap()
    );
    assert_eq!(
        config.last_vault, None,
        "forgetting the last vault clears last_vault"
    );

    config.forget_vault(&a);
    assert!(config.recent_vaults_mru().is_empty());
}

#[test]
fn a_missing_recent_path_is_never_turned_into_a_vault() {
    let dir = tempdir().unwrap();
    let gone = dir.path().join("Deleted Vault");

    let error =
        Vault::open(&gone).expect_err("opening a missing path must fail, not create a vault");
    assert!(matches!(error, Error::InvalidPath(_)), "got {error:?}");
    assert!(!gone.exists(), "a failed open must not create the folder");
}

// ---------------------------------------------------------------------------
// Per-vault session state
// ---------------------------------------------------------------------------

#[test]
fn per_vault_session_state_is_keyed_by_vault_id() {
    let id_a = Uuid::new_v4();
    let id_b = Uuid::new_v4();
    let mut config = AppConfig::default();

    let note = Uuid::new_v4();
    config.set_vault_session(
        id_a,
        VaultSessionState {
            last_note: Some(note),
            last_view: Some("pinned".into()),
            editor_scroll: Some(120.0),
            ..VaultSessionState::default()
        },
    );
    config.set_vault_session(
        id_b,
        VaultSessionState {
            last_note: None,
            last_view: Some("notebook:Work".into()),
            editor_scroll: None,
            ..VaultSessionState::default()
        },
    );

    assert_eq!(config.vault_session(id_a).unwrap().last_note, Some(note));
    assert_eq!(
        config.vault_session(id_b).unwrap().last_view.as_deref(),
        Some("notebook:Work")
    );

    // An all-empty state removes the entry rather than accumulating noise.
    config.set_vault_session(id_a, VaultSessionState::default());
    assert!(config.vault_session(id_a).is_none());
    assert!(config.vault_session(id_b).is_some());
}

#[test]
fn session_state_survives_a_config_round_trip() {
    let id = Uuid::new_v4();
    let mut config = AppConfig::default();
    config.set_vault_session(
        id,
        VaultSessionState {
            last_note: Some(Uuid::new_v4()),
            last_view: Some("archive".into()),
            editor_scroll: Some(42.5),
            ..VaultSessionState::default()
        },
    );
    let text = toml::to_string(&config).unwrap();
    let back: AppConfig = toml::from_str(&text).unwrap();
    assert_eq!(back, config);
}

#[test]
fn an_older_config_without_vault_sessions_still_loads() {
    let source = "recent_vaults = [\"/notes/one\"]\nlast_vault = \"/notes/one\"\n";
    let config: AppConfig = toml::from_str(source).expect("pre-Stage-B config must still parse");
    assert!(config.vault_sessions.is_empty());
}

// ---------------------------------------------------------------------------
// Storage isolation: acting on B never changes A
// ---------------------------------------------------------------------------

#[test]
fn operations_on_vault_b_do_not_modify_vault_a() {
    let dir = tempdir().unwrap();
    let vault_a = Vault::create(dir.path().join("A")).unwrap();
    let vault_b = Vault::create(dir.path().join("B")).unwrap();

    let note_a = vault_a.create_note("A note", Path::new("Inbox")).unwrap();
    let a_snapshot = note_bytes(&vault_a, &note_a.relative_path);
    let a_mtime = fs::metadata(vault_a.root().join("Notes").join(&note_a.relative_path))
        .unwrap()
        .modified()
        .unwrap();

    // Churn B heavily.
    for index in 0..10 {
        let note_b = vault_b
            .create_note(&format!("B {index}"), Path::new("Inbox"))
            .unwrap();
        let entry = vault_b.move_to_trash(&note_b.relative_path).unwrap();
        vault_b.restore_from_trash(entry.id).unwrap();
    }
    vault_b.create_notebook("B/Deep/Nested").unwrap();

    // A is byte-for-byte and mtime-for-mtime unchanged.
    assert_eq!(note_bytes(&vault_a, &note_a.relative_path), a_snapshot);
    assert_eq!(
        fs::metadata(vault_a.root().join("Notes").join(&note_a.relative_path))
            .unwrap()
            .modified()
            .unwrap(),
        a_mtime
    );
    assert_eq!(vault_a.scan_notes().unwrap().len(), 1);
    assert!(!vault_a.root().join("Notes/B").exists());
}

#[test]
fn repeated_a_b_switching_leaves_both_vaults_consistent() {
    let dir = tempdir().unwrap();
    let root_a = dir.path().join("A");
    let root_b = dir.path().join("B");
    Vault::create(&root_a).unwrap();
    Vault::create(&root_b).unwrap();

    for round in 0..20 {
        // Simulate a switch by fully re-opening each vault, as `open_vault` does.
        let a = Vault::open(&root_a).unwrap();
        let created = a
            .create_note(&format!("A{round}"), Path::new("Inbox"))
            .unwrap();
        let entry = a.move_to_trash(&created.relative_path).unwrap();
        a.permanently_delete(entry.id).unwrap();

        let b = Vault::open(&root_b).unwrap();
        let created = b
            .create_note(&format!("B{round}"), Path::new("Inbox"))
            .unwrap();
        let entry = b.move_to_trash(&created.relative_path).unwrap();
        b.restore_from_trash(entry.id).unwrap();
    }

    let a = Vault::open(&root_a).unwrap();
    let b = Vault::open(&root_b).unwrap();
    assert_eq!(
        a.scan_notes().unwrap().len(),
        0,
        "every A note was permanently deleted"
    );
    assert_eq!(
        b.scan_notes().unwrap().len(),
        20,
        "every B note was restored"
    );
    assert!(a.scan_trash().unwrap().is_empty());
    assert!(b.scan_trash().unwrap().is_empty());
}

#[test]
fn switching_away_and_back_keeps_an_encrypted_snote_openable() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("Vault");
    let password = "correct horse battery staple";

    let relative = {
        let vault = Vault::create(&root).unwrap();
        let mut note = vault.create_note("Secret", Path::new("Inbox")).unwrap();
        note.body = "classified".to_string();
        let stamp = vault.current_stamp(&note.relative_path).unwrap();
        let (_stamp, _session) = vault
            .encrypt_note(&mut note, Some(&stamp), password)
            .unwrap();
        note.relative_path.clone()
    };

    // "Switch to another vault and back" = drop and re-open.
    let vault = Vault::open(&root).unwrap();
    let scanned = vault.scan_notes().unwrap();
    assert_eq!(scanned.len(), 1);
    assert!(scanned[0].encrypted && scanned[0].locked);

    let (note, _stamp, _session) = vault.load_encrypted_note(&relative, password).unwrap();
    assert_eq!(note.body, "classified");
    assert!(
        vault
            .load_encrypted_note(&relative, "wrong password")
            .is_err(),
        "the wrong password must still fail after a round-trip"
    );
}
