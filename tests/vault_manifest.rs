//! Stage A: `vault.toml` v2 schema, the v1 → v2 migration, and its safety
//! invariants.

use std::fs;
use std::path::Path;

use senatorial_notes::vault_manifest::{
    CURRENT_MANIFEST_VERSION, Migration, ORDINARY_MANIFEST_VERSION, VaultManifest, manifest_path,
};
use senatorial_notes::{Error, Vault, VaultKind};
use tempfile::tempdir;
use uuid::Uuid;

/// Lays down a minimally-complete vault directory tree containing a manifest
/// with the given `vault.toml` contents. `Vault::open` fills in any missing
/// standard directory, but a note file placed under `Notes/Inbox` must survive
/// untouched.
fn seed_vault(root: &Path, manifest_toml: &str) {
    let state = root.join(".senatorial-notes");
    fs::create_dir_all(&state).unwrap();
    fs::create_dir_all(root.join("Notes/Inbox")).unwrap();
    fs::write(state.join("vault.toml"), manifest_toml).unwrap();
}

const V1_MANIFEST: &str = "\
format_version = 1
vault_id = \"680adbd7-8797-4039-b2d5-12e36677b519\"
created_at = \"2026-01-15T09:30:00Z\"
";

fn read_manifest(root: &Path) -> VaultManifest {
    let text = fs::read_to_string(manifest_path(&root.join(".senatorial-notes"))).unwrap();
    toml::from_str(&text).unwrap()
}

#[test]
fn v1_migrates_to_v2_ordinary_and_preserves_identity() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("Vault");
    seed_vault(&root, V1_MANIFEST);

    let vault = Vault::open(&root).expect("v1 vault should open");

    assert_eq!(vault.kind(), VaultKind::Ordinary);
    assert_eq!(
        vault.vault_id(),
        Uuid::parse_str("680adbd7-8797-4039-b2d5-12e36677b519").unwrap(),
        "vault_id must be preserved across migration"
    );
    assert_eq!(vault.manifest().migrated_from, Some(1));
    assert_eq!(vault.manifest().format_version, ORDINARY_MANIFEST_VERSION);
    assert!(matches!(
        vault.migration(),
        Migration::Persisted { from: 1 }
    ));

    // The on-disk file was rewritten to v2, with identity intact.
    let on_disk = read_manifest(&root);
    assert_eq!(on_disk.format_version, ORDINARY_MANIFEST_VERSION);
    assert_eq!(on_disk.kind, VaultKind::Ordinary);
    assert_eq!(on_disk.migrated_from, Some(1));
    assert_eq!(on_disk.vault_id, vault.vault_id());
    assert_eq!(
        on_disk.created_at,
        vault.manifest().created_at,
        "created_at must be preserved across migration"
    );
    assert_eq!(
        on_disk
            .created_at
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "2026-01-15T09:30:00Z",
        "created_at must be the exact timestamp from the v1 manifest"
    );
}

#[test]
fn v1_with_hand_added_encrypted_kind_still_migrates_to_ordinary() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("Vault");
    seed_vault(
        &root,
        "\
format_version = 1
vault_id = \"680adbd7-8797-4039-b2d5-12e36677b519\"
created_at = \"2026-01-15T09:30:00Z\"
kind = \"encrypted\"
",
    );

    let vault = Vault::open(&root).expect("a v1 manifest is always ordinary, regardless of `kind`");
    assert_eq!(
        vault.kind(),
        VaultKind::Ordinary,
        "a `kind` key on a v1 manifest must be ignored, never honoured as encrypted"
    );
    assert_eq!(read_manifest(&root).kind, VaultKind::Ordinary);
}

#[test]
fn migration_does_not_touch_note_files() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("Vault");
    seed_vault(&root, V1_MANIFEST);

    let note_path = root.join("Notes/Inbox/keep--abcd1234.md");
    let note_body = "---\nid: abcd1234\n---\nuntouched\n";
    fs::write(&note_path, note_body).unwrap();
    let before = fs::metadata(&note_path).unwrap().modified().unwrap();

    Vault::open(&root).expect("v1 vault should open");

    assert_eq!(fs::read_to_string(&note_path).unwrap(), note_body);
    assert_eq!(
        fs::metadata(&note_path).unwrap().modified().unwrap(),
        before,
        "migration must not rewrite any note file"
    );
}

#[test]
fn fresh_vault_is_v2_ordinary_without_migrated_from() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("Fresh Vault");

    let vault = Vault::create(&root).expect("fresh vault should be created");

    assert_eq!(vault.manifest().format_version, ORDINARY_MANIFEST_VERSION);
    assert_eq!(vault.kind(), VaultKind::Ordinary);
    assert_eq!(vault.manifest().migrated_from, None);
    assert!(matches!(vault.migration(), Migration::NotNeeded));

    let on_disk = read_manifest(&root);
    assert_eq!(on_disk.format_version, ORDINARY_MANIFEST_VERSION);
    assert_eq!(on_disk.migrated_from, None);
    let text = fs::read_to_string(manifest_path(&root.join(".senatorial-notes"))).unwrap();
    assert!(
        !text.contains("migrated_from"),
        "a freshly created manifest must not serialize migrated_from"
    );
}

#[test]
fn reopening_a_v2_vault_does_not_rewrite_the_manifest() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("Vault");
    Vault::create(&root).unwrap();

    let path = manifest_path(&root.join(".senatorial-notes"));
    let bytes_before = fs::read(&path).unwrap();

    let reopened = Vault::open(&root).expect("v2 vault should reopen");
    assert!(matches!(reopened.migration(), Migration::NotNeeded));
    assert_eq!(
        fs::read(&path).unwrap(),
        bytes_before,
        "reopening a current-version vault must not rewrite vault.toml"
    );
}

#[test]
fn format_version_above_supported_is_refused_and_changes_nothing() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("Vault");
    let manifest = format!(
        "format_version = {}\nvault_id = \"680adbd7-8797-4039-b2d5-12e36677b519\"\ncreated_at = \"2026-01-15T09:30:00Z\"\nkind = \"ordinary\"\n",
        CURRENT_MANIFEST_VERSION + 1
    );
    seed_vault(&root, &manifest);
    let before = fs::read_to_string(manifest_path(&root.join(".senatorial-notes"))).unwrap();

    let error = Vault::open(&root).expect_err("a newer manifest version must be refused");
    assert!(
        matches!(error, Error::UnsupportedVaultVersion { found, supported }
            if found == CURRENT_MANIFEST_VERSION + 1 && supported == CURRENT_MANIFEST_VERSION),
        "got {error:?}"
    );

    assert_eq!(
        fs::read_to_string(manifest_path(&root.join(".senatorial-notes"))).unwrap(),
        before,
        "a refused open must not rewrite the manifest"
    );
    assert!(
        !root.join("Trash").exists() && !root.join("Attachments").exists(),
        "a refused open must not create the standard directory tree"
    );
}

#[test]
fn corrupt_manifest_is_refused() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("Vault");
    seed_vault(&root, "this is not valid toml =====");

    let error = Vault::open(&root).expect_err("a corrupt manifest must be refused");
    assert!(
        matches!(error, Error::VaultManifestCorrupt(_)),
        "got {error:?}"
    );
    assert!(!root.join("Trash").exists());
}

#[test]
fn manifest_without_format_version_is_corrupt_not_a_panic() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("Vault");
    seed_vault(
        &root,
        "vault_id = \"680adbd7-8797-4039-b2d5-12e36677b519\"\ncreated_at = \"2026-01-15T09:30:00Z\"\n",
    );
    let error = Vault::open(&root).expect_err("a manifest without format_version must be refused");
    assert!(
        matches!(error, Error::VaultManifestCorrupt(_)),
        "got {error:?}"
    );
}

#[test]
fn encrypted_kind_is_refused_by_this_build() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("Vault");
    seed_vault(
        &root,
        "\
format_version = 2
vault_id = \"680adbd7-8797-4039-b2d5-12e36677b519\"
created_at = \"2026-01-15T09:30:00Z\"
kind = \"encrypted\"
",
    );

    let error = Vault::open(&root).expect_err("this build cannot open an encrypted vault");
    assert!(
        matches!(error, Error::UnsupportedVaultKind),
        "got {error:?}"
    );
    assert!(
        !root.join("Trash").exists() && !root.join("Attachments").exists(),
        "a refused encrypted vault must not create the directory tree"
    );
}

#[test]
fn read_only_vault_still_opens_via_in_memory_migration() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempdir().unwrap();
    let root = dir.path().join("Vault");
    seed_vault(&root, V1_MANIFEST);
    // The whole standard tree must already exist (a real read-only vault has
    // it); only writing back the upgraded manifest should be what fails.
    for sub in [
        "Attachments",
        "Trash",
        ".senatorial-notes/history",
        ".senatorial-notes/recovery",
    ] {
        fs::create_dir_all(root.join(sub)).unwrap();
    }

    let state = root.join(".senatorial-notes");
    fs::set_permissions(&state, fs::Permissions::from_mode(0o555)).unwrap();

    let opened = Vault::open(&root);

    // Restore write access before the tempdir is cleaned up, regardless of the
    // assertion outcome.
    fs::set_permissions(&state, fs::Permissions::from_mode(0o755)).unwrap();

    let vault = opened.expect("a read-only vault must still open (the open path must not fail)");
    assert_eq!(vault.kind(), VaultKind::Ordinary);
    assert_eq!(
        vault.vault_id().to_string(),
        "680adbd7-8797-4039-b2d5-12e36677b519"
    );

    match vault.migration() {
        Migration::InMemoryOnly { from, .. } => {
            assert_eq!(*from, 1);
            assert!(vault.migration().warning().is_some());
            // The on-disk file could not be upgraded and is still v1.
            assert_eq!(read_manifest(&root).format_version, 1);
        }
        // Running as root bypasses the permission bits, so the write succeeds.
        Migration::Persisted { from } => {
            assert_eq!(*from, 1);
            assert_eq!(
                read_manifest(&root).format_version,
                ORDINARY_MANIFEST_VERSION
            );
        }
        Migration::NotNeeded => panic!("a v1 manifest must record a migration"),
    }
}

/// Enables the read-only path: a v1 vault whose `.senatorial-notes` directory
/// cannot be written, so the v1 → v2 rewrite fails. Returns the opened vault and
/// a guard that restores permissions when dropped (so the tempdir can be
/// cleaned up).
fn open_read_only_v1(root: &Path) -> (Vault, PermGuard) {
    use std::os::unix::fs::PermissionsExt;
    let state = root.join(".senatorial-notes");
    // A real read-only vault already has its full tree.
    for sub in [
        "Notes/Inbox",
        "Attachments",
        "Trash",
        ".senatorial-notes/history",
        ".senatorial-notes/recovery",
    ] {
        fs::create_dir_all(root.join(sub)).unwrap();
    }
    fs::set_permissions(&state, fs::Permissions::from_mode(0o555)).unwrap();
    let guard = PermGuard(state.clone());
    let vault = Vault::open(root).expect("a read-only vault must still open");
    (vault, guard)
}

struct PermGuard(std::path::PathBuf);
impl Drop for PermGuard {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
    }
}

#[test]
fn read_only_migration_blocks_note_creation() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("Vault");
    seed_vault(&root, V1_MANIFEST);

    let (vault, _guard) = open_read_only_v1(&root);
    if !vault.is_read_only() {
        return; // running as root: the migration persisted, nothing to test here
    }

    let before: Vec<_> = fs::read_dir(root.join("Notes/Inbox")).unwrap().collect();
    let error = vault
        .create_note("Should Not Exist", Path::new("Inbox"))
        .expect_err("a read-only vault must reject note creation");
    assert!(matches!(error, Error::VaultReadOnly), "got {error:?}");
    let after: Vec<_> = fs::read_dir(root.join("Notes/Inbox")).unwrap().collect();
    assert_eq!(
        before.len(),
        after.len(),
        "a rejected create_note must not leave any file behind"
    );
}

#[test]
fn read_only_migration_blocks_mutation_of_an_existing_note() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("Vault");
    seed_vault(&root, V1_MANIFEST);
    let note_rel = Path::new("Inbox/keep--abcd1234.md");
    let note_body = "---\nid: abcd1234\ntitle: Keep\n---\nbody\n";
    fs::write(root.join("Notes").join(note_rel), note_body).unwrap();

    let (vault, _guard) = open_read_only_v1(&root);
    if !vault.is_read_only() {
        return;
    }

    let trash_err = vault
        .move_to_trash(note_rel)
        .expect_err("a read-only vault must reject move_to_trash");
    assert!(
        matches!(trash_err, Error::VaultReadOnly),
        "got {trash_err:?}"
    );

    let nb_err = vault
        .create_notebook("Work")
        .expect_err("a read-only vault must reject create_notebook");
    assert!(matches!(nb_err, Error::VaultReadOnly), "got {nb_err:?}");

    assert_eq!(
        fs::read_to_string(root.join("Notes").join(note_rel)).unwrap(),
        note_body,
        "the note must be untouched"
    );
    assert!(
        !root.join("Notes/Work").exists(),
        "no notebook directory may be created in a read-only session"
    );
}

#[test]
fn read_only_migration_does_not_partially_create_the_directory_tree() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempdir().unwrap();
    let root = dir.path().join("Vault");
    seed_vault(&root, V1_MANIFEST); // only `.senatorial-notes` + `Notes/Inbox`
    let state = root.join(".senatorial-notes");
    fs::set_permissions(&state, fs::Permissions::from_mode(0o555)).unwrap();

    let opened = Vault::open(&root);
    fs::set_permissions(&state, fs::Permissions::from_mode(0o755)).unwrap();
    let vault = opened.expect("read-only vault opens");

    if !vault.is_read_only() {
        return;
    }
    assert!(
        !root.join("Trash").exists() && !root.join("Attachments").exists(),
        "ensure_directories must be skipped for a read-only vault (no partial tree upgrade)"
    );
    assert_eq!(
        read_manifest(&root).format_version,
        1,
        "on-disk manifest stays v1"
    );
    assert!(vault.migration().warning().is_some());
}

#[test]
fn writable_v1_vault_migrates_and_stays_writable() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("Vault");
    seed_vault(&root, V1_MANIFEST);

    let vault = Vault::open(&root).expect("writable v1 vault should open");
    assert!(
        !vault.is_read_only(),
        "a writable vault must not be marked read-only"
    );
    assert!(matches!(
        vault.migration(),
        Migration::Persisted { from: 1 }
    ));

    // A normal mutation still works.
    let note = vault
        .create_note("Normal Note", Path::new("Inbox"))
        .expect("a writable migrated vault must still accept writes");
    assert!(
        root.join("Notes").join(&note.relative_path).is_file(),
        "the note file must actually be written"
    );
}

#[test]
fn schema_round_trips_and_uses_kebab_case() {
    let manifest = VaultManifest::new_ordinary();
    let text = toml::to_string_pretty(&manifest).unwrap();
    assert!(
        text.contains("kind = \"ordinary\""),
        "kind must serialize kebab-case: {text}"
    );
    assert!(!text.contains("migrated_from"));

    let parsed: VaultManifest = toml::from_str(&text).unwrap();
    assert_eq!(parsed, manifest);
}

#[test]
fn vault_kind_serializes_as_exactly_ordinary_and_encrypted() {
    // The report had garbled terminal output ("ordinarypted"); this pins the
    // real strings.
    let ordinary = VaultManifest::new_ordinary();
    let mut encrypted = ordinary.clone();
    encrypted.kind = VaultKind::Encrypted;

    let ord_toml = toml::to_string_pretty(&ordinary).unwrap();
    let enc_toml = toml::to_string_pretty(&encrypted).unwrap();

    assert!(
        ord_toml.lines().any(|l| l == "kind = \"ordinary\""),
        "exact line `kind = \"ordinary\"` expected, got:\n{ord_toml}"
    );
    assert!(
        enc_toml.lines().any(|l| l == "kind = \"encrypted\""),
        "exact line `kind = \"encrypted\"` expected, got:\n{enc_toml}"
    );
    assert!(!ord_toml.contains("encrypted") && !enc_toml.contains("ordinary"));

    let back: VaultManifest = toml::from_str(&enc_toml).unwrap();
    assert_eq!(back.kind, VaultKind::Encrypted);
}

#[test]
fn schema_defaults_kind_to_ordinary_and_ignores_unknown_fields() {
    let parsed: VaultManifest = toml::from_str(
        "\
format_version = 2
vault_id = \"680adbd7-8797-4039-b2d5-12e36677b519\"
created_at = \"2026-01-15T09:30:00Z\"
future_field = \"ignored\"
",
    )
    .expect("missing kind and unknown fields must both be tolerated");
    assert_eq!(parsed.kind, VaultKind::Ordinary);
    assert_eq!(parsed.migrated_from, None);
}

#[test]
fn golden_v1_fixture_opens_and_migrates() {
    let fixture = include_str!("fixtures/vault_v1.toml");
    let dir = tempdir().unwrap();
    let root = dir.path().join("Vault");
    seed_vault(&root, fixture);

    let vault = Vault::open(&root).expect("the checked-in v1 fixture must still open");
    assert_eq!(vault.kind(), VaultKind::Ordinary);
    assert_eq!(vault.manifest().format_version, ORDINARY_MANIFEST_VERSION);
    assert_eq!(vault.manifest().migrated_from, Some(1));
}
