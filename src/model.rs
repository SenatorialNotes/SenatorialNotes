use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_yaml::Value;
use uuid::Uuid;
use zeroize::Zeroize;

use crate::{Error, Result};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NoteMetadata {
    pub id: Uuid,
    pub title: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub archived: bool,
    #[serde(flatten)]
    pub unknown: BTreeMap<String, Value>,
}

impl NoteMetadata {
    pub fn new(title: impl Into<String>) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            title: title.into(),
            created_at: now,
            updated_at: now,
            tags: Vec::new(),
            pinned: false,
            archived: false,
            unknown: BTreeMap::new(),
        }
    }

    /// Adds `tag` unless an existing tag already matches it case-insensitively
    /// (after trimming). Returns `true` if a new tag was added. The casing a
    /// user first saved a tag under is never rewritten just because a later
    /// add used different casing.
    pub fn add_tag(&mut self, tag: &str) -> bool {
        let trimmed = tag.trim();
        if trimmed.is_empty() {
            return false;
        }
        let already_present = self
            .tags
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(trimmed) || existing == trimmed);
        // `eq_ignore_ascii_case` only folds ASCII; fall back to a full
        // lowercase comparison so non-ASCII tags (e.g. accented words) are
        // still deduplicated case-insensitively.
        let already_present = already_present
            || self
                .tags
                .iter()
                .any(|existing| existing.to_lowercase() == trimmed.to_lowercase());
        if already_present {
            return false;
        }
        self.tags.push(trimmed.to_string());
        true
    }

    /// Removes any tag matching `tag` case-insensitively. Returns `true` if a
    /// tag was removed.
    pub fn remove_tag(&mut self, tag: &str) -> bool {
        let trimmed = tag.trim();
        let before = self.tags.len();
        self.tags
            .retain(|existing| existing.to_lowercase() != trimmed.to_lowercase());
        self.tags.len() != before
    }

    pub fn clear_sensitive(&mut self) {
        self.title.zeroize();
        for tag in &mut self.tags {
            tag.zeroize();
        }
        for value in self.unknown.values_mut() {
            clear_yaml_value(value);
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Note {
    pub metadata: NoteMetadata,
    pub body: String,
    /// Relative to the vault's `Notes` directory.
    pub relative_path: PathBuf,
}

impl Note {
    pub fn new(title: impl Into<String>, relative_path: PathBuf) -> Self {
        Self {
            metadata: NoteMetadata::new(title),
            body: String::new(),
            relative_path,
        }
    }

    pub fn parse(markdown: &str, relative_path: PathBuf) -> Result<Self> {
        let normalized = markdown.strip_prefix('\u{feff}').unwrap_or(markdown);
        let Some(after_marker) = normalized.strip_prefix("---\n") else {
            return Err(Error::InvalidFrontMatter(
                "the file must start with a YAML front-matter marker".into(),
            ));
        };
        let Some(end) = after_marker.find("\n---\n") else {
            return Err(Error::InvalidFrontMatter(
                "the closing YAML front-matter marker is missing".into(),
            ));
        };

        let metadata = serde_yaml::from_str(&after_marker[..end])
            .map_err(|error| Error::InvalidFrontMatter(error.to_string()))?;
        let body = after_marker[end + "\n---\n".len()..].to_owned();

        Ok(Self {
            metadata,
            body,
            relative_path,
        })
    }

    pub fn to_markdown(&self) -> Result<String> {
        let yaml = serde_yaml::to_string(&self.metadata)
            .map_err(|error| Error::InvalidFrontMatter(error.to_string()))?;
        let yaml = yaml.strip_prefix("---\n").unwrap_or(&yaml);
        let yaml = yaml.strip_suffix("...\n").unwrap_or(yaml);
        Ok(format!("---\n{}\n---\n{}", yaml.trim_end(), self.body))
    }

    pub fn clear_sensitive(&mut self) {
        self.metadata.clear_sensitive();
        self.body.zeroize();
    }
}

fn clear_yaml_value(value: &mut Value) {
    match value {
        Value::String(text) => text.zeroize(),
        Value::Sequence(values) => {
            for value in values {
                clear_yaml_value(value);
            }
        }
        Value::Mapping(values) => {
            let old = std::mem::take(values);
            for (mut key, mut value) in old {
                clear_yaml_value(&mut key);
                clear_yaml_value(&mut value);
            }
        }
        Value::Tagged(tagged) => clear_yaml_value(&mut tagged.value),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct NoteSummary {
    pub id: Uuid,
    pub title: String,
    pub updated_at: DateTime<Utc>,
    pub relative_path: PathBuf,
    pub preview: String,
    /// Full plaintext body, kept in memory only so local search can match note
    /// contents. Always empty for locked encrypted notes so their plaintext
    /// never reaches the search path.
    pub body: String,
    /// Plaintext tags, kept in memory for the same reason. Empty when locked.
    pub tags: Vec<String>,
    pub encrypted: bool,
    pub pinned: bool,
    /// Whether the note is archived. For a locked encrypted note this is
    /// always `false`: the real value lives inside the encrypted payload and
    /// SenatorialNotes never guesses or leaks it while locked. See
    /// [`NoteSummary::locked`].
    pub archived: bool,
    /// Whether SenatorialNotes currently lacks plaintext for this note - true
    /// only for a locked encrypted note (from [`NoteSummary::locked`]), never
    /// for a plaintext note or a currently-unlocked encrypted one. Distinct
    /// from `encrypted`, which only says the note *is* a `.snote` file and
    /// stays true whether it is locked or unlocked. Smart views built on a
    /// protected field (pinned, archived, recency) must check this before
    /// trusting that field, since a locked note's copy of it is a
    /// non-committal placeholder, not real data - see `pinned`/`archived`.
    pub locked: bool,
}

impl From<&Note> for NoteSummary {
    fn from(note: &Note) -> Self {
        let preview = note
            .body
            .split_whitespace()
            .take(24)
            .collect::<Vec<_>>()
            .join(" ");
        Self {
            id: note.metadata.id,
            title: note.metadata.title.clone(),
            updated_at: note.metadata.updated_at,
            relative_path: note.relative_path.clone(),
            preview,
            body: note.body.clone(),
            tags: note.metadata.tags.clone(),
            encrypted: false,
            pinned: note.metadata.pinned,
            archived: note.metadata.archived,
            locked: false,
        }
    }
}

impl NoteSummary {
    /// Placeholder summary for a locked encrypted note. Every field the
    /// encrypted payload protects (title, preview, body, tags, pinned,
    /// archived) is a fixed, non-committal value — never the real one and
    /// never persisted separately — so locked notes can be listed without
    /// decrypting them and without smart views built from a protected field
    /// (Pinned, Archive, Recently Edited) claiming to know something they
    /// cannot. See the "Locked encrypted notes" note in `SECURITY.md`.
    pub fn locked(id: Uuid, relative_path: PathBuf) -> Self {
        Self {
            id,
            title: "Locked Note".into(),
            updated_at: DateTime::<Utc>::UNIX_EPOCH,
            relative_path,
            preview: "Encrypted — unlock to view".into(),
            body: String::new(),
            tags: Vec::new(),
            encrypted: true,
            pinned: false,
            archived: false,
            locked: true,
        }
    }
}
