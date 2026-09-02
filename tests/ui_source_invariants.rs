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
    assert!(body.contains("ViewMode::Favourites") && body.contains("ViewMode::RecentlyOpened"));
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

/// `open_vault` plus `proceed_with_opened_vault`, as a single slice. A Stage D
/// refactor moved the advisory-lock decision out of `open_vault` into the
/// shared `proceed_with_opened_vault` helper (also reached by the encrypted-
/// vault creator); the Stage B/C lock invariants inspect the flow as a whole.
/// `open_vault`'s body still comes first, so relative-order assertions hold.
fn open_vault_flow(source: &str) -> String {
    format!(
        "{}\n{}",
        function_body(source, "open_vault"),
        function_body(source, "proceed_with_opened_vault"),
    )
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

// ---------------------------------------------------------------------------
// Stage B: multi-vault switch lifecycle and stale-callback inertness
// ---------------------------------------------------------------------------

#[test]
fn vault_open_validates_and_locks_before_disturbing_the_current_session() {
    // `open_vault` validates the target and decides the lock; it must not clear
    // the current session, cancel timers, or bump the generation until
    // `commit_vault_switch`, and it must handle a validation `Err` and a lock
    // `Err` first.
    let source = include_str!("../src/ui.rs");
    let flow = open_vault_flow(source);

    let validate_at = flow
        .find("Vault::create(path)")
        .expect("open_vault must validate the target vault");
    let lock_at = flow
        .find("VaultLock::acquire(&vault)")
        .expect("the open flow must decide the lock before committing");
    assert!(validate_at < lock_at, "validate before locking");
    // The commit machinery is *not* inline in the open flow.
    for not_here in [
        "clear_sensitive_documents(state)",
        "cancel_all_timers(pending)",
        "widgets.sessions.bump()",
        "state.vault = Some(vault)",
        "prepare_to_leave_active(state, widgets, pending)",
    ] {
        assert!(
            !flow.contains(not_here),
            "the open flow must delegate `{not_here}` to commit_vault_switch"
        );
    }
    // A lock error is reported and returns without touching the session.
    assert!(flow.contains("Could not lock the vault"));

    // commit_vault_switch: validate/lock already done; flush the outgoing vault,
    // and a failed flush aborts (dropping the just-acquired lock).
    let commit = function_body(source, "commit_vault_switch");
    let flush_at = commit
        .find("prepare_to_leave_active(state, widgets, pending)")
        .expect("commit_vault_switch must flush the outgoing vault");
    assert!(
        commit[flush_at..commit.find("state.vault = Some(vault)").unwrap()].contains("drop(lock)"),
        "a failed flush must release the newly acquired lock"
    );
    assert!(
        commit[flush_at..].contains("staying on this vault"),
        "a failed flush must abort and keep the current vault"
    );
    assert!(
        flush_at < commit.find("state.vault = Some(vault)").unwrap(),
        "flush before the vault is swapped"
    );
}

#[test]
fn the_old_writable_lock_is_released_only_after_the_outgoing_vault_is_flushed() {
    let source = include_str!("../src/ui.rs");
    let commit = function_body(source, "commit_vault_switch");
    let flush_at = commit
        .find("persist_vault_session_state(state, widgets)")
        .unwrap();
    let release_at = commit
        .find("state.borrow_mut().vault_lock.take()")
        .expect("commit_vault_switch must release the old lock");
    let swap_at = commit.find("state.vault_lock = Some(lock)").unwrap();
    assert!(
        flush_at < release_at && release_at < swap_at,
        "old lock released after flush + session-state save, before the new lock is stored"
    );
    // A read-only session holds a non-owning lock and never owns the file.
    assert!(commit.contains("!lock.is_owner()"));
}

#[test]
fn takeover_is_offered_only_for_a_proven_dead_lock() {
    let source = include_str!("../src/ui.rs");
    let open = open_vault_flow(source);
    assert!(open.contains("present_lock_contention_dialog"));

    let dialog = function_body(source, "present_lock_contention_dialog");

    // Only ProvenDead carries a "Take Over" button + a TakeOver action.
    let proven_dead_at = dialog
        .find("LockStatus::ProvenDead")
        .expect("the dialog must branch on ProvenDead");
    let live_at = dialog
        .find("reason: BlockedReason::Live")
        .expect("the dialog must special-case a live blocked lock");
    let other_blocked_at = dialog
        .find("LockStatus::Blocked { owner, reason } =>")
        .expect("the dialog must have a catch-all Blocked arm");

    // "Take Over" appears exactly once - in the ProvenDead arm.
    assert_eq!(dialog.matches("\"Take Over\"").count(), 1);
    let take_over_at = dialog.find("\"Take Over\"").unwrap();
    assert!(
        proven_dead_at < take_over_at && take_over_at < live_at,
        "the Take Over button belongs to the ProvenDead arm only"
    );

    // The Live arm offers Show Existing Window; the catch-all Blocked arm
    // offers ONLY Cancel + Open Read-Only (no third button).
    let live_arm = &dialog[live_at..other_blocked_at];
    assert!(live_arm.contains("\"Show Existing Window\""));
    let other_arm = &dialog[other_blocked_at..dialog.find("LockStatus::Free |").unwrap()];
    assert!(other_arm.contains("vec![\"Cancel\", \"Open Read-Only\"]"));
    assert!(
        !other_arm.contains("Take Over") && !other_arm.contains("Show Existing Window"),
        "a non-live blocked lock offers no third button"
    );

    // Takeover is reviewed (a specific DeadReason) and a stale/live result is
    // surfaced, never forced.
    assert!(dialog.contains("VaultLock::take_over(&vault, *reason)"));
    assert!(dialog.contains("ContentionAction::TakeOver(*reason)"));
    assert!(dialog.contains("Ok(LockAcquisition::Contended(_)) => report_vault_open_error"));
    // Read-only fallback: a non-owning handle; the blocked owner's file stays put.
    assert!(dialog.contains("VaultLock::read_only()"));
}

#[test]
fn a_blocked_lock_never_becomes_writable_through_a_ui_fallback() {
    let source = include_str!("../src/ui.rs");
    // The only writable path out of a contended lock is take_over on a proven-
    // dead lock. The read-only fallback and the "read-only anyway" shortcut both
    // use VaultLock::read_only() (non-owning); acquire only ever hands out a
    // writable lock for Free / HeldByThisProcess.
    let flow = open_vault_flow(source);
    assert!(flow.contains("Ok(LockAcquisition::Contended(_)) if vault.is_read_only()"));
    assert!(flow.contains("VaultLock::read_only()"));

    // The lock module: acquire returns Acquired only for Free / HeldByThisProcess.
    let lock_source = include_str!("../src/vault_lock.rs");
    let acquire = function_body(lock_source, "acquire");
    let acquired_at = acquire.find("LockAcquisition::Acquired").unwrap();
    let contended_at = acquire.find("LockAcquisition::Contended").unwrap();
    assert!(acquire[..contended_at].contains("LockStatus::HeldByThisProcess"));
    assert!(acquire[..contended_at].contains("LockStatus::Free"));
    assert!(
        acquire[acquired_at..contended_at]
            .contains("Blocked { .. } | LockStatus::ProvenDead { .. }")
            || acquire.contains("Blocked { .. } | LockStatus::ProvenDead { .. }"),
        "acquire must hand Blocked and ProvenDead back as Contended, never Acquired"
    );
}

#[test]
fn different_host_is_never_proven_dead() {
    let lock_source = include_str!("../src/vault_lock.rs");
    let verdict = function_body(lock_source, "liveness_verdict");
    // The DifferentHost branch yields Blocked, and it is checked before any
    // Dead verdict.
    let host_at = verdict
        .find("BlockedReason::DifferentHost")
        .expect("liveness_verdict must handle a foreign hostname");
    let first_dead_at = verdict
        .find("Verdict::Dead(")
        .expect("liveness_verdict must be able to prove death");
    assert!(
        host_at < first_dead_at,
        "a different hostname must be evaluated (as Blocked) before any proof of death"
    );
    // DeadReason has exactly the three proven cases.
    assert!(lock_source.contains("pub enum DeadReason"));
    for dead in ["DifferentBoot", "ProcessGone", "PidReused"] {
        assert!(lock_source.contains(dead));
    }
    // EPERM maps to CannotVerify -> Blocked, never Dead.
    assert!(lock_source.contains("libc::ESRCH => ProbeResult::NoSuchProcess"));
    assert!(lock_source.contains("_ => ProbeResult::CannotVerify"));
}

#[test]
fn every_deferred_mutation_carries_a_session_generation_check() {
    // An autosave timer, the title-commit timer, and the coalesced selection
    // dispatch are all armed under one vault and may fire after a switch. Each
    // must record `widgets.sessions.current()` when armed and bail on
    // `!widgets.sessions.is_current(session)` when it runs.
    let source = include_str!("../src/ui.rs");
    for func in [
        "schedule_body_save",
        "schedule_title_commit",
        "request_selection",
    ] {
        let body = function_body(source, func);
        assert!(
            body.contains("widgets.sessions.current()")
                || body.contains("widgets_for_save.sessions.current()"),
            "{func} must capture the session generation when arming its callback"
        );
        assert!(
            body.contains("sessions.is_current(session)"),
            "{func}'s deferred callback must bail when the session generation moved on"
        );
    }
}

#[test]
fn a_vault_switch_tears_down_the_previous_session_before_committing() {
    let source = include_str!("../src/ui.rs");
    let body = function_body(source, "commit_vault_switch");
    for teardown in [
        "clear_sensitive_documents(state)",
        "cancel_all_timers(pending)",
        "cancel_pending_selection(widgets)",
        "cancel_editor_deferrals(widgets)",
        "state.borrow_mut().vault_lock.take()",
    ] {
        assert!(
            body.contains(teardown),
            "commit_vault_switch must call `{teardown}`"
        );
    }
    // The old vault's `VaultWatcher` is dropped by the reassignment, which stops
    // its backend thread - no stale filesystem events survive the switch.
    assert!(body.contains("state.watcher = watcher"));
}

#[test]
fn read_only_ui_disables_mutation_controls_but_not_browsing() {
    let source = include_str!("../src/ui.rs");
    let body = function_body(source, "apply_read_only_ui");
    for disabled in [
        "widgets.new_note",
        "widgets.new_notebook",
        "widgets.delete_button",
        "widgets.formatting_bar.set_sensitive(writable)",
        "widgets.title.set_editable(writable)",
        "widgets.editor.set_editable(writable)",
        "vault_readonly_icon.set_visible(read_only)",
    ] {
        assert!(
            body.contains(disabled),
            "apply_read_only_ui must gate `{disabled}`"
        );
    }
    // Browsing widgets must never be touched here.
    for browsing in ["note_list", "search", "selection", "all_notes_button"] {
        assert!(
            !body.contains(browsing),
            "apply_read_only_ui must not disable browsing control `{browsing}`"
        );
    }

    // The vault layer is the real safety net: a read-only vault rejects every
    // mutation regardless of the UI.
    assert!(source.contains("state.read_only"));
    let body_save = function_body(source, "schedule_body_save");
    assert!(
        body_save.contains("state.read_only"),
        "autosave must not even be scheduled for a read-only vault"
    );
}

#[test]
fn read_only_state_never_creates_a_note_on_open() {
    let source = include_str!("../src/ui.rs");
    // The workspace build (shared by a plain switch and an encrypted unlock)
    // only auto-creates a first note when the vault is writable.
    let body = function_body(source, "enter_vault_workspace");
    assert!(
        body.contains("is_empty && !read_only"),
        "enter_vault_workspace must not auto-create a note in a read-only vault"
    );
}

#[test]
fn startup_restores_the_previous_vault_or_shows_the_chooser_without_creating_one() {
    let source = include_str!("../src/ui.rs");
    let build = function_body(source, "build_application");
    // Restore only when the folder still exists...
    assert!(build.contains("Some(path) if path.is_dir() => open_vault"));
    // ...otherwise a message, never a silent `Vault::create`.
    assert!(build.contains("no longer there"));
    assert!(
        !build.contains("Vault::create"),
        "startup must never create a replacement vault"
    );
    // Recent vaults are reachable from the welcome state.
    assert!(build.contains("render_vault_switcher(&state, &widgets, &pending)"));
}

#[test]
fn the_refcell_gtk_stabilization_machinery_is_unchanged_by_stage_b() {
    // Frozen: the selection coordinator, the signal gates, and the coalesced
    // dispatch design. Stage B adds a *separate* SessionRegistry and never
    // touches these.
    let source = include_str!("../src/ui.rs");
    assert!(source.contains("editor_events: Rc::new(SignalGate::default())"));
    assert!(source.contains("notebook_events: Rc::new(SignalGate::default())"));
    assert!(source.contains("tags_events: Rc::new(SignalGate::default())"));
    assert!(source.contains("selection: Rc::new(SelectionCoordinator::default())"));
    // The session generation lives on Widgets alongside the other
    // reentrancy-independent coordinators, never inside RefCell<AppState>.
    assert!(source.contains("sessions: Rc<SessionRegistry>"));
    assert!(source.contains("sessions: Rc::new(SessionRegistry::default())"));
    let state_struct = &source[source.find("struct AppState {").unwrap()
        ..source.find("struct AppState {").unwrap() + 900];
    assert!(
        !state_struct.contains("SessionRegistry") && !state_struct.contains("session_generation"),
        "the session generation must not be a field of AppState"
    );
}

// ---------------------------------------------------------------------------
// Stage C: advisory vault lock
// ---------------------------------------------------------------------------

#[test]
fn a_failed_lock_acquisition_never_touches_the_current_session() {
    let source = include_str!("../src/ui.rs");
    let open = open_vault_flow(source);
    // On a lock error, the open flow reports and returns - no
    // commit_vault_switch, no teardown.
    let err_at = open
        .find("Err(error) => {")
        .expect("the open flow must handle a lock Err");
    let tail = &open[err_at..];
    assert!(tail.contains("Could not lock the vault"));
    assert!(
        !tail[..tail.find('}').unwrap_or(tail.len())].contains("commit_vault_switch"),
        "a lock error must not commit a switch"
    );
}

#[test]
fn the_read_only_session_holds_a_non_owning_lock() {
    let source = include_str!("../src/ui.rs");
    let open = open_vault_flow(source);
    // A read-only vault opened over a contended lock uses read_only(), not a
    // takeover.
    assert!(open.contains("vault.is_read_only()"));
    assert!(open.contains("VaultLock::read_only()"));
    // AppState carries the lock so its Drop releases it; read-only = non-owning.
    assert!(source.contains("vault_lock: Option<VaultLock>"));
    let commit = function_body(source, "commit_vault_switch");
    assert!(commit.contains("vault.is_read_only() || !lock.is_owner()"));
}

#[test]
fn a_normal_exit_releases_the_vault_lock() {
    let source = include_str!("../src/ui.rs");
    let build = function_body(source, "build_application");
    let close_at = build
        .find("connect_close_request")
        .expect("the window has a close handler");
    let close = &build[close_at..];
    let persist_at = close
        .find("persist_active(&state, &widgets, true)")
        .unwrap();
    let release_at = close
        .find("state.borrow_mut().vault_lock.take()")
        .expect("the close handler must release the vault lock");
    assert!(
        persist_at < release_at,
        "the lock is released only after the final save succeeds"
    );
}

#[test]
fn lock_handling_adds_no_new_reentrancy_surface() {
    // The lock module is pure filesystem I/O: it never touches GTK and never
    // borrows AppState. The UI calls it only outside a `state` borrow.
    let lock_source = include_str!("../src/vault_lock.rs");
    for forbidden in ["gtk::", "glib::", "AppState", "RefCell", "widgets"] {
        assert!(
            !lock_source.contains(forbidden),
            "vault_lock.rs must not reference `{forbidden}`"
        );
    }
    // The contention dialog follows the existing modal-AlertDialog pattern.
    let source = include_str!("../src/ui.rs");
    let dialog = function_body(source, "present_lock_contention_dialog");
    assert!(dialog.contains("gtk::AlertDialog::builder()"));
    assert!(dialog.contains(".modal(true)"));
}

// ---------------------------------------------------------------------------
// Stage D: whole-vault encryption UI
// ---------------------------------------------------------------------------

#[test]
fn vault_unlock_derives_the_key_off_the_main_thread() {
    // Argon2id for a whole-vault unlock must never run on the GTK main loop.
    let source = include_str!("../src/ui.rs");
    let unlock = function_body(source, "begin_vault_unlock");
    let spawn_at = unlock
        .find("gio::spawn_blocking(")
        .expect("the KEK derivation must run on a worker thread");
    let derive_at = unlock
        .find("vault_crypto::open_keyfile(")
        .expect("begin_vault_unlock must call open_keyfile");
    assert!(
        spawn_at < derive_at,
        "open_keyfile must be *inside* the spawn_blocking closure"
    );
    // The main-thread continuation re-checks the session generation before it
    // touches any widget (a vault switch during derivation makes it inert).
    assert!(unlock.contains("widgets.sessions.is_current(session)"));
    assert!(unlock.contains("vault.finish_unlock(keys)"));
    // A failed unlock only updates the lock card's message - it never builds a
    // workspace and never re-primes (leaving the shell the user already sees).
    let fail_region = &unlock[unlock.find("Ok(Err(_))").expect("a wrong-password arm")..];
    assert!(fail_region.contains("set_vault_locked_message"));

    // The encrypted-vault creator derives its keyfile off-thread too.
    let create = function_body(source, "present_encrypted_vault_creator");
    let create_spawn = create
        .find("gio::spawn_blocking(")
        .expect("create_keyfile must run on a worker thread");
    let create_derive = create
        .find("vault_crypto::create_keyfile(")
        .expect("the creator must call create_keyfile");
    assert!(create_spawn < create_derive);

    // The target folder is validated (not already a vault / no plaintext notes)
    // BEFORE the password prompt and the key-derivation worker - never
    // encrypting on top of an existing ordinary vault.
    let check_at = create
        .find("Vault::check_encrypted_target(&path)")
        .expect("the creator must validate the chosen folder");
    let prompt_at = create
        .find("present_password_dialog(")
        .expect("the creator prompts for a password");
    assert!(
        check_at < prompt_at && check_at < create_spawn,
        "the folder check must run before the password prompt and the KDF worker"
    );
}

#[test]
fn creating_an_encrypted_vault_is_reachable_without_the_welcome_screen() {
    // The welcome screen is only shown at startup when there is no openable
    // last vault. Vault creation (ordinary and encrypted) must also be reachable
    // once a workspace is open - via the header vault-switcher popover and the
    // primary menu, exactly like "Open Vault…".
    let source = include_str!("../src/ui.rs");

    let actions = function_body(source, "install_actions");
    assert!(
        actions.contains(r#"gio::SimpleAction::new("create-encrypted-vault", None)"#),
        "an app.create-encrypted-vault action must exist"
    );
    assert!(
        actions.contains("present_encrypted_vault_creator(&state, &widgets, &pending)"),
        "the action must invoke the encrypted-vault creator"
    );

    let popover = function_body(source, "build_vault_popover");
    for action in [
        "app.open-vault",
        "app.create-vault",
        "app.create-encrypted-vault",
    ] {
        assert!(
            popover.contains(&format!("set_action_name(Some(\"{action}\"))")),
            "the vault switcher popover must offer {action}"
        );
    }

    let menu = function_body(source, "application_menu");
    for action in [
        "app.open-vault",
        "app.create-vault",
        "app.create-encrypted-vault",
    ] {
        assert!(
            menu.contains(&format!("Some(\"{action}\")")),
            "the primary menu must offer {action}"
        );
    }
}

#[test]
fn user_facing_vault_terminology_is_product_language_not_the_technical_phrase() {
    // "whole-vault encryption" is fine in comments / docs but must not reach
    // any user-visible string. The two vault kinds are "Standard Vault" /
    // "Secure Vault" in the UI.
    let source = include_str!("../src/ui.rs");
    // Strip line comments before scanning for user-facing text.
    let visible: String = source
        .lines()
        .map(|line| line.split("//").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n");
    for banned in [
        "whole-vault",
        "Whole-Vault",
        "Whole Vault",
        "Create Encrypted Vault",
        "Encrypted Vault Password",
    ] {
        assert!(
            !visible.contains(banned),
            "user-facing string still uses the technical phrase {banned:?}"
        );
    }
    assert!(source.contains("Create Secure Vault…"));
    assert!(
        source.contains(r#""SECURED VAULTS""#),
        "sidebar section heading lists actual Secure Vaults"
    );
    assert!(
        !source.contains(r#""SECURITY""#),
        "the sidebar heading must be SECURED VAULTS, not SECURITY"
    );

    // "Lock Vault" (vault key) and "Lock Note" (per-note) are separate actions.
    let actions = function_body(source, "install_actions");
    assert!(actions.contains(r#"gio::SimpleAction::new("lock-vault", None)"#));
    assert!(actions.contains(r#"gio::SimpleAction::new("lock-note", None)"#));
    assert!(
        !actions.contains(r#""lock-now""#),
        "the ambiguous lock-now action must be split into lock-vault / lock-note"
    );
    // "Lock Note" must never be wired to the whole-vault lock.
    let lock_note_at = actions
        .find(r#"SimpleAction::new("lock-note", None)"#)
        .unwrap();
    let lock_note_end = (lock_note_at + 400).min(actions.len());
    assert!(
        !actions[lock_note_at..lock_note_end].contains("lock_vault("),
        "lock-note must call the per-note lock, never lock_vault()"
    );

    // The whole-vault lock action calls lock_vault; the vault-password change
    // derives its key off the main thread.
    let lock_vault_at = actions
        .find(r#"SimpleAction::new("lock-vault", None)"#)
        .unwrap();
    let lock_vault_end = (lock_vault_at + 400).min(actions.len());
    assert!(
        actions[lock_vault_at..lock_vault_end].contains("lock_vault(&state, &widgets, &pending)")
    );
    assert!(actions.contains(r#"gio::SimpleAction::new("change-vault-password", None)"#));
    let change = function_body(source, "change_vault_password_flow");
    let spawn_at = change
        .find("gio::spawn_blocking(")
        .expect("the vault-password re-wrap must run on a worker thread");
    let derive_at = change
        .find("Vault::rewrap_encrypted_keyfile(")
        .expect("it calls rewrap_encrypted_keyfile");
    assert!(spawn_at < derive_at);
}

#[test]
fn the_secure_vault_popover_actions_are_hidden_for_a_standard_vault() {
    let source = include_str!("../src/ui.rs");
    let render = function_body(source, "render_vault_switcher");
    assert!(
        render.contains("vault.is_encrypted() && !vault.is_locked()")
            && render.contains("vault_popover_secure_actions")
            && render.contains(".set_visible("),
        "render_vault_switcher must show the Lock Vault / Change Vault Password group \
         only for an unlocked Secure Vault"
    );
}

#[test]
fn the_encrypted_notes_view_is_per_note_encryption_not_every_secure_vault_note() {
    let source = include_str!("../src/ui.rs");
    let includes = function_body(source, "view_includes");
    let arm_at = includes
        .find("ViewMode::EncryptedNotes =>")
        .expect("view_includes must handle the Encrypted Notes view");
    let arm_end = (arm_at + 120).min(includes.len());
    assert!(
        includes[arm_at..arm_end].contains("summary.encrypted"),
        "the Encrypted Notes view filters on per-note encryption (summary.encrypted)"
    );
}

// ---------------------------------------------------------------------------
// UX Stage 1: managed workspace, first-run, Secured Vaults sidebar, rename
// ---------------------------------------------------------------------------

#[test]
fn a_fresh_install_gets_a_managed_workspace_with_no_folder_picker() {
    let source = include_str!("../src/ui.rs");
    let build = function_body(source, "build_application");
    // Startup branches on first-run before falling through to "restore last vault".
    let first_at = build
        .find("run_first_run_setup(&state, &widgets, &pending)")
        .expect("build_application must run first-run setup on a fresh install");
    let restore_at = build
        .find("Some(path) if path.is_dir() => open_vault(&path, false")
        .expect("otherwise it restores the last vault");
    assert!(first_at < restore_at);
    assert!(
        build.contains("!config.first_run_done")
            && build.contains("config.recent_vaults.is_empty()"),
        "first-run fires only with no prior config / vaults"
    );

    let setup = function_body(source, "run_first_run_setup");
    assert!(setup.contains("paths::default_workspace_root()"));
    assert!(
        setup.contains(r#"Vault::create(&main_path)"#) && setup.contains(r#"root.join("Main")"#),
        "first run creates a Standard \"Main\" vault under the managed root"
    );
    assert!(setup.contains("first_run_done = true"));
    assert!(
        !setup.contains("FileDialog"),
        "first-run setup must not require a folder picker"
    );

    // The managed Secure Vault flow also has no folder picker and derives the
    // key off the main thread.
    let secure = function_body(source, "present_managed_secure_vault_setup");
    assert!(
        !secure.contains("FileDialog"),
        "the managed Secure Vault setup must not use a folder picker"
    );
    let spawn_at = secure
        .find("gio::spawn_blocking(")
        .expect("Argon2id runs on a worker thread");
    let derive_at = secure
        .find("vault_crypto::create_keyfile(")
        .expect("it calls create_keyfile");
    assert!(spawn_at < derive_at);
    assert!(
        secure.contains(r#"set_vault_display_name(vault.root(), &display)"#),
        "the new Secure Vault gets a display name (\"Secure\")"
    );
}

#[test]
fn the_secured_vaults_sidebar_lists_bounded_secure_vaults_not_encrypted_notes() {
    let source = include_str!("../src/ui.rs");
    // No "Encrypted Notes" smart-view button in the sidebar anymore.
    assert!(
        !source.contains(r#"sidebar_button("Encrypted Notes""#),
        "the SECURED section must list vaults, not an encrypted-notes smart view"
    );
    let render = function_body(source, "render_secure_vaults_sidebar");
    assert!(
        render.contains("state.config.secure_vaults_mru()"),
        "the list comes from the Secure-Vault index"
    );
    assert!(
        render.contains("take(SIDEBAR_SECURE_VAULTS)"),
        "the list is bounded"
    );
    assert!(
        render.contains(r#"open_vault(&path, false"#),
        "clicking a row switches to that vault"
    );
    assert!(
        render.contains(r#""More…""#),
        "overflow uses a More… affordance"
    );
    // The bound is small.
    let bound = source
        .split("const SIDEBAR_SECURE_VAULTS: usize = ")
        .nth(1)
        .and_then(|s| s.split(';').next())
        .and_then(|s| s.trim().parse::<usize>().ok())
        .expect("SIDEBAR_SECURE_VAULTS is a literal usize");
    assert!(
        (2..=6).contains(&bound),
        "the sidebar shows ~3-4 Secure Vaults, not an unlimited list (got {bound})"
    );
}

#[test]
fn the_search_field_lives_in_the_sidebar_and_scopes_to_the_open_vault() {
    let source = include_str!("../src/ui.rs");
    let build = function_body(source, "build_window");
    let search_at = build
        .find("let search = gtk::SearchEntry::builder()")
        .expect("the search entry is built");
    let sidebar_append_at = build
        .find("sidebar.append(&search)")
        .expect("the search entry is appended to the sidebar");
    let notes_box_at = build
        .find("notes_box.append(&notes_header)")
        .expect("marker for the note-list column header");
    assert!(
        search_at < sidebar_append_at && sidebar_append_at < notes_box_at,
        "search is created and placed in the sidebar, above the note-list column"
    );
    assert!(
        !build.contains("notes_box.append(&search)"),
        "the search entry no longer lives in the note-list column"
    );
    // Locking a Secure Vault clears the search box and the in-memory results.
    let lock = function_body(source, "lock_vault");
    assert!(lock.contains(r#"widgets.search.set_text("")"#));
    assert!(lock.contains("state.notes.clear()"));
}

#[test]
fn renaming_a_vault_is_display_name_only() {
    let source = include_str!("../src/ui.rs");
    let rename = function_body(source, "rename_current_vault");
    assert!(rename.contains("config.set_vault_display_name(&root, &name)"));
    for forbidden in [
        "fs::rename",
        "vault.move_note",
        "finish_create_encrypted",
        "rewrap",
        "std::fs::rename",
    ] {
        assert!(
            !rename.contains(forbidden),
            "rename_current_vault must not `{forbidden}` - display name only"
        );
    }
    let actions = function_body(source, "install_actions");
    assert!(actions.contains(r#"gio::SimpleAction::new("rename-vault", None)"#));
}

#[test]
fn an_encrypted_vault_opens_to_the_lock_screen_not_the_workspace() {
    let source = include_str!("../src/ui.rs");
    let commit = function_body(source, "commit_vault_switch");
    let branch_at = commit
        .find("vault.is_encrypted() && vault.is_locked()")
        .expect("commit_vault_switch must detect a locked encrypted vault");
    let show_at = commit
        .find("show_vault_locked_screen(state, widgets, pending, None)")
        .expect("a locked encrypted vault shows the locked-vault workspace");
    let return_at = commit[show_at..]
        .find("return;")
        .map(|offset| show_at + offset)
        .expect("and returns before building the note workspace");
    assert!(branch_at < show_at && show_at < return_at);
    // The unlocked-workspace build (note list, editor, restored selection) is
    // only reached past that early return.
    let workspace_at = commit
        .find("enter_vault_workspace(state, widgets, pending)")
        .expect("commit_vault_switch delegates the workspace build");
    assert!(
        return_at < workspace_at,
        "the unlocked workspace is never built for a still-locked vault"
    );
}

#[test]
fn a_locked_secure_vault_keeps_a_working_shell_and_switcher() {
    // The lock screen must never be a dead-end: the header, vault identity and
    // vault switcher stay live so the user can move to another vault (Main
    // included) without this vault's password.
    let source = include_str!("../src/ui.rs");
    let show = function_body(source, "show_vault_locked_screen");
    // The whole application shell (header + sidebar) stays visible...
    assert!(show.contains(r#"widgets.stack.set_visible_child_name("workspace")"#));
    // ...only the *content* area shows the lock card.
    let msg = function_body(source, "set_vault_locked_message");
    let msg_flat: String = msg.split_whitespace().collect();
    assert!(msg_flat.contains(r#"document_stack.set_visible_child_name("vault-locked")"#));
    // The switcher is rebuilt so it works from the locked state.
    assert!(show.contains("render_vault_switcher(state, widgets, pending)"));
    // No decrypted navigation data may survive into the locked view.
    for cleared in [
        r#"widgets.search.set_text("")"#,
        "widgets.search.set_sensitive(false)",
        "set_quick_actions_visible(widgets, false)",
        "clear_locked_vault_navigation(widgets)",
    ] {
        assert!(
            show.contains(cleared),
            "show_vault_locked_screen must run `{cleared}`"
        );
    }
    // That clear actually empties both the notebook list and the tag chips.
    let clear = function_body(source, "clear_locked_vault_navigation");
    assert!(clear.contains("widgets.notebook_list.remove"));
    assert!(clear.contains("widgets.tags_flow.remove"));
    assert!(clear.contains("widgets.notebook_rows.borrow_mut().clear()"));

    // The lock card itself lives inside the document stack, not as a top-level
    // stack child that would replace the whole shell.
    let build = function_body(source, "build_window");
    assert!(
        build.contains(
            r#"document_stack.add_named(&scroll_center(&vault_locked_page), Some("vault-locked"))"#
        ),
        "the lock card is a document_stack child so the shell stays visible"
    );
}

#[test]
fn locking_a_vault_drops_plaintext_and_invalidates_stale_callbacks() {
    let source = include_str!("../src/ui.rs");
    let lock = function_body(source, "lock_vault");
    // Only ever acts on an unlocked encrypted vault.
    assert!(lock.contains("vault.is_encrypted() && !vault.is_locked()"));
    // Flushes first; a failed save keeps the vault unlocked.
    let persist_at = lock
        .find("persist_active(state, widgets, true)")
        .expect("lock_vault must flush the open note first");
    let drop_key_at = lock
        .find("vault.lock()")
        .expect("lock_vault must drop the key material");
    assert!(persist_at < drop_key_at);
    for cleared in [
        "clear_sensitive_documents(state)",
        "state.notes.clear()",
        "state.trash.clear()",
        "widgets.sessions.bump()",
        "set_buffer_text_silently(&widgets.buffer, \"\")",
        "widgets.search.set_text(\"\")",
        "show_vault_locked_screen(state, widgets, pending, None)",
    ] {
        assert!(lock.contains(cleared), "lock_vault must run `{cleared}`");
    }
    // The session bump (making armed callbacks inert) happens after the key
    // material is gone.
    assert!(
        drop_key_at < lock.find("widgets.sessions.bump()").unwrap(),
        "bump the session generation after locking, not before"
    );

    // Auto-lock is wired into the existing idle + focus-loss machinery.
    let events = function_body(source, "connect_locking_events");
    assert_eq!(
        events
            .matches("lock_vault(&state, &widgets, &pending)")
            .count(),
        2,
        "both the focus-loss handler and the idle timer must call lock_vault"
    );
}

#[test]
fn the_watcher_never_parses_the_encrypted_store() {
    let source = include_str!("../src/ui.rs");
    // The stat-only snapshot follows the vault's own watch_paths(), which for
    // an encrypted vault is the opaque ciphertext store.
    let snapshot = function_body(source, "note_tree_snapshot");
    assert!(snapshot.contains("vault.watch_paths()"));
    assert!(
        !snapshot.contains("vault.notes_dir()") && !snapshot.contains("vault.trash_dir()"),
        "note_tree_snapshot must not hard-code the plaintext trees"
    );
    // The poll skips reconciliation entirely while the vault is locked.
    let poll = function_body(source, "install_watcher_poll");
    assert!(poll.contains("Vault::is_locked"));
    assert!(poll.contains("editor_is_clean && !vault_locked"));
}

#[test]
fn encrypted_vault_session_state_is_never_written_to_the_plaintext_config() {
    // last_view is a notebook name and last_note is a note UUID - neither may
    // reach ~/.config for an encrypted vault.
    let source = include_str!("../src/ui.rs");
    let persist = function_body(source, "persist_vault_session_state");
    let guard_at = persist
        .find("if is_encrypted {")
        .expect("persist_vault_session_state must branch on an encrypted vault");
    let return_at = persist[guard_at..]
        .find("return;")
        .map(|offset| guard_at + offset)
        .expect("the encrypted branch returns before the config write");
    let write_at = persist
        .find("state.config.set_vault_session(")
        .expect("it writes the session to config for a Standard Vault");
    assert!(
        guard_at < return_at && return_at < write_at,
        "the encrypted-vault early return must precede the config write"
    );
    // An unlocked Secure Vault seals its session into the manifest instead.
    assert!(persist.contains("vault.set_encrypted_session_state(session)"));
    // A locked Secure Vault drops the working copy entirely (no key to seal).
    assert!(persist.contains("if !is_locked"));

    // Loading mirrors it: the sealed manifest for a Secure Vault, config for a
    // Standard one.
    let load = function_body(source, "load_vault_session_state");
    assert!(load.contains("vault.encrypted_session_state()"));
    assert!(load.contains("vault_session(vault.vault_id())"));
}

#[test]
fn hkdf_labels_match_the_format_document() {
    // The HKDF info labels are format-stable: the code constants and the
    // published format doc must not drift apart.
    let crypto = include_str!("../src/crypto/vault.rs");
    let doc = include_str!("../docs/ENCRYPTED_VAULT_FORMAT.md");
    for label in [
        "senatorialnotes/vault/v1/content",
        "senatorialnotes/vault/v1/names",
        "senatorialnotes/vault/v1/attachments",
        "senatorialnotes/vault/v1/metadata",
        "senatorialnotes/vault/v1/index",
    ] {
        assert!(
            crypto.contains(label),
            "HKDF label {label:?} missing from crypto/vault.rs"
        );
        assert!(
            doc.contains(label),
            "HKDF label {label:?} missing from docs/ENCRYPTED_VAULT_FORMAT.md"
        );
    }
}

// ---------------------------------------------------------------------------
// v0.3 UX package: note-header quick actions, vault lock control, settings
// ---------------------------------------------------------------------------

#[test]
fn the_note_header_carries_favourite_pin_and_note_level_lock_quick_actions() {
    let source = include_str!("../src/ui.rs");
    // The three quick buttons plus an overflow menu button exist and are laid
    // into the note title row.
    let build = function_body(source, "build_window");
    for widget in [
        "let note_lock_button = gtk::Button::from_icon_name",
        "let note_favourite_button = gtk::Button::from_icon_name",
        "let note_pin_button = gtk::Button::from_icon_name",
        "let note_overflow_button = gtk::MenuButton::builder()",
        "title_row.append(&note_favourite_button)",
        "title_row.append(&note_pin_button)",
        "title_row.append(&note_overflow_button)",
    ] {
        assert!(build.contains(widget), "note header missing `{widget}`");
    }

    // Favourite / Pin toggle immediately through the shared flag path, and the
    // header refreshes right after.
    let toggle = function_body(source, "toggle_note_flag");
    assert!(toggle.contains("update_note_quick_actions(state, widgets)"));

    // The quick buttons are wired to the flag toggles and the note-level lock
    // dispatcher.
    assert!(source.contains("toggle_note_flag(NoteFlag::Favourite, id, &state, &widgets)"));
    assert!(source.contains("note_quick_lock(&state, &widgets, &pending)"));
}

#[test]
fn the_note_header_lock_is_note_level_and_never_locks_the_vault() {
    let source = include_str!("../src/ui.rs");
    let quick = function_body(source, "note_quick_lock");
    // Dispatches on the note's own state...
    assert!(quick.contains("lock_active_note(state, widgets)"));
    assert!(quick.contains("encrypt_active_note(state, widgets)"));
    assert!(quick.contains("open_note_by_id(id, state, widgets, true)"));
    // ...and never calls the whole-vault lock.
    assert!(
        !quick.contains("lock_vault("),
        "the note-header lock must never invoke lock_vault"
    );

    // `lock_active_note` only touches the active document, not the vault key or
    // the session cache of other notes.
    let lock_one = function_body(source, "lock_active_note");
    assert!(
        !lock_one.contains("vault.lock()"),
        "note lock must not drop the vault key"
    );
    assert!(
        !lock_one.contains("unlocked_cache.drain()"),
        "note lock must not touch other notes"
    );
    assert!(lock_one.contains("ActiveDocument::is_encrypted"));
    assert!(lock_one.contains("update_note_quick_actions(state, widgets)"));

    // The overflow menu keeps the uncommon actions and NOT favourite / pin.
    let overflow = function_body(source, "note_overflow_menu");
    assert!(overflow.contains("app.context-move-to-notebook"));
    assert!(overflow.contains("app.context-move-to-trash"));
    assert!(overflow.contains("Change Note Password"));
    assert!(overflow.contains("Remove Note Encryption"));
    assert!(
        !overflow.contains("Favourite") && !overflow.contains("\"Pin\""),
        "favourite / pin must not be buried in the overflow menu"
    );
}

#[test]
fn the_vault_lock_control_is_secure_vault_only() {
    let source = include_str!("../src/ui.rs");
    let build = function_body(source, "build_window");
    assert!(
        build.contains("let vault_lock_button = labeled_icon_button(\"Lock Vault\""),
        "the header must carry a distinct vault-level Lock Vault button"
    );

    // Its visibility follows the open vault: an unlocked Secure Vault only.
    let controls = function_body(source, "update_vault_lock_controls");
    assert!(controls.contains("vault.is_encrypted()"));
    assert!(controls.contains("set_visible(is_secure && !is_locked)"));

    // The button and the app action both go to `lock_vault`, never a note lock.
    assert!(source.contains("lock_vault(&state, &widgets, &pending)"));

    // A Standard Vault's switcher disables the vault-security actions.
    let switcher = function_body(source, "render_vault_switcher");
    assert!(switcher.contains(r#"toggle("lock-vault", secure_vault_unlocked)"#));
}

#[test]
fn a_locked_secure_vault_disables_search_and_re_enables_it_on_unlock() {
    let source = include_str!("../src/ui.rs");
    let locked = function_body(source, "show_vault_locked_screen");
    assert!(locked.contains("widgets.search.set_sensitive(false)"));
    let entered = function_body(source, "enter_vault_workspace");
    assert!(entered.contains("widgets.search.set_sensitive(true)"));
    // The sidebar search placeholder is a stable product string, never a
    // per-view label.
    let build = function_body(source, "build_window");
    assert!(build.contains(r#".placeholder_text("Search this vault")"#));
}

#[test]
fn secure_vault_settings_opens_a_focused_dialog_not_generic_preferences() {
    let source = include_str!("../src/ui.rs");
    // A dedicated action + dialog.
    assert!(source.contains(r#"gio::SimpleAction::new("vault-settings", None)"#));
    let dialog = function_body(source, "show_vault_settings");
    assert!(dialog.contains("adw::PreferencesWindow::builder()"));
    assert!(dialog.contains("Secure Vault Settings"));
    // Auto-Lock, Security and General groups.
    assert!(dialog.contains(r#"adw::PreferencesGroup::builder()"#));
    assert!(dialog.contains("Lock after inactivity"));
    assert!(dialog.contains("Lock when the app loses focus"));
    assert!(dialog.contains("Lock when minimized"));
    assert!(dialog.contains("Change Vault Password"));
    assert!(dialog.contains("Rename Vault"));
    // The Secure-Vault security controls are gated behind `is_secure`.
    assert!(dialog.contains("if is_secure {"));

    // The popover's old "Auto-Lock Settings…" entry no longer points at the
    // generic Preferences window.
    let build = function_body(source, "build_vault_popover");
    assert!(
        !build.contains(r#"auto_lock_button.set_action_name(Some("app.preferences"))"#),
        "the vault popover must not route settings to generic Preferences"
    );
}

#[test]
fn recently_opened_is_recorded_when_a_note_is_displayed_not_when_it_is_saved() {
    let source = include_str!("../src/ui.rs");
    // Recorded in the common display sink...
    let display = function_body(source, "display_document");
    assert!(display.contains("record_note_opened(&mut state, note_id)"));
    // ...and `record_note_opened` only touches the in-memory session.
    let record = function_body(source, "record_note_opened");
    assert!(record.contains("state.session.record_opened(id)"));
    assert!(
        !record.contains("save") && !record.contains("write"),
        "recording a view must never persist the note"
    );
    // The smart view is filtered by the session token, not mtime.
    let includes = function_body(source, "view_includes");
    let recently_arm = includes
        .split("ViewMode::RecentlyOpened =>")
        .nth(1)
        .expect("a RecentlyOpened arm");
    assert!(recently_arm.contains("recently_opened.contains(&summary.id)"));
    assert!(recently_arm.contains("!summary.locked"));
}
