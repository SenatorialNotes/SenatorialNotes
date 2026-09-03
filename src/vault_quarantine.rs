//! R18 — detection and explicit, user-consented quarantine of plaintext storage
//! artifacts that an old or incompatible SenatorialNotes binary wrote into the
//! root of a Secure (whole-vault-encrypted) vault.
//!
//! A `v0.1` / `v0.2` binary never reads `vault.toml`. Opening a `format_version`
//! 3 Secure Vault it runs its unconditional "create the standard directories"
//! loop and, if the user makes a note, writes a plaintext `.md` into a
//! *top-level* `Notes/` tree. The encrypted object store lives entirely under
//! `.senatorial-notes/store/`, so ciphertext and plaintext never share a
//! directory — but the stray plaintext must not be left where a future
//! old-binary run could grow it, and it must **never** be merged into, parsed
//! as, or overwritten by encrypted storage.
//!
//! This module never moves or deletes anything on its own. [`detect`] only
//! looks. [`PendingQuarantine::quarantine`] moves the offending artifacts —
//! byte-for-byte unchanged, by same-filesystem rename — into
//! `.senatorial-notes/quarantine/<timestamp>/`, and only a UI caller acting on
//! an explicit user choice ever calls it. Nothing under `.senatorial-notes/` is
//! ever inspected or moved (the quarantine destination is the sole write there).

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use crate::constants::VAULT_STATE_DIR;
use crate::error::io_error;
use crate::vault::create_private_directory;
use crate::{Error, Result};

const QUARANTINE_DIR: &str = "quarantine";
const LOOSE_FILES_DIR: &str = "vault-root-files";

/// A category of ordinary-storage artifact found in a Secure Vault's root.
///
/// Categories describe *shape*, never content: no note title, body, tag, or
/// filename is recorded here or in any log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArtifactCategory {
    /// A top-level `Notes/` directory containing at least one `.md` / `.snote`.
    NotesDirectory,
    /// A top-level `Trash/` directory containing at least one `.md` / `.snote`.
    TrashDirectory,
    /// A top-level `Attachments/` directory containing at least one regular file.
    AttachmentsDirectory,
    /// One or more `.md` files directly in the vault root.
    StrayRootMarkdown,
    /// One or more `.snote` files directly in the vault root.
    StrayRootSnote,
}

impl ArtifactCategory {
    /// A short, content-free description for a dialog.
    pub fn describe(self) -> &'static str {
        match self {
            Self::NotesDirectory => "a plaintext \u{201c}Notes\u{201d} folder",
            Self::TrashDirectory => "a plaintext \u{201c}Trash\u{201d} folder",
            Self::AttachmentsDirectory => "an \u{201c}Attachments\u{201d} folder with files",
            Self::StrayRootMarkdown => "loose .md files in the vault folder",
            Self::StrayRootSnote => "loose .snote files in the vault folder",
        }
    }
}

impl fmt::Display for ArtifactCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.describe())
    }
}

/// Plaintext storage artifacts detected in a Secure Vault root, not yet moved.
///
/// Held by [`crate::Vault`] for an affected Secure Vault; while it is present
/// the vault is forced read-only and every mutating operation is refused. The
/// UI surfaces the conflict and only calls [`PendingQuarantine::quarantine`]
/// after the user explicitly chooses to.
#[derive(Clone, Debug)]
pub struct PendingQuarantine {
    root: PathBuf,
    categories: Vec<ArtifactCategory>,
    /// Absolute paths of top-level directories to move wholesale.
    directories: Vec<PathBuf>,
    /// Absolute paths of individual stray files in the root to move.
    files: Vec<PathBuf>,
    file_count: usize,
}

/// What a completed [`PendingQuarantine::quarantine`] moved, and where.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuarantineReport {
    /// The vault root the artifacts were moved out of.
    pub original_root: PathBuf,
    /// The `.senatorial-notes/quarantine/<timestamp>/` directory they now live in.
    pub quarantine_path: PathBuf,
    /// Total number of user-data files moved.
    pub file_count: usize,
    /// The categories that were found and moved.
    pub categories: Vec<ArtifactCategory>,
}

/// Looks for ordinary-storage artifacts in `root` that indicate an old or
/// incompatible binary wrote into this Secure Vault. Reads only; moves nothing.
///
/// Returns `None` when the root is clean. Empty legacy directories on their own
/// (an old binary's "create the standard directories" loop with no note ever
/// made) are **not** a conflict, and unrelated files such as `README.txt` never
/// trigger it. `.senatorial-notes/` is never looked inside.
pub fn detect(root: &Path) -> Option<PendingQuarantine> {
    let mut categories = Vec::new();
    let mut directories = Vec::new();
    let mut files = Vec::new();
    let mut file_count = 0_usize;

    for (name, category) in [
        ("Notes", ArtifactCategory::NotesDirectory),
        ("Trash", ArtifactCategory::TrashDirectory),
    ] {
        let dir = root.join(name);
        if dir.is_dir() {
            let count = count_note_files(&dir);
            if count > 0 {
                categories.push(category);
                directories.push(dir);
                file_count += count;
            }
        }
    }

    let attachments = root.join("Attachments");
    if attachments.is_dir() {
        let count = count_regular_files(&attachments);
        if count > 0 {
            categories.push(ArtifactCategory::AttachmentsDirectory);
            directories.push(attachments);
            file_count += count;
        }
    }

    let mut stray_md = Vec::new();
    let mut stray_snote = Vec::new();
    if let Ok(entries) = fs::read_dir(root) {
        for entry in entries.flatten() {
            if entry.file_name().to_str() == Some(VAULT_STATE_DIR) {
                continue;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_file() {
                continue;
            }
            match entry.path().extension().and_then(|value| value.to_str()) {
                Some("md") => stray_md.push(entry.path()),
                Some("snote") => stray_snote.push(entry.path()),
                _ => {}
            }
        }
    }
    if !stray_md.is_empty() {
        categories.push(ArtifactCategory::StrayRootMarkdown);
        file_count += stray_md.len();
        files.append(&mut stray_md);
    }
    if !stray_snote.is_empty() {
        categories.push(ArtifactCategory::StrayRootSnote);
        file_count += stray_snote.len();
        files.append(&mut stray_snote);
    }

    if categories.is_empty() {
        return None;
    }
    Some(PendingQuarantine {
        root: root.to_path_buf(),
        categories,
        directories,
        files,
        file_count,
    })
}

impl PendingQuarantine {
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn categories(&self) -> &[ArtifactCategory] {
        &self.categories
    }

    pub fn file_count(&self) -> usize {
        self.file_count
    }

    /// A one-line, content-free summary for a status line.
    pub fn summary(&self) -> String {
        let parts: Vec<&str> = self.categories.iter().map(|c| c.describe()).collect();
        format!(
            "{} plaintext file(s) found in this Secure Vault ({})",
            self.file_count,
            parts.join(", ")
        )
    }

    /// Moves every detected artifact, unchanged, into
    /// `<root>/.senatorial-notes/quarantine/<timestamp>/` by same-filesystem
    /// rename, then re-scans the root and fails if anything remains.
    ///
    /// Nothing is ever deleted, parsed, merged, or overwritten. On a rename
    /// failure the error names the exact path; artifacts already moved stay in
    /// the quarantine directory (still on disk, nothing lost) and the caller
    /// keeps the vault unopened or read-only.
    pub fn quarantine(&self) -> Result<QuarantineReport> {
        let base = self.root.join(VAULT_STATE_DIR).join(QUARANTINE_DIR);
        create_private_directory(&base)?;

        let stamp = timestamp();
        let mut dest = base.join(&stamp);
        let mut suffix = 2;
        while dest.exists() {
            dest = base.join(format!("{stamp}-{suffix}"));
            suffix += 1;
        }
        create_private_directory(&dest)?;

        for dir in &self.directories {
            let Some(name) = dir.file_name() else {
                return Err(Error::Quarantine(format!(
                    "{} has no final path component",
                    dir.display()
                )));
            };
            let target = dest.join(name);
            fs::rename(dir, &target).map_err(|source| io_error(dir, source))?;
        }

        if !self.files.is_empty() {
            let loose = dest.join(LOOSE_FILES_DIR);
            create_private_directory(&loose)?;
            for file in &self.files {
                let Some(name) = file.file_name() else {
                    return Err(Error::Quarantine(format!(
                        "{} has no final path component",
                        file.display()
                    )));
                };
                let target = loose.join(name);
                fs::rename(file, &target).map_err(|source| io_error(file, source))?;
            }
        }

        if let Some(remaining) = detect(&self.root) {
            return Err(Error::Quarantine(format!(
                "artifacts remain after the move ({})",
                remaining.summary()
            )));
        }

        Ok(QuarantineReport {
            original_root: self.root.clone(),
            quarantine_path: dest,
            file_count: self.file_count,
            categories: self.categories.clone(),
        })
    }
}

/// A filesystem-safe UTC timestamp, e.g. `20260903T142501Z`.
fn timestamp() -> String {
    chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string()
}

/// Recursively counts `.md` / `.snote` regular files under `dir` (symlinks are
/// never followed).
fn count_note_files(dir: &Path) -> usize {
    count_files(dir, &|path| {
        matches!(
            path.extension().and_then(|value| value.to_str()),
            Some("md" | "snote")
        )
    })
}

/// Recursively counts every regular file under `dir` (symlinks are never
/// followed).
fn count_regular_files(dir: &Path) -> usize {
    count_files(dir, &|_| true)
}

fn count_files(dir: &Path, accept: &dyn Fn(&Path) -> bool) -> usize {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let mut count = 0;
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        if file_type.is_dir() {
            count += count_files(&path, accept);
        } else if file_type.is_file() && accept(&path) {
            count += 1;
        }
    }
    count
}
