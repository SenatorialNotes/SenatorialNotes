//! GUI-independent interaction state.
//!
//! GTK can emit selection signals synchronously while a list is being changed.
//! This module keeps that re-entrancy bookkeeping outside the application's
//! `RefCell<AppState>` so a signal can always determine whether it should be
//! processed without trying to borrow the application model.

use std::cell::{Cell, RefCell};

use uuid::Uuid;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ViewMode {
    #[default]
    Notes,
    Inbox,
    Trash,
}

impl ViewMode {
    /// Heading shown above the note list for this view.
    pub fn heading(self) -> &'static str {
        match self {
            Self::Notes => "All Notes",
            Self::Inbox => "Inbox",
            Self::Trash => "Trash",
        }
    }

    /// Placeholder text for the search entry in this view. Kept here so every
    /// entry point (opening a vault, switching views, creating a note from a
    /// smart view) resolves it the same way and cannot leave a stale label.
    pub fn search_placeholder(self) -> &'static str {
        match self {
            Self::Notes => "Search notes",
            Self::Inbox => "Search Inbox",
            Self::Trash => "Search Trash",
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UiFlow {
    view: ViewMode,
    selected_note: Option<Uuid>,
    selected_trash: Option<Uuid>,
}

impl UiFlow {
    pub fn view(self) -> ViewMode {
        self.view
    }

    pub fn selected_note(self) -> Option<Uuid> {
        self.selected_note
    }

    pub fn selected_trash(self) -> Option<Uuid> {
        self.selected_trash
    }

    pub fn switch_view(&mut self, view: ViewMode) {
        self.view = view;
        match view {
            ViewMode::Notes | ViewMode::Inbox => self.selected_trash = None,
            ViewMode::Trash => self.selected_note = None,
        }
    }

    pub fn select_note(&mut self, id: Uuid) {
        if self.view == ViewMode::Trash {
            self.view = ViewMode::Notes;
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn every_view_has_a_distinct_heading_and_search_placeholder() {
        assert_eq!(ViewMode::Notes.heading(), "All Notes");
        assert_eq!(ViewMode::Notes.search_placeholder(), "Search notes");
        assert_eq!(ViewMode::Inbox.search_placeholder(), "Search Inbox");
        assert_eq!(ViewMode::Trash.search_placeholder(), "Search Trash");
        // "All Notes" must never present the Inbox placeholder.
        assert_ne!(
            ViewMode::Notes.search_placeholder(),
            ViewMode::Inbox.search_placeholder()
        );
    }

    #[test]
    fn inbox_selection_survives_switching_between_note_views() {
        let mut flow = UiFlow::default();
        let id = Uuid::new_v4();
        flow.select_note(id);
        flow.switch_view(ViewMode::Inbox);
        assert_eq!(flow.view(), ViewMode::Inbox);
        assert_eq!(flow.selected_note(), Some(id));
        flow.switch_view(ViewMode::Notes);
        assert_eq!(flow.selected_note(), Some(id));
    }
}
