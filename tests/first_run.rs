//! UX Stage 1: managed workspace / first-run structure and display-name
//! renaming. The GTK wiring (`build_application` → `run_first_run_setup`,
//! sidebar) is covered by `ui_source_invariants`; this file covers the
//! storage-layer guarantees those flows rely on.

use std::fs;

use senatorial_notes::{Vault, VaultKind, paths};
use tempfile::tempdir;

const PASSWORD: &str = "correct horse battery staple";

fn blob_bytes(vault: &Vault) -> Vec<(std::path::PathBuf, Vec<u8>)> {
    let store = vault.state_dir().join("store");
    let mut out: Vec<_> = fs::read_dir(&store)
        .expect("store dir")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .map(|p| (p.clone(), fs::read(&p).expect("blob")))
        .collect();
    out.sort();
    out
}

#[test]
fn the_managed_workspace_root_is_an_absolute_named_folder() {
    let root = paths::default_workspace_root().expect("a workspace root");
    assert!(root.is_absolute(), "never a relative or hard-coded path");
    assert_eq!(
        root.file_name().and_then(|n| n.to_str()),
        Some("SenatorialNotes"),
        "the managed root is <Documents>/SenatorialNotes"
    );
    assert!(
        root.parent().is_some_and(|p| p.is_absolute()),
        "sits inside a real platform directory"
    );
}

#[test]
fn first_run_shape_main_is_standard_and_secure_is_the_encrypted_format() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("SenatorialNotes");

    // "Main" — created automatically, no password, Standard Vault.
    let main = Vault::create(root.join("Main")).expect("Main vault");
    assert_eq!(main.kind(), VaultKind::Ordinary);
    assert!(!main.is_encrypted());
    assert_eq!(main.manifest().format_version, 2);
    main.create_note("first", "Inbox")
        .expect("write immediately");

    // "Secure" — created only after a password is chosen, existing v3 format.
    let secure = Vault::create_encrypted(root.join("Secure"), PASSWORD).expect("Secure vault");
    assert_eq!(secure.kind(), VaultKind::Encrypted);
    assert_eq!(secure.manifest().format_version, 3);
    assert!(secure.state_dir().join("vault.keys").is_file());
    assert!(!root.join("Secure/Notes").exists());
}

#[test]
fn a_display_name_rename_touches_no_vault_bytes() {
    // Renaming is a `config.vault_index` operation only. This test asserts the
    // vault side is inert: the folder is not renamed, the manifest and every
    // encrypted blob are byte-identical, and the vault still opens + unlocks.
    let dir = tempdir().unwrap();
    let path = dir.path().join("Secure");
    let vault = Vault::create_encrypted(&path, PASSWORD).expect("secure vault");
    vault.create_note("bound note", "Inbox").expect("note");

    let blobs_before = blob_bytes(&vault);
    let keyfile_before = fs::read(vault.state_dir().join("vault.keys")).expect("keyfile");
    let toml_before = fs::read(vault.state_dir().join("vault.toml")).expect("vault.toml");

    // A rename in the product changes only the config-side display name;
    // nothing calls into the vault at all. Simulate "time passes" and reopen.
    drop(vault);
    let reopened = Vault::open(&path).expect("reopen after rename");
    reopened
        .unlock(PASSWORD)
        .expect("still unlocks with the same password");
    assert_eq!(reopened.scan_notes().expect("scan").len(), 1);

    assert!(path.is_dir(), "the folder keeps its name");
    assert_eq!(
        blobs_before,
        blob_bytes(&reopened),
        "no blob is rewritten by a rename"
    );
    assert_eq!(
        keyfile_before,
        fs::read(reopened.state_dir().join("vault.keys")).unwrap()
    );
    assert_eq!(
        toml_before,
        fs::read(reopened.state_dir().join("vault.toml")).unwrap()
    );
}

#[test]
fn a_standard_vault_rename_is_also_bytes_inert() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("Main");
    let vault = Vault::create(&path).expect("standard vault");
    let created = vault.create_note("keep", "Inbox").expect("note");
    let note_path = vault.note_path(&created.relative_path).expect("note path");
    let before = fs::read(&note_path).expect("note bytes");

    drop(vault);
    // (rename = config only)
    let reopened = Vault::open(&path).expect("reopen");
    assert_eq!(reopened.kind(), VaultKind::Ordinary);
    assert_eq!(fs::read(&note_path).unwrap(), before);
    assert!(path.is_dir());
}
