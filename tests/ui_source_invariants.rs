#[test]
fn gui_source_keeps_known_reentrant_borrow_patterns_out() {
    let source = include_str!("../src/ui.rs");
    for forbidden in [
        "match state.borrow().flow.view()",
        "state.borrow_mut().loading",
        "state.borrow().loading",
        "list_for_click.select_row",
        "try_borrow_mut()",
        "try_borrow()",
    ] {
        assert!(
            !source.contains(forbidden),
            "re-entrant GUI borrow pattern returned: {forbidden}"
        );
    }
}

#[test]
fn user_triggered_gui_source_has_no_process_aborting_shortcuts() {
    let source = include_str!("../src/ui.rs");
    for forbidden in [".unwrap()", ".expect(", "panic!(", "unreachable!("] {
        assert!(
            !source.contains(forbidden),
            "GUI source contains a process-aborting shortcut: {forbidden}"
        );
    }
}

#[test]
fn context_menu_actions_are_uuid_targeted_and_do_not_force_selection() {
    let source = include_str!("../src/ui.rs");
    assert!(source.contains("append_targeted_menu_item"));
    assert!(source.contains("app.context-move-to-trash"));
    assert!(source.contains("app.context-restore"));
    assert!(!source.contains("list_for_click.select_row"));
}

#[test]
fn context_menu_uses_one_shared_popover_owned_by_the_list() {
    let source = include_str!("../src/ui.rs");
    // Exactly one PopoverMenu, parented to the long-lived list, never to a row.
    assert!(source.contains("row_menu.set_parent(&note_list)"));
    assert!(!source.contains("popover.set_parent(&row)"));
    assert!(!source.contains("PopoverMenu::from_model(Some(&menu))"));
    // Rows are held weakly by the gesture so removed rows finalize cleanly.
    assert!(source.contains("let anchor = row.downgrade();"));
}

#[test]
fn programmatic_editor_replacement_does_not_fight_gtksourceview_undo() {
    let source = include_str!("../src/ui.rs");
    // Programmatic loads disable undo around set_text instead of nesting an
    // irreversible action inside an active user action.
    assert!(source.contains("fn set_buffer_text_silently"));
    assert!(source.contains("buffer.set_enable_undo(false)"));
    // Formatting edits the changed span instead of replacing the whole buffer.
    assert!(source.contains("buffer.delete(&mut delete_start, &mut delete_end)"));
    assert!(source.contains("buffer.insert(&mut insert_at, replacement)"));
}

#[test]
fn password_prompts_do_not_use_self_closing_dialogs() {
    let source = include_str!("../src/ui.rs");
    // adw::MessageDialog closes before its response fires, which made rejected
    // passwords silently no-op. The prompts are now windows we control.
    assert!(!source.contains("adw::MessageDialog::new"));
    assert!(source.contains("fn present_password_dialog"));
    assert!(source.contains("MIN_PASSWORD_LENGTH"));
}
