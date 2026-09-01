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

/// Extracts the body of the named function (from its `fn` line to the next
/// top-level `fn`), for the Editor V2 "rendering tags are purely
/// presentational" checks below.
fn function_body<'a>(source: &'a str, name: &str) -> &'a str {
    let start = source
        .find(&format!("fn {name}("))
        .unwrap_or_else(|| panic!("{name} should exist"));
    let end = source[start..]
        .find("\nfn ")
        .map_or(source.len(), |offset| start + offset);
    &source[start..end]
}

#[test]
fn editor_v2_never_uses_gtktexttag_invisible() {
    // The Stage C.0 prototype found a real, fatal GTK4 crash
    // (gtktextbtree.c) when move-cursor is emitted on a buffer containing
    // an invisible-tagged span. Editor V2 must never reach for it again -
    // marker punctuation is dimmed (a foreground colour/alpha tag), never
    // hidden.
    let source = include_str!("../src/ui.rs");
    assert!(!source.contains("set_invisible"));
    assert!(!source.contains(".invisible(true)"));
    assert!(!source.contains("\"invisible\""));
}

#[test]
fn markdown_style_recompute_only_ever_applies_or_removes_tags() {
    let source = include_str!("../src/ui.rs");
    let recompute = function_body(source, "recompute_markdown_styles");
    // Only tag operations - never a text mutation, which is the only way
    // this pass could possibly touch buffer.text(), the modified bit,
    // undo history, or (by having no path to any save function at all)
    // autosave/updated_at.
    for forbidden in [
        ".insert(",
        ".delete(",
        ".set_text(",
        "persist_active",
        "save_note",
        "save_encrypted_note",
    ] {
        assert!(
            !recompute.contains(forbidden),
            "recompute_markdown_styles must never contain {forbidden} - it is presentation only"
        );
    }
    assert!(recompute.contains("apply_tag_by_name") || recompute.contains("apply_tag_range"));
    assert!(recompute.contains("remove_tag_by_name"));

    let apply_range = function_body(source, "apply_tag_range");
    for forbidden in [".insert(", ".delete(", ".set_text("] {
        assert!(!apply_range.contains(forbidden));
    }
}

#[test]
fn markdown_style_recompute_is_debounced_separately_from_autosave() {
    let source = include_str!("../src/ui.rs");
    let schedule = function_body(source, "schedule_style_recompute");
    // A distinct SourceId slot and timer from schedule_body_save's, so
    // restyling can never coalesce with, delay, or be mistaken for a save.
    assert!(schedule.contains("style_recompute_source"));
    assert!(!schedule.contains("pending.borrow"));
}

#[test]
fn gtksourceview_builtin_syntax_highlighting_stays_disabled() {
    // Editor V2's own markdown_spans-driven tags are the single source of
    // Markdown visual styling. A real-machine acceptance pass found Ctrl+B
    // visually producing bold+italic instead of bold-only, most plausibly
    // from GtkSourceView's own independent syntax highlighting stacking on
    // top; a dedicated pipeline test (tests/formatting_pipeline.rs) proves
    // the formatting/rendering logic itself produces bold-only for that
    // input, so re-enabling this must not happen without re-auditing that
    // interaction.
    let source = include_str!("../src/ui.rs");
    assert!(source.contains("buffer.set_highlight_syntax(false)"));
}

#[test]
fn toolbar_active_state_updates_are_deferred_off_the_signal_stack() {
    // `cursor-position` is a GObject property notify, which fires
    // synchronously wherever the cursor moves - including nested inside
    // GtkTextBuffer::delete/insert while a formatting action is still on
    // the stack. Mutating the toolbar buttons' CSS classes directly from
    // that handler risks a GTK layout/allocation race (a real-machine
    // acceptance pass hit "Trying to snapshot GtkGizmo ... without a
    // current allocation" while spam-triggering formatting controls); the
    // handler must only ever schedule the update, never perform it inline.
    let source = include_str!("../src/ui.rs");
    assert!(
        source.contains("connect_cursor_position_notify(move |_| schedule_format_toolbar_update")
    );
    let schedule = function_body(source, "schedule_format_toolbar_update");
    assert!(schedule.contains("idle_add_local_once"));
}

#[test]
fn changing_sort_order_persists_the_choice_before_refreshing_the_visible_list() {
    let source = include_str!("../src/ui.rs");
    let start = source
        .find("let set_sort_order = gio::SimpleAction::new_stateful")
        .expect("set_sort_order action should exist");
    let end = source[start..]
        .find("\n    application.add_action(&set_sort_order);")
        .map_or(source.len(), |offset| start + offset);
    let body = &source[start..end];
    let config_set_at = body
        .find("state.config.sort_order = Some(order)")
        .expect("activating set-sort-order must update state.config.sort_order");
    let render_at = body
        .find("render_note_list(&state, &widgets)")
        .expect("activating set-sort-order must refresh the visible list");
    assert!(
        config_set_at < render_at,
        "state.config.sort_order must be updated before render_note_list reads it, so the \
         list is never rebuilt against the stale sort order"
    );
    assert!(
        body.contains("state.config.save()"),
        "an explicit sort choice must persist across restarts"
    );
}

#[test]
fn filtered_notes_reads_the_persisted_sort_order() {
    let source = include_str!("../src/ui.rs");
    let filtered = function_body(source, "filtered_notes");
    assert!(
        filtered.contains("state.config.sort_order"),
        "filtered_notes must read the user's persisted sort choice, not a hardcoded order"
    );
    assert!(filtered.contains("sort_notes(&mut notes"));
}

#[test]
fn render_note_list_reselects_the_previously_selected_note_by_uuid_after_resorting() {
    let source = include_str!("../src/ui.rs");
    let render = function_body(source, "render_note_list");
    // The pre-render selected note id is captured, then looked back up by
    // UUID in the freshly (re)sorted row order - never by a stale numeric
    // index, which a sort-order change would invalidate.
    assert!(render.contains("selected_note"));
    assert!(render.contains("widgets.selection.index_of(RowTarget::Note(id))"));
}

#[test]
fn update_active_summary_syncs_the_real_title_and_preview_while_unlocked() {
    let source = include_str!("../src/ui.rs");
    let body = function_body(source, "update_active_summary");
    // This function only ever runs against `state.active`, which for an
    // encrypted note only ever holds it while unlocked - so once a note is
    // open, its sidebar row must reflect the real, current title/preview,
    // not the locked placeholder. No `if !encrypted` (or `if encrypted`)
    // gate may skip the summary/row-widget sync below.
    assert!(
        !body.contains("if !encrypted"),
        "update_active_summary must not skip syncing the sidebar row for an \
         open, unlocked encrypted note"
    );
    assert!(
        !body.contains("if encrypted"),
        "update_active_summary must not special-case the preview text for \
         an open, unlocked encrypted note"
    );
    assert!(body.contains("summary.title = title.clone()"));
    assert!(body.contains("summary.preview = preview.clone()"));
    assert!(body.contains("row_widgets.title.set_label(&title)"));
    assert!(
        body.contains("summary.locked = false"),
        "an open, unlocked encrypted note's summary must transition out of \
         the locked state, the reverse of what lock_all_encrypted does"
    );
}

#[test]
fn locked_note_rows_get_their_uuid_derived_label_not_a_bare_locked_note_string() {
    let source = include_str!("../src/ui.rs");
    // `lock_all_encrypted` must reuse the exact label `NoteSummary::locked`
    // computed (its anonymous, UUID-derived suffix) for the row it directly
    // relabels, never a bare "Locked Note" literal that would make every
    // locked note indistinguishable again.
    let lock_all = function_body(source, "lock_all_encrypted");
    assert!(!lock_all.contains("row_widgets.title.set_label(\"Locked Note\")"));
    assert!(lock_all.contains("locked_titles"));
}

#[test]
fn switching_to_a_view_never_prompts_for_a_password_on_its_own() {
    let source = include_str!("../src/ui.rs");
    // Automatic fallback selection (landing on whatever note sorts first
    // when a view is opened, or on an adjacent note after one is removed)
    // must never call the password-prompting entry point directly - only
    // `select_note_without_prompting_if_locked`, which shows a locked note
    // as selected without launching its unlock dialog. A real-machine
    // acceptance pass found that switching to Inbox could pop a password
    // prompt for whichever encrypted note happened to sort first; this
    // pins the fix so it cannot silently regress.
    let select_first_row = function_body(source, "select_first_row");
    assert!(
        select_first_row.contains("select_note_without_prompting_if_locked(id, state, widgets)")
    );
    assert!(!select_first_row.contains("=> load_note_by_id(id, state, widgets)"));

    let select_adjacent_after_removal = function_body(source, "select_adjacent_after_removal");
    assert!(
        select_adjacent_after_removal
            .contains("select_note_without_prompting_if_locked(id, state, widgets)")
    );
    assert!(!select_adjacent_after_removal.contains("=> load_note_by_id(id, state, widgets)"));

    // The non-prompting path must still show the locked placeholder (so the
    // note reads as locked, not silently blank), it just must not launch
    // `present_password_dialog` unless the caller asked for that.
    let open_note_by_id = function_body(source, "open_note_by_id");
    assert!(open_note_by_id.contains("show_locked_placeholder(widgets)"));
    assert!(open_note_by_id.contains("if !prompt_if_locked"));
}

#[test]
fn inbox_notebook_directory_name_is_unchanged_only_its_display_label_moved_to_unfiled() {
    // Issue #7: the storage directory must stay named "Inbox" for backward
    // compatibility with existing vaults - only the UI-facing label changes
    // to "Unfiled". `Vault::DEFAULT_NOTEBOOK`/`is_reserved_notebook` must
    // never be touched by this cosmetic change.
    let vault_source = include_str!("../src/vault.rs");
    assert!(vault_source.contains(r#"const DEFAULT_NOTEBOOK: &str = "Inbox";"#));

    let ui_source = include_str!("../src/ui.rs");
    assert!(
        !ui_source.contains(r#"sidebar_button("Inbox""#),
        "the Inbox sidebar row must display \"Unfiled\", not the raw notebook name"
    );
    assert!(ui_source.contains(r#"sidebar_button("Unfiled""#));
    assert!(ui_source.contains(r#"switch_view(ViewMode::Notebook(PathBuf::from("Inbox"))"#));
}
