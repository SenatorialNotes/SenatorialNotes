//! GUI-independent interaction state.
//!
//! GTK can emit selection signals synchronously while a list is being changed.
//! This module keeps that re-entrancy bookkeeping outside the application's
//! `RefCell<AppState>` so a signal can always determine whether it should be
//! processed without trying to borrow the application model.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};

use uuid::Uuid;

/// Which notes the note list currently shows.
///
/// `Notebook` carries the notebook's path relative to the vault's `Notes`
/// directory. Selecting a notebook shows only notes directly inside it -
/// never its descendants - so nested notebooks stay independently
/// selectable (see `sort`/`ui.rs` filtering).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ViewMode {
    #[default]
    AllNotes,
    Notebook(PathBuf),
    /// Notes the user has recently *opened / viewed* (from
    /// `VaultSessionState::recently_opened`), most-recent first. Deliberately
    /// **not** modification time.
    RecentlyOpened,
    /// Notes the user marked as a favourite (a star). Independent of `Pinned`.
    Favourites,
    Pinned,
    Archive,
    /// Notes that carry their own per-note password (individually encrypted
    /// `.snote` files). This is *not* "every note in a Secure Vault" - a
    /// Secure Vault protects its notes as a whole; this view is only the notes
    /// that additionally have a note password.
    EncryptedNotes,
    Trash,
}

impl ViewMode {
    /// Heading shown above the note list for this view.
    ///
    /// The reserved top-level `Inbox` notebook (see
    /// `Vault::is_reserved_notebook`) displays as "Unfiled" - a friendlier
    /// name for the fallback/default notebook - while its on-disk directory
    /// stays named `Inbox` for backward compatibility with existing vaults.
    /// A user-created notebook that happens to be *nested* under another
    /// notebook and literally named "Inbox" (e.g. `Work/Inbox`) is a
    /// distinct, non-reserved notebook and keeps its own name verbatim.
    pub fn heading(&self) -> String {
        match self {
            Self::AllNotes => "All Notes".to_string(),
            Self::Notebook(path) if path.as_path() == Path::new("Inbox") => "Unfiled".to_string(),
            Self::Notebook(path) => path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("Notebook")
                .to_string(),
            Self::RecentlyOpened => "Recently Opened".to_string(),
            Self::Favourites => "Favourites".to_string(),
            Self::Pinned => "Pinned".to_string(),
            Self::Archive => "Archive".to_string(),
            Self::EncryptedNotes => "Encrypted Notes".to_string(),
            Self::Trash => "Trash".to_string(),
        }
    }

    /// Placeholder text for the search entry in this view. Kept here so every
    /// entry point (opening a vault, switching views, creating a note from a
    /// smart view) resolves it the same way and cannot leave a stale label.
    pub fn search_placeholder(&self) -> String {
        match self {
            Self::AllNotes => "Search notes".to_string(),
            _ => format!("Search {}", self.heading()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowTarget {
    Note(Uuid),
    Trash(Uuid),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionIntent {
    Suppressed,
    Activate(RowTarget),
    None,
}

#[derive(Debug, Default)]
pub struct SelectionCoordinator {
    suppression_depth: Cell<u32>,
    rows: RefCell<Vec<RowTarget>>,
}

impl SelectionCoordinator {
    pub fn suppress(&self) -> SelectionSuppression<'_> {
        self.suppression_depth
            .set(self.suppression_depth.get().saturating_add(1));
        SelectionSuppression { coordinator: self }
    }

    pub fn is_suppressed(&self) -> bool {
        self.suppression_depth.get() > 0
    }

    pub fn replace_rows(&self, rows: Vec<RowTarget>) {
        *self.rows.borrow_mut() = rows;
    }

    pub fn insert_row(&self, index: usize, target: RowTarget) {
        let mut rows = self.rows.borrow_mut();
        let index = index.min(rows.len());
        rows.insert(index, target);
    }

    pub fn remove_row(&self, target: RowTarget) -> Option<usize> {
        let mut rows = self.rows.borrow_mut();
        let index = rows.iter().position(|candidate| *candidate == target)?;
        rows.remove(index);
        Some(index)
    }

    pub fn index_of(&self, target: RowTarget) -> Option<usize> {
        self.rows
            .borrow()
            .iter()
            .position(|candidate| *candidate == target)
    }

    pub fn target_at(&self, index: i32) -> Option<RowTarget> {
        usize::try_from(index)
            .ok()
            .and_then(|index| self.rows.borrow().get(index).copied())
    }

    pub fn rows(&self) -> Vec<RowTarget> {
        self.rows.borrow().clone()
    }

    pub fn selection_intent(&self, index: i32) -> SelectionIntent {
        if self.is_suppressed() {
            SelectionIntent::Suppressed
        } else {
            self.target_at(index)
                .map(SelectionIntent::Activate)
                .unwrap_or(SelectionIntent::None)
        }
    }
}

pub struct SelectionSuppression<'a> {
    coordinator: &'a SelectionCoordinator,
}

impl Drop for SelectionSuppression<'_> {
    fn drop(&mut self) {
        self.coordinator
            .suppression_depth
            .set(self.coordinator.suppression_depth.get().saturating_sub(1));
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UiFlow {
    view: ViewMode,
    selected_note: Option<Uuid>,
    selected_trash: Option<Uuid>,
}

impl UiFlow {
    pub fn view(&self) -> &ViewMode {
        &self.view
    }

    pub fn selected_note(&self) -> Option<Uuid> {
        self.selected_note
    }

    pub fn selected_trash(&self) -> Option<Uuid> {
        self.selected_trash
    }

    pub fn switch_view(&mut self, view: ViewMode) {
        match view {
            ViewMode::Trash => self.selected_note = None,
            _ => self.selected_trash = None,
        }
        self.view = view;
    }

    pub fn select_note(&mut self, id: Uuid) {
        if self.view == ViewMode::Trash {
            self.view = ViewMode::AllNotes;
        }
        self.selected_note = Some(id);
        self.selected_trash = None;
    }

    pub fn select_trash(&mut self, id: Uuid) {
        self.view = ViewMode::Trash;
        self.selected_trash = Some(id);
        self.selected_note = None;
    }

    pub fn clear_selection(&mut self) {
        self.selected_note = None;
        self.selected_trash = None;
    }

    pub fn note_moved_to_trash(&mut self, id: Uuid) {
        if self.selected_note == Some(id) {
            self.selected_note = None;
        }
    }

    pub fn note_restored(&mut self, id: Uuid) {
        if self.selected_trash == Some(id) {
            self.selected_trash = None;
        }
    }
}

/// Local, non-persistent filtering on top of the active `ViewMode` and the
/// search query. Kept separate from `UiFlow` because it is a plain UI
/// preference, not selection/reentrancy-sensitive state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FilterState {
    active_tag: Option<String>,
}

impl FilterState {
    pub fn active_tag(&self) -> Option<&str> {
        self.active_tag.as_deref()
    }

    pub fn set_active_tag(&mut self, tag: Option<String>) {
        self.active_tag = tag;
    }

    pub fn clear(&mut self) {
        self.active_tag = None;
    }
}

/// Tracks which vault "session" the UI is currently in.
///
/// Every vault open/switch [`bump`](SessionRegistry::bump)s the generation. A
/// deferred callback (an autosave/title timer, the coalesced selection
/// dispatch, a dialog response) records the generation it was armed under and,
/// when it finally runs, calls [`is_current`](SessionRegistry::is_current): if
/// the vault has been switched in the meantime the callback is stale and must
/// do nothing, so it can never mutate the new vault with the old vault's
/// intent. Kept here, outside `RefCell<AppState>`, for the same reason as
/// [`SelectionCoordinator`] — a stale callback can check it without borrowing
/// the application model.
#[derive(Debug, Default)]
pub struct SessionRegistry {
    generation: Cell<u64>,
}

impl SessionRegistry {
    /// The current session generation. Record this when arming a deferred
    /// callback.
    pub fn current(&self) -> u64 {
        self.generation.get()
    }

    /// Advances to a new session generation (on a vault open/switch) and
    /// returns it. Every callback armed under an earlier generation is now
    /// stale.
    pub fn bump(&self) -> u64 {
        let next = self.generation.get().wrapping_add(1);
        self.generation.set(next);
        next
    }

    /// Whether `token` (from a prior [`current`](SessionRegistry::current)) is
    /// still the live session.
    pub fn is_current(&self, token: u64) -> bool {
        self.generation.get() == token
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_generation_marks_earlier_callbacks_stale_after_a_switch() {
        let sessions = SessionRegistry::default();
        // Vault A opens.
        let gen_a = sessions.bump();
        assert!(sessions.is_current(gen_a));

        // An autosave timer is armed under vault A.
        let armed_under_a = sessions.current();

        // The user switches to vault B before the timer fires.
        let gen_b = sessions.bump();
        assert_ne!(gen_a, gen_b);
        assert!(sessions.is_current(gen_b));

        // The stale timer finally fires: it must see itself as no longer current.
        assert!(
            !sessions.is_current(armed_under_a),
            "a callback armed under vault A must be inert after switching to vault B"
        );

        // A fresh callback armed under B is current.
        let armed_under_b = sessions.current();
        assert!(sessions.is_current(armed_under_b));
    }

    #[test]
    fn session_generation_survives_many_switches_and_wraps_safely() {
        let sessions = SessionRegistry::default();
        let mut last = sessions.current();
        for _ in 0..10_000 {
            let next = sessions.bump();
            assert_ne!(next, last);
            assert!(sessions.is_current(next));
            last = next;
        }
    }

    #[test]
    fn programmatic_selection_is_suppressed_until_the_guard_drops() {
        let coordinator = SelectionCoordinator::default();
        let id = Uuid::new_v4();
        coordinator.replace_rows(vec![RowTarget::Note(id)]);

        {
            let _guard = coordinator.suppress();
            assert_eq!(coordinator.selection_intent(0), SelectionIntent::Suppressed);
        }

        assert_eq!(
            coordinator.selection_intent(0),
            SelectionIntent::Activate(RowTarget::Note(id))
        );
    }

    #[test]
    fn nested_suppression_remains_active_until_the_outer_update_finishes() {
        let coordinator = SelectionCoordinator::default();
        let outer = coordinator.suppress();
        {
            let _inner = coordinator.suppress();
            assert!(coordinator.is_suppressed());
        }
        assert!(coordinator.is_suppressed());
        drop(outer);
        assert!(!coordinator.is_suppressed());
    }

    #[test]
    fn every_smart_view_has_a_distinct_heading_and_search_placeholder() {
        assert_eq!(ViewMode::AllNotes.heading(), "All Notes");
        assert_eq!(ViewMode::AllNotes.search_placeholder(), "Search notes");
        assert_eq!(ViewMode::Pinned.heading(), "Pinned");
        assert_eq!(ViewMode::Pinned.search_placeholder(), "Search Pinned");
        assert_eq!(ViewMode::RecentlyOpened.heading(), "Recently Opened");
        assert_eq!(ViewMode::Favourites.heading(), "Favourites");
        assert_eq!(ViewMode::Archive.heading(), "Archive");
        assert_eq!(ViewMode::EncryptedNotes.heading(), "Encrypted Notes");
        assert_eq!(
            ViewMode::EncryptedNotes.search_placeholder(),
            "Search Encrypted Notes"
        );
        assert_eq!(ViewMode::Trash.heading(), "Trash");
        assert_eq!(ViewMode::Trash.search_placeholder(), "Search Trash");
        // "All Notes" must never present another view's placeholder.
        assert_ne!(
            ViewMode::AllNotes.search_placeholder(),
            ViewMode::Pinned.search_placeholder()
        );
    }

    #[test]
    fn notebook_heading_is_the_notebook_s_own_name_not_its_full_path() {
        let nested = ViewMode::Notebook(std::path::PathBuf::from("Work/Projects"));
        assert_eq!(nested.heading(), "Projects");
        assert_eq!(nested.search_placeholder(), "Search Projects");
    }

    #[test]
    fn reserved_top_level_inbox_notebook_displays_as_unfiled() {
        // The on-disk directory stays "Inbox" (see `Vault::DEFAULT_NOTEBOOK`
        // and `is_reserved_notebook`) - only the UI-facing heading changes.
        let top_level = ViewMode::Notebook(std::path::PathBuf::from("Inbox"));
        assert_eq!(top_level.heading(), "Unfiled");
        assert_eq!(top_level.search_placeholder(), "Search Unfiled");
    }

    #[test]
    fn a_nested_notebook_literally_named_inbox_keeps_its_own_name() {
        // Only the reserved *top-level* Inbox is renamed for display; a
        // user-created nested notebook that happens to share the name is a
        // distinct, non-reserved notebook and must not be relabeled.
        let nested_inbox = ViewMode::Notebook(std::path::PathBuf::from("Work/Inbox"));
        assert_eq!(nested_inbox.heading(), "Inbox");
    }

    #[test]
    fn notebook_selection_survives_switching_between_note_views() {
        let mut flow = UiFlow::default();
        let id = Uuid::new_v4();
        let inbox = ViewMode::Notebook(std::path::PathBuf::from("Inbox"));
        flow.select_note(id);
        flow.switch_view(inbox.clone());
        assert_eq!(flow.view(), &inbox);
        assert_eq!(flow.selected_note(), Some(id));
        flow.switch_view(ViewMode::AllNotes);
        assert_eq!(flow.selected_note(), Some(id));
    }

    #[test]
    fn switching_to_trash_clears_note_selection_and_back_clears_trash_selection() {
        let mut flow = UiFlow::default();
        let note_id = Uuid::new_v4();
        let trash_id = Uuid::new_v4();
        flow.select_note(note_id);
        flow.switch_view(ViewMode::Trash);
        assert_eq!(flow.selected_note(), None, "leaving a note view clears it");

        flow.select_trash(trash_id);
        flow.switch_view(ViewMode::AllNotes);
        assert_eq!(
            flow.selected_trash(),
            None,
            "leaving Trash for any note view clears the trash selection"
        );
    }

    #[test]
    fn filter_state_starts_with_no_active_tag_and_can_be_cleared() {
        let mut filter = FilterState::default();
        assert_eq!(filter.active_tag(), None);
        filter.set_active_tag(Some("errands".to_string()));
        assert_eq!(filter.active_tag(), Some("errands"));
        filter.clear();
        assert_eq!(filter.active_tag(), None);
    }
}
