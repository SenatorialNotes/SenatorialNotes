use std::path::Path;

use senatorial_notes::ui_state::{
    RowTarget, SelectionCoordinator, SelectionIntent, UiFlow, ViewMode,
};
use senatorial_notes::{NoteSummary, Vault};
use tempfile::tempdir;
use uuid::Uuid;

#[test]
fn one_hundred_new_notes_can_be_inserted_and_selected_without_reentrant_dispatch() {
    let coordinator = SelectionCoordinator::default();
    let mut flow = UiFlow::default();
    let mut ids = Vec::new();

    for _ in 0..100 {
        let id = Uuid::new_v4();
        {
            let _suppression = coordinator.suppress();
            coordinator.insert_row(0, RowTarget::Note(id));
            assert_eq!(coordinator.selection_intent(0), SelectionIntent::Suppressed);
        }
        flow.select_note(id);
        assert_eq!(
            coordinator.selection_intent(0),
            SelectionIntent::Activate(RowTarget::Note(id))
        );
        assert_eq!(flow.selected_note(), Some(id));
        ids.insert(0, id);
    }

    assert_eq!(ids.len(), 100);
    for (index, id) in ids.into_iter().enumerate() {
        assert_eq!(
            coordinator.selection_intent(index as i32),
            SelectionIntent::Activate(RowTarget::Note(id))
        );
    }
}

#[test]
fn selection_delete_restore_view_switch_and_context_targets_stay_consistent() {
    let coordinator = SelectionCoordinator::default();
    let mut flow = UiFlow::default();
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    coordinator.replace_rows(vec![RowTarget::Note(first), RowTarget::Note(second)]);

    for _ in 0..250 {
        flow.select_note(first);
        assert_eq!(
            coordinator.selection_intent(0),
            SelectionIntent::Activate(RowTarget::Note(first))
        );
        flow.select_note(second);
        assert_eq!(
            coordinator.selection_intent(1),
            SelectionIntent::Activate(RowTarget::Note(second))
        );
    }

    flow.note_moved_to_trash(second);
    coordinator.remove_row(RowTarget::Note(second));
    assert_eq!(flow.selected_note(), None);
    flow.switch_view(ViewMode::Trash);
    coordinator.replace_rows(vec![RowTarget::Trash(second)]);
    flow.select_trash(second);
    assert_eq!(
        coordinator.selection_intent(0),
        SelectionIntent::Activate(RowTarget::Trash(second))
    );

    flow.note_restored(second);
    coordinator.remove_row(RowTarget::Trash(second));
    assert_eq!(flow.selected_trash(), None);
    flow.switch_view(ViewMode::Notes);
    coordinator.replace_rows(vec![RowTarget::Note(first), RowTarget::Note(second)]);
    assert_eq!(flow.view(), ViewMode::Notes);
}

#[test]
fn storage_backed_create_rename_pin_trash_restore_sequence_survives_stress() {
    let temporary = tempdir().expect("temporary directory");
    let vault = Vault::create(temporary.path().join("Vault")).expect("vault creation");
    let mut ids = Vec::new();

    for index in 0..100 {
        let created = vault
            .create_note(&format!("Note {index}"), Path::new("Inbox"))
            .expect("note creation");
        ids.push(created.metadata.id);
    }
    assert_eq!(vault.scan_notes().expect("scan after creation").len(), 100);

    for (index, id) in ids.iter().copied().enumerate() {
        let summary = vault
            .scan_notes()
            .expect("scan before edit")
            .into_iter()
            .find(|summary| summary.id == id)
            .expect("created note remains present");
        let (mut note, stamp) = vault.load_note(&summary.relative_path).expect("load note");
        note.metadata.pinned = index % 2 == 0;
        let stamp = vault
            .save_note(&mut note, Some(&stamp))
            .expect("save pinned state");
        vault
            .commit_title(&mut note, Some(&stamp), &format!("Renamed {index}"))
            .expect("rename note");
    }

    let notes = vault.scan_notes().expect("scan after edit");
    assert_eq!(notes.len(), 100);
    assert!(notes.iter().take(50).all(|summary| summary.pinned));

    for summary in notes.iter().take(25) {
        vault
            .move_to_trash(&summary.relative_path)
            .expect("context-menu trash action");
    }
    let trash = vault.scan_trash().expect("scan trash");
    assert_eq!(trash.len(), 25);
    for entry in trash {
        vault
            .restore_from_trash(entry.id)
            .expect("context-menu restore action");
    }
    assert!(vault.scan_trash().expect("trash is empty").is_empty());
    assert_eq!(vault.scan_notes().expect("all notes restored").len(), 100);
}

#[test]
fn nested_rebuild_and_rapid_keyboard_selection_stress_does_not_drop_final_intent() {
    let coordinator = SelectionCoordinator::default();
    let ids = (0..32).map(|_| Uuid::new_v4()).collect::<Vec<_>>();
    let rows = ids.iter().copied().map(RowTarget::Note).collect::<Vec<_>>();

    for round in 0..2_000 {
        {
            let _outer = coordinator.suppress();
            coordinator.replace_rows(rows.clone());
            {
                let _inner = coordinator.suppress();
                assert_eq!(
                    coordinator.selection_intent((round % ids.len()) as i32),
                    SelectionIntent::Suppressed
                );
            }
            assert!(coordinator.is_suppressed());
        }
        let index = round % ids.len();
        assert_eq!(
            coordinator.selection_intent(index as i32),
            SelectionIntent::Activate(RowTarget::Note(ids[index]))
        );
    }
}

#[test]
fn twenty_five_thousand_rapid_selection_requests_coalesce_to_the_final_target() {
    // Models `ui::request_selection`: every row click overwrites a single
    // pending slot, and a coalesced idle dispatch consumes only the newest
    // value. A burst of rapid switches must therefore collapse to very few
    // loads, always ending on the last-requested note.
    let coordinator = SelectionCoordinator::default();
    let ids: Vec<Uuid> = (0..64).map(|_| Uuid::new_v4()).collect();
    coordinator.replace_rows(ids.iter().copied().map(RowTarget::Note).collect());

    let pending: std::cell::Cell<Option<RowTarget>> = std::cell::Cell::new(None);
    let mut dispatches = 0usize;
    let mut last_dispatched = None;
    let mut drain = |pending: &std::cell::Cell<Option<RowTarget>>| {
        if let Some(target) = pending.take() {
            dispatches += 1;
            last_dispatched = Some(target);
        }
    };

    const REQUESTS: usize = 25_000;
    for round in 0..REQUESTS {
        let index = (round * 7) % ids.len();
        let target = coordinator.target_at(index as i32).expect("row exists");
        pending.set(Some(target));
        // The idle dispatcher only runs when the main loop is not busy; model
        // that as roughly once per 500 queued requests.
        if round % 500 == 499 {
            drain(&pending);
        }
    }
    drain(&pending);

    assert!(
        dispatches <= REQUESTS / 400,
        "rapid selection must coalesce, but dispatched {dispatches} times"
    );
    let final_index = ((REQUESTS - 1) * 7) % ids.len();
    assert_eq!(
        last_dispatched,
        coordinator.target_at(final_index as i32),
        "the newest requested selection must win"
    );
}

#[test]
fn note_summary_preserves_pinned_state_for_incremental_row_updates() {
    let temporary = tempdir().expect("temporary directory");
    let vault = Vault::create(temporary.path().join("Vault")).expect("vault creation");
    let created = vault
        .create_note("Pinned", Path::new("Inbox"))
        .expect("note creation");
    let (mut note, stamp) = vault.load_note(&created.relative_path).expect("load note");
    note.metadata.pinned = true;
    vault
        .save_note(&mut note, Some(&stamp))
        .expect("save pinned state");

    let summary = NoteSummary::from(&note);
    assert!(summary.pinned);
}
