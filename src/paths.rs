use std::path::{Component, Path, PathBuf};

use directories::{BaseDirs, UserDirs};

use crate::{Error, Result};

/// The managed workspace root SenatorialNotes creates on first run, e.g.
/// `~/Documents/SenatorialNotes/`. Uses the platform's Documents directory via
/// the `directories` abstraction (XDG `XDG_DOCUMENTS_DIR` on Linux); falls back
/// to `<home>/SenatorialNotes/` only when no Documents directory is known.
pub fn default_workspace_root() -> Option<PathBuf> {
    if let Some(documents) =
        UserDirs::new().and_then(|dirs| dirs.document_dir().map(Path::to_path_buf))
    {
        return Some(documents.join("SenatorialNotes"));
    }
    BaseDirs::new().map(|base| base.home_dir().join("SenatorialNotes"))
}

pub fn sanitize_title(title: &str) -> String {
    let mut slug = String::with_capacity(title.len());
    let mut pending_dash = false;

    for character in title.chars() {
        if character.is_alphanumeric() {
            if pending_dash && !slug.is_empty() {
                slug.push('-');
            }
            for lower in character.to_lowercase() {
                slug.push(lower);
            }
            pending_dash = false;
        } else {
            pending_dash = true;
        }
    }

    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "untitled".into()
    } else {
        slug.chars().take(80).collect()
    }
}

pub fn note_filename(title: &str, id: uuid::Uuid) -> String {
    let short_id: String = id.simple().to_string().chars().take(8).collect();
    format!("{}--{short_id}.md", sanitize_title(title))
}

pub fn encrypted_note_filename(id: uuid::Uuid) -> String {
    let short_id: String = id.simple().to_string().chars().take(8).collect();
    format!("encrypted--{short_id}.snote")
}

pub fn validate_notebook_path(path: &Path) -> Result<PathBuf> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(Error::InvalidPath(path.display().to_string()));
    }

    for component in path.components() {
        match component {
            Component::Normal(value) if !value.is_empty() => {}
            _ => return Err(Error::InvalidPath(path.display().to_string())),
        }
    }

    Ok(path.to_path_buf())
}

/// Validates a single notebook path component (not a full path) supplied by
/// the user for a rename or a new-child-notebook name, e.g. from a text
/// entry. Rejects empty names, `.`/`..`, and any path separator.
pub fn validate_notebook_name(name: &str) -> Result<String> {
    let trimmed = name.trim();
    if trimmed.is_empty()
        || trimmed == "."
        || trimmed == ".."
        || trimmed.contains('/')
        || trimmed.contains('\\')
    {
        return Err(Error::InvalidPath(name.to_string()));
    }
    Ok(trimmed.to_string())
}

pub fn ensure_relative_note_path(path: &Path) -> Result<PathBuf> {
    let checked = validate_notebook_path(path)?;
    if !matches!(
        checked.extension().and_then(|value| value.to_str()),
        Some("md" | "snote")
    ) {
        return Err(Error::InvalidPath(path.display().to_string()));
    }
    Ok(checked)
}
