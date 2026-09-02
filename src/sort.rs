//! Pure note-list ordering, kept separate from filtering/search.
//!
//! Sorting only ever reorders an in-memory `Vec<NoteSummary>` - it never
//! reads or writes a note file, so switching sort order can never change a
//! file's modification time.

use crate::config::SortOrder;
use crate::model::NoteSummary;

/// Orders `notes` in place.
///
/// `order = None` means no explicit preference has been saved: behavior is
/// byte-for-byte the v0.1 default (pinned-first, then most-recently-updated,
/// then case-insensitive title). `order = Some(_)` is an explicit choice, so
/// that field becomes the sole primary key, and pinned-first is dropped
/// (silently regrouping by pin would contradict what the user just asked
/// for). Either way the note's UUID is the final tie-breaker, so notes that
/// compare equal on the chosen field never produce an order that "flickers"
/// between renders.
pub fn sort_notes(notes: &mut [NoteSummary], order: Option<SortOrder>) {
    match order {
        None => notes.sort_by(|left, right| {
            right
                .pinned
                .cmp(&left.pinned)
                .then_with(|| right.updated_at.cmp(&left.updated_at))
                .then_with(|| left.title.to_lowercase().cmp(&right.title.to_lowercase()))
                .then_with(|| left.id.cmp(&right.id))
        }),
        Some(SortOrder::LastEdited) => notes.sort_by(|left, right| {
            right
                .updated_at
                .cmp(&left.updated_at)
                .then_with(|| left.id.cmp(&right.id))
        }),
        Some(SortOrder::DateCreated) => notes.sort_by(|left, right| {
            right
                .created_at
                .cmp(&left.created_at)
                .then_with(|| left.id.cmp(&right.id))
        }),
        Some(SortOrder::TitleAsc) => notes.sort_by(|left, right| {
            left.title
                .to_lowercase()
                .cmp(&right.title.to_lowercase())
                .then_with(|| left.id.cmp(&right.id))
        }),
        Some(SortOrder::TitleZa) => notes.sort_by(|left, right| {
            right
                .title
                .to_lowercase()
                .cmp(&left.title.to_lowercase())
                .then_with(|| left.id.cmp(&right.id))
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use chrono::{DateTime, Utc};
    use uuid::Uuid;

    use super::*;

    fn summary(title: &str, updated_at_seconds: i64, pinned: bool) -> NoteSummary {
        summary_with_created(title, updated_at_seconds, updated_at_seconds, pinned)
    }

    fn summary_with_created(
        title: &str,
        created_at_seconds: i64,
        updated_at_seconds: i64,
        pinned: bool,
    ) -> NoteSummary {
        NoteSummary {
            id: Uuid::new_v4(),
            title: title.into(),
            created_at: DateTime::<Utc>::from_timestamp(created_at_seconds, 0)
                .expect("valid timestamp"),
            updated_at: DateTime::<Utc>::from_timestamp(updated_at_seconds, 0)
                .expect("valid timestamp"),
            relative_path: PathBuf::from("Inbox/note.md"),
            preview: String::new(),
            body: String::new(),
            tags: Vec::new(),
            encrypted: false,
            pinned,
            favourite: false,
            archived: false,
            locked: false,
        }
    }

    #[test]
    fn default_order_is_pinned_first_then_recency_then_title() {
        let mut notes = vec![
            summary("Zebra", 100, false),
            summary("Apple", 100, true),
            summary("Mango", 200, false),
        ];
        sort_notes(&mut notes, None);
        assert_eq!(
            notes
                .iter()
                .map(|note| note.title.as_str())
                .collect::<Vec<_>>(),
            vec!["Apple", "Mango", "Zebra"]
        );
    }

    #[test]
    fn explicit_last_edited_drops_pinned_first_grouping() {
        let mut notes = vec![summary("Older", 100, true), summary("Newer", 200, false)];
        sort_notes(&mut notes, Some(SortOrder::LastEdited));
        assert_eq!(
            notes
                .iter()
                .map(|note| note.title.as_str())
                .collect::<Vec<_>>(),
            vec!["Newer", "Older"],
            "an explicit sort choice must not silently regroup by pinned state"
        );
    }

    #[test]
    fn title_sorts_are_case_insensitive_in_both_directions() {
        let mut notes = vec![
            summary("banana", 0, false),
            summary("Apple", 0, false),
            summary("cherry", 0, false),
        ];
        sort_notes(&mut notes, Some(SortOrder::TitleAsc));
        assert_eq!(
            notes
                .iter()
                .map(|note| note.title.as_str())
                .collect::<Vec<_>>(),
            vec!["Apple", "banana", "cherry"]
        );
        sort_notes(&mut notes, Some(SortOrder::TitleZa));
        assert_eq!(
            notes
                .iter()
                .map(|note| note.title.as_str())
                .collect::<Vec<_>>(),
            vec!["cherry", "banana", "Apple"]
        );
    }

    #[test]
    fn equal_sort_keys_break_ties_deterministically_by_uuid() {
        let mut first = vec![summary("Same", 100, false), summary("Same", 100, false)];
        let mut second = first.clone();
        sort_notes(&mut first, Some(SortOrder::LastEdited));
        sort_notes(&mut second, Some(SortOrder::LastEdited));
        assert_eq!(
            first.iter().map(|note| note.id).collect::<Vec<_>>(),
            second.iter().map(|note| note.id).collect::<Vec<_>>(),
            "sorting the same input twice must always produce the same order"
        );
    }

    #[test]
    fn date_created_sorts_by_created_at_not_updated_at() {
        // "Old" was created first but edited most recently; "New" was
        // created most recently but never edited again. A DateCreated sort
        // must order by creation time, the opposite of what a LastEdited
        // sort (or the old created_at-less fallback) would produce.
        let mut notes = vec![
            summary_with_created("Old", 100, 900, false),
            summary_with_created("New", 500, 500, false),
        ];
        sort_notes(&mut notes, Some(SortOrder::DateCreated));
        assert_eq!(
            notes
                .iter()
                .map(|note| note.title.as_str())
                .collect::<Vec<_>>(),
            vec!["New", "Old"],
            "DateCreated must sort by created_at (New=500 before Old=100), not updated_at"
        );
    }
}
