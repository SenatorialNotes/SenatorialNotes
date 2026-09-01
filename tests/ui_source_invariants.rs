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

#[test]
fn moving_a_note_flushes_pending_edits_before_touching_the_filesystem() {
    let source = include_str!("../src/ui.rs");
    let start = source
        .find("fn move_note_by_id")
        .expect("move_note_by_id should exist");
    let end = source[start..]
        .find("\nfn ")
        .map_or(source.len(), |offset| start + offset);
    let body = &source[start..end];
    let persist_at = body
        .find("persist_active")
        .expect("move_note_by_id must flush pending edits with persist_active");
    let move_at = body
        .find("vault.move_note(")
        .expect("move_note_by_id must call vault.move_note");
    assert!(
        persist_at < move_at,
        "persist_active must run before vault.move_note, so a pending autosave/title-commit \
         can never fire against the path after it has moved"
    );
    // Every runtime structure keyed by the note's old path is rebound after
    // the move, not left stale.
    assert!(body.contains("refresh_watch_baseline"));
    assert!(body.contains("state.plain_cache.get_mut"));
    assert!(body.contains("state.unlocked_cache.get_mut"));
}

#[test]
fn renaming_a_notebook_flushes_pending_edits_before_touching_the_filesystem() {
    let source = include_str!("../src/ui.rs");
    let start = source
        .find("fn present_rename_notebook_dialog")
        .expect("present_rename_notebook_dialog should exist");
    let end = source[start..]
        .find("\nfn ")
        .map_or(source.len(), |offset| start + offset);
    let body = &source[start..end];
    let persist_at = body
        .find("persist_active")
        .expect("renaming a notebook must flush pending edits with persist_active");
    let rename_at = body
        .find("vault.rename_notebook(")
        .expect("must call vault.rename_notebook");
    assert!(
        persist_at < rename_at,
        "persist_active must run before vault.rename_notebook"
    );
}

#[test]
fn locking_encrypted_notes_resets_their_summaries_to_the_locked_placeholder() {
    let source = include_str!("../src/ui.rs");
    let start = source
        .find("fn lock_all_encrypted")
        .expect("lock_all_encrypted should exist");
    let end = source[start..]
        .find("\nfn ")
        .map_or(source.len(), |offset| start + offset);
    let body = &source[start..end];
    // Every note this call locks must have its in-memory summary reset to
    // the same non-committal placeholder a fresh scan would produce - never
    // left holding whatever pinned/archived/title value was true a moment
    // ago (see NoteSummary::locked and its "Locked encrypted notes" doc).
    assert!(body.contains("NoteSummary::locked("));
    // A locked note that no longer belongs in a protected-field smart view
    // must actually leave the list, not just have its row relabeled.
    assert!(body.contains("ViewMode::Pinned | ViewMode::Archive | ViewMode::RecentlyEdited"));
}

#[test]
fn changing_pinned_or_archived_refuses_a_locked_encrypted_note() {
    let source = include_str!("../src/ui.rs");
    let start = source
        .find("fn toggle_note_flag")
        .expect("toggle_note_flag should exist");
    let end = source[start..]
        .find("\nfn ")
        .map_or(source.len(), |offset| start + offset);
    let body = &source[start..end];
    assert!(
        body.contains("summary.encrypted && !active_is_target"),
        "pinned/archived must never be changed on a locked encrypted note - it has no plaintext \
         side channel and must not guess"
    );
}

#[test]
fn deleting_a_notebook_is_confirmed_and_never_uses_remove_dir_all() {
    let source = include_str!("../src/ui.rs");
    assert!(source.contains("fn confirm_delete_notebook"));
    assert!(source.contains("gtk::AlertDialog::builder()"));
    assert!(!source.contains("remove_dir_all"));
}

#[test]
fn note_info_dialog_never_shows_a_locked_note_s_title_tags_or_word_count() {
    let source = include_str!("../src/ui.rs");
    let start = source
        .find("fn present_note_info_dialog")
        .expect("present_note_info_dialog should exist");
    let end = source[start..]
        .find("\nfn ")
        .map_or(source.len(), |offset| start + offset);
    let body = &source[start..end];
    // The locked branch must only ever build its content from the summary
    // (relative_path/id), never from a decrypted `Note` - there is no path
    // in this function that can reach one while `active_snapshot` is `None`.
    let none_arm_start = body
        .find("None =>")
        .expect("a None (locked) match arm must exist");
    let none_arm = &body[none_arm_start..];
    // The tuple-destructuring `Some((` (not a bare `Some(`, which also
    // matches the unrelated `gtk::Label::new(Some("Encrypted · locked"))`
    // call inside the locked branch itself) marks the start of the unlocked
    // match arm.
    let some_arm_start = none_arm
        .find("Some((")
        .expect("a Some (unlocked) match arm must exist");
    let locked_branch = &none_arm[..some_arm_start];
    for leak in ["note.metadata.title", "note.metadata.tags", "note.body"] {
        assert!(
            !locked_branch.contains(leak),
            "the locked-note branch of the info dialog must never reference {leak}"
        );
    }
    assert!(locked_branch.contains("Encrypted"));
}

#[test]
fn next_and_previous_note_accelerators_do_not_collide_with_gtksourceview_defaults() {
    let source = include_str!("../src/ui.rs");
    // GtkSourceView binds Alt+Up/Down to move-lines and Alt+Left/Right to
    // move-words by default; a global accelerator on the same combination
    // would only ever fire when focus happens to be outside the editor.
    for claimed in ["<Alt>Up", "<Alt>Down", "<Alt>Left", "<Alt>Right"] {
        assert!(
            !source.contains(&format!("&[\"{claimed}\"]")),
            "{claimed} is already a GtkSourceView default keybinding and must not be reused as \
             a global accelerator"
        );
    }
}

#[test]
fn note_information_is_reachable_by_shortcut_header_button_and_context_menu() {
    let source = include_str!("../src/ui.rs");
    assert!(source.contains("\"app.note-info\""));
    assert!(source.contains("\"app.context-note-info\""));
    assert!(source.contains("note_info_button.set_action_name(Some(\"app.note-info\"))"));
    assert!(source.contains("Note Information"));
}
