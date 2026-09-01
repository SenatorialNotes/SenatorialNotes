//! Local, in-memory note search.
//!
//! Search runs entirely against the `NoteSummary` values already held in memory:
//! there is no network access and no persistent plaintext search database. A
//! locked encrypted note only ever contributes its placeholder title ("Locked
//! Note"); its real title, body, and tags are never populated on the summary, so
//! they cannot match here even if a caller passes a stale value.

use crate::model::NoteSummary;

/// Whether `summary` matches the free-text `query`.
///
/// An empty query matches everything. A non-empty query matches when it is a
/// case-insensitive substring of the title, or - for notes that are not locked
/// encrypted - the body or any tag.
pub fn summary_matches(summary: &NoteSummary, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return true;
    }
    if summary.title.to_lowercase().contains(&query) {
        return true;
    }
    if summary.encrypted {
        // Locked encrypted notes never expose body or tags to search.
        return false;
    }
    if summary.body.to_lowercase().contains(&query) {
        return true;
    }
    summary
        .tags
        .iter()
        .any(|tag| tag.to_lowercase().contains(&query))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use uuid::Uuid;

    use super::*;

    fn summary(title: &str, body: &str, tags: &[&str]) -> NoteSummary {
        NoteSummary {
            id: Uuid::new_v4(),
            title: title.into(),
            updated_at: chrono::DateTime::<chrono::Utc>::UNIX_EPOCH,
            relative_path: PathBuf::from("Inbox/note.md"),
            preview: String::new(),
            body: body.into(),
            tags: tags.iter().map(|tag| (*tag).to_string()).collect(),
            encrypted: false,
            pinned: false,
        }
    }

    #[test]
    fn matches_title_body_and_tags_case_insensitively() {
        let note = summary("Weekly Plan", "Buy MILK and eggs", &["errands", "home"]);
        assert!(summary_matches(&note, ""));
        assert!(summary_matches(&note, "weekly"));
        assert!(summary_matches(&note, "milk"));
        assert!(summary_matches(&note, "ERRANDS"));
        assert!(!summary_matches(&note, "quarterly"));
    }

    #[test]
    fn locked_encrypted_notes_never_match_body_or_tags() {
        // Even if a caller somehow left plaintext on the struct, the encrypted
        // flag alone must prevent body/tag matches from leaking.
        let mut note = summary("Locked Note", "TOP SECRET launch codes", &["classified"]);
        note.encrypted = true;
        assert!(!summary_matches(&note, "secret"));
        assert!(!summary_matches(&note, "classified"));
        // The placeholder title is still searchable.
        assert!(summary_matches(&note, "locked"));
    }
}
