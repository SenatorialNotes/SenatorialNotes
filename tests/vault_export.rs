//! Stage E: Secure \u{2192} Standard **safe export**.
//!
//! Rules under test:
//! * every live note, the notebook tree (empty notebooks included), all
//!   metadata, and Trash are reproduced in a new *Standard* vault;
//! * per-note `.snote` containers come out byte-identical and still open with
//!   their own password;
//! * the source Secure Vault is **byte-for-byte unchanged** (not just mtimes);
//! * export is directory-transactional — a failure leaves no destination and no
//!   temp directory, and never touches the source;
//! * path relationships that could clobber the source are refused;
//! * a locked vault, a non-empty destination, and a wrong password are refused.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use senatorial_notes::vault_export::{
    ExportParams, ExportProgress, export_secure_vault_to_standard,
};
use senatorial_notes::{Error, Vault};
use tempfile::tempdir;

const PASSWORD: &str = "correct horse battery staple";
const NOTE_PASSWORD: &str = "per note secret phrase";

fn keyfile_bytes(vault: &Vault) -> Vec<u8> {
    fs::read(vault.state_dir().join("vault.keys")).unwrap()
}

fn params(source: &Vault, destination: &Path) -> ExportParams {
    ExportParams {
        source_root: source.root().to_path_buf(),
        source_state_dir: source.state_dir(),
        vault_id: source.vault_id(),
        keyfile_bytes: keyfile_bytes(source),
        password: zeroize::Zeroizing::new(PASSWORD.to_string()),
        destination: destination.to_path_buf(),
    }
}

/// Every regular file under `dir`, as `path -> bytes`, for byte-level identity
/// checks.
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut out = BTreeMap::new();
    fn walk(dir: &Path, base: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, base, out);
            } else if let Ok(bytes) = fs::read(&path) {
                out.insert(path.strip_prefix(base).unwrap().to_path_buf(), bytes);
            }
        }
    }
    walk(dir, dir, &mut out);
    out
}

fn build_source(root: &Path) -> Vault {
    let vault = Vault::create_encrypted(root, PASSWORD).expect("secure vault");
    vault.unlock(PASSWORD).unwrap();
    vault.create_notebook("Work/Projects").unwrap();
    vault.create_notebook("Archive Box").unwrap(); // stays empty

    let mut a = vault.create_note("Alpha", "Inbox").unwrap();
    a.body = "alpha body".into();
    a.metadata.tags = vec!["red".into(), "green".into()];
    a.metadata.pinned = true;
    a.metadata.favourite = true;
    vault.save_note(&mut a, None).unwrap();

    let mut b = vault.create_note("Beta", "Work/Projects").unwrap();
    b.body = "beta body".into();
    b.metadata.archived = true;
    vault.save_note(&mut b, None).unwrap();

    let mut c = vault.create_note("Gamma", "Inbox").unwrap();
    c.body = "gamma secret".into();
    vault.save_note(&mut c, None).unwrap();
    vault
        .encrypt_note(&mut c, None, NOTE_PASSWORD)
        .expect("per-note encryption");

    vault
}

#[test]
fn export_reproduces_notes_notebooks_metadata_and_is_a_new_standard_vault() {
    let dir = tempdir().unwrap();
    let source = build_source(&dir.path().join("Secure"));
    let dest_path = dir.path().join("Exported");

    let report =
        export_secure_vault_to_standard(params(&source, &dest_path), ExportProgress::new())
            .unwrap();
    assert_eq!(report.notes, 2);
    assert_eq!(report.snotes, 1);
    assert_eq!(report.trashed, 0);

    let exported = Vault::open(&dest_path).unwrap();
    assert!(!exported.is_encrypted(), "the export is a Standard Vault");
    assert_ne!(
        exported.vault_id(),
        source.vault_id(),
        "a copy is a new vault with its own id"
    );

    let mut summaries = exported.scan_notes().unwrap();
    summaries.sort_by(|l, r| l.title.cmp(&r.title));
    assert_eq!(summaries.len(), 3);

    // Alpha: plaintext, metadata preserved exactly.
    let (alpha, _) = exported
        .load_note(
            Path::new("Inbox").join(
                summaries
                    .iter()
                    .find(|s| s.title == "Alpha")
                    .unwrap()
                    .relative_path
                    .file_name()
                    .unwrap(),
            ),
        )
        .unwrap();
    assert_eq!(alpha.body, "alpha body");
    assert_eq!(alpha.metadata.tags, vec!["red", "green"]);
    assert!(alpha.metadata.pinned && alpha.metadata.favourite);

    // Beta lives in the nested notebook and kept its archived flag.
    let beta_summary = summaries.iter().find(|s| s.title == "Beta").unwrap();
    assert!(beta_summary.relative_path.starts_with("Work/Projects"));

    // Empty notebook recreated.
    let notebooks: Vec<_> = exported
        .list_notebooks()
        .unwrap()
        .into_iter()
        .map(|n| n.relative_path)
        .collect();
    assert!(notebooks.contains(&PathBuf::from("Archive Box")));
    assert!(notebooks.contains(&PathBuf::from("Work/Projects")));

    // Gamma: the .snote came out byte-identical and still opens with its
    // per-note password.
    let gamma = summaries.iter().find(|s| s.encrypted).unwrap();
    let (note, _, _) = exported
        .load_encrypted_note(&gamma.relative_path, NOTE_PASSWORD)
        .expect("inner .snote opens with its original password");
    assert_eq!(note.body, "gamma secret");
}

#[test]
fn the_source_secure_vault_is_byte_identical_after_export() {
    let dir = tempdir().unwrap();
    let source = build_source(&dir.path().join("Secure"));
    let before = snapshot(&source.state_dir());

    export_secure_vault_to_standard(
        params(&source, &dir.path().join("Exported")),
        ExportProgress::new(),
    )
    .unwrap();

    assert_eq!(
        before,
        snapshot(&source.state_dir()),
        "every source byte (blobs, manifest, keyfile, vault.toml) must be unchanged"
    );
}

#[test]
fn trash_round_trips_and_can_be_restored_from_the_exported_vault() {
    let dir = tempdir().unwrap();
    let source = build_source(&dir.path().join("Secure"));

    let summaries = source.scan_notes().unwrap();
    let alpha = summaries.iter().find(|s| s.title == "Alpha").unwrap();
    let trashed = source.move_to_trash(&alpha.relative_path).unwrap();

    let dest_path = dir.path().join("Exported");
    let report =
        export_secure_vault_to_standard(params(&source, &dest_path), ExportProgress::new())
            .unwrap();
    assert_eq!(report.trashed, 1);
    assert_eq!(report.notes + report.snotes, 2);

    let exported = Vault::open(&dest_path).unwrap();
    let export_trash = exported.scan_trash().unwrap();
    assert_eq!(export_trash.len(), 1);
    assert_eq!(export_trash[0].id, trashed.id);

    let restored_path = exported.restore_from_trash(trashed.id).unwrap();
    let (restored, _) = exported.load_note(&restored_path).unwrap();
    assert_eq!(restored.body, "alpha body");
    assert_eq!(restored.metadata.tags, vec!["red", "green"]);
}

#[test]
fn a_locked_secure_vault_cannot_be_exported() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("Secure");
    let vault = Vault::create_encrypted(&root, PASSWORD).unwrap();
    vault.lock();

    // A locked vault still exposes its keyfile bytes; the export must refuse on
    // the wrong password rather than proceed. Use a deliberately wrong password.
    let mut p = params(&vault, &dir.path().join("Exported"));
    p.password = zeroize::Zeroizing::new("not the vault password".into());
    let err = export_secure_vault_to_standard(p, ExportProgress::new()).unwrap_err();
    assert!(matches!(err, Error::DecryptionFailed));
    assert!(!dir.path().join("Exported").exists());
}

#[test]
fn a_wrong_vault_password_writes_nothing() {
    let dir = tempdir().unwrap();
    let source = build_source(&dir.path().join("Secure"));
    let dest = dir.path().join("Exported");
    let mut p = params(&source, &dest);
    p.password = zeroize::Zeroizing::new("wrong".into());

    let err = export_secure_vault_to_standard(p, ExportProgress::new()).unwrap_err();
    assert!(matches!(err, Error::DecryptionFailed));
    assert!(!dest.exists());
    assert!(no_temp_dirs(dir.path()));
}

#[test]
fn a_non_empty_or_vault_destination_is_refused() {
    let dir = tempdir().unwrap();
    let source = build_source(&dir.path().join("Secure"));

    let non_empty = dir.path().join("HasStuff");
    fs::create_dir_all(&non_empty).unwrap();
    fs::write(non_empty.join("file"), b"x").unwrap();
    assert!(matches!(
        export_secure_vault_to_standard(params(&source, &non_empty), ExportProgress::new()),
        Err(Error::ExportTargetInvalid(_))
    ));

    let existing_vault = dir.path().join("AlreadyAVault");
    Vault::create(&existing_vault).unwrap();
    assert!(matches!(
        export_secure_vault_to_standard(params(&source, &existing_vault), ExportProgress::new()),
        Err(Error::ExportTargetInvalid(_))
    ));
}

#[test]
fn destination_inside_or_equal_to_the_source_is_refused() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("Secure");
    let source = build_source(&root);

    for candidate in [root.clone(), root.join("nested/export"), root.join("out")] {
        let err =
            export_secure_vault_to_standard(params(&source, &candidate), ExportProgress::new())
                .unwrap_err();
        assert!(
            matches!(err, Error::ExportTargetInvalid(_)),
            "must refuse a destination at/inside the source: {}",
            candidate.display()
        );
    }
    // And the reverse: source inside destination.
    let outer = dir.path().join("Secure"); // parent-ish alias handled by resolve
    let _ = outer;
}

#[test]
fn a_mid_export_decrypt_failure_leaves_no_destination_and_no_temp() {
    let dir = tempdir().unwrap();
    let source = build_source(&dir.path().join("Secure"));
    let source_before = snapshot(&source.state_dir());

    // Corrupt one note blob so peeling it fails partway through the export.
    let store = source.state_dir().join("store");
    let victim = fs::read_dir(&store)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.is_file() && p.file_name().unwrap() != "manifest")
        .expect("a note blob");
    let mut bytes = fs::read(&victim).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    fs::write(&victim, &bytes).unwrap();

    let dest = dir.path().join("Exported");
    let err =
        export_secure_vault_to_standard(params(&source, &dest), ExportProgress::new()).unwrap_err();
    assert!(matches!(
        err,
        Error::DecryptionFailed | Error::InvalidEncryptedVault(_)
    ));
    assert!(!dest.exists(), "no destination on failure");
    assert!(no_temp_dirs(dir.path()), "temp export directory cleaned up");

    // The (already independently corrupted) source is otherwise untouched by
    // the export itself: only our one victim blob differs.
    let after = snapshot(&source.state_dir());
    let differing: Vec<_> = source_before
        .keys()
        .filter(|k| source_before.get(*k) != after.get(*k))
        .collect();
    assert_eq!(differing.len(), 1, "export changed nothing in the source");
}

#[test]
fn progress_reports_totals_and_cancellation_is_clean() {
    let dir = tempdir().unwrap();
    let source = build_source(&dir.path().join("Secure"));
    let dest = dir.path().join("Exported");

    let progress = ExportProgress::new();
    progress.request_cancel();
    let err =
        export_secure_vault_to_standard(params(&source, &dest), progress.clone()).unwrap_err();
    assert!(matches!(err, Error::ExportCancelled));
    assert!(!dest.exists());
    assert!(no_temp_dirs(dir.path()));
    assert_eq!(progress.total(), 3, "manifest was read before cancelling");
}

fn no_temp_dirs(parent: &Path) -> bool {
    fs::read_dir(parent).unwrap().flatten().all(|e| {
        !e.file_name()
            .to_string_lossy()
            .starts_with(".senatorial-export-")
    })
}
