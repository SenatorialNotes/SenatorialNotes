use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use adw::{Application, ApplicationWindow};
use gtk::gdk;
use gtk::gio;
use gtk::glib;
use gtk::glib::variant::ToVariant;
use senatorial_notes::config::{
    Accent, AppConfig, NoteListDensity, SortOrder, Theme, VaultSessionState,
};
use senatorial_notes::constants::{APP_ID, APP_NAME, MIN_PASSWORD_LENGTH, PRIVACY_STATEMENT};
use senatorial_notes::crypto::vault::{self as vault_crypto, VaultKeys};
use senatorial_notes::formatting::{FormatAction, apply_markdown_format};
use senatorial_notes::markdown_spans::{self, SpanKind};
use senatorial_notes::search::summary_matches;
use senatorial_notes::sort::sort_notes;
use senatorial_notes::ui_state::{
    FilterState, RowTarget, SelectionCoordinator, SelectionIntent, SessionRegistry, UiFlow,
    ViewMode,
};
use senatorial_notes::vault_export::{
    ExportParams, ExportProgress, ExportReport, export_secure_vault_to_standard,
};
use senatorial_notes::vault_lock::{
    BlockedReason, DeadReason, LockAcquisition, LockStatus, VaultLock,
};
use senatorial_notes::vault_quarantine::{ArtifactCategory, QuarantineReport};
use senatorial_notes::watcher::VaultWatcher;
use senatorial_notes::{
    EncryptedSession, FileStamp, Note, NoteMetadata, NoteSummary, NotebookEntry, TrashEntry, Vault,
    VaultKind, paths,
};
use sourceview5::prelude::*;
use uuid::Uuid;
use zeroize::Zeroizing;

enum ActiveDocument {
    Plain {
        note: Note,
        stamp: FileStamp,
    },
    Encrypted {
        note: Note,
        stamp: FileStamp,
        session: EncryptedSession,
    },
}

impl ActiveDocument {
    fn note(&self) -> &Note {
        match self {
            Self::Plain { note, .. } | Self::Encrypted { note, .. } => note,
        }
    }

    fn id(&self) -> Uuid {
        self.note().metadata.id
    }

    fn note_mut(&mut self) -> &mut Note {
        match self {
            Self::Plain { note, .. } | Self::Encrypted { note, .. } => note,
        }
    }

    fn is_encrypted(&self) -> bool {
        matches!(self, Self::Encrypted { .. })
    }

    fn stamp(&self) -> &FileStamp {
        match self {
            Self::Plain { stamp, .. } | Self::Encrypted { stamp, .. } => stamp,
        }
    }

    fn clear_sensitive(&mut self) {
        match self {
            Self::Plain { note, .. } | Self::Encrypted { note, .. } => note.clear_sensitive(),
        }
    }
}

#[derive(Default)]
struct AppState {
    config: AppConfig,
    vault: Option<Vault>,
    notes: Vec<NoteSummary>,
    trash: Vec<TrashEntry>,
    active: Option<ActiveDocument>,
    unlocked_cache: HashMap<Uuid, ActiveDocument>,
    /// Clean, already-parsed plaintext notes kept in memory so switching back to
    /// one does not re-read and re-parse the file. Each entry is validated
    /// against the file's modification time and length before it is reused.
    plain_cache: HashMap<Uuid, (Note, FileStamp)>,
    watcher: Option<VaultWatcher>,
    /// Cheap stat-only snapshot (path, mtime, length) of the notes and trash
    /// trees as SenatorialNotes last wrote them. The watcher poll compares
    /// against this so a just-committed internal atomic write does not trigger a
    /// redundant vault-wide rescan.
    watch_baseline: Vec<(std::path::PathBuf, std::time::SystemTime, u64)>,
    body_dirty: bool,
    title_dirty: bool,
    title_draft: String,
    flow: UiFlow,
    filter: FilterState,
    /// Working copy of the currently open vault's per-vault session state
    /// (last note, last view, recently-opened notes, editor scroll). Loaded
    /// on vault open - from the app config for a Standard Vault, from the
    /// sealed manifest for a Secure Vault - updated as the user views notes,
    /// and persisted on leave / lock. `recently_opened` powers the "Recently
    /// Opened" smart view.
    session: VaultSessionState,
    last_sensitive_activity: Option<Instant>,
    /// Mirrors `Vault::is_read_only()` for the currently open vault, cached so
    /// the UI can gate mutation controls without holding the `Vault`.
    read_only: bool,
    /// The advisory lock for the currently open vault. A writable session owns
    /// it (dropping it releases the lock file); a read-only session holds a
    /// non-owning `VaultLock::read_only()` handle. `None` before any vault is
    /// open.
    vault_lock: Option<VaultLock>,
}

/// Upper bound on cached clean plaintext documents. Notes are small, but this
/// keeps memory bounded on very large vaults.
const PLAIN_CACHE_LIMIT: usize = 48;

#[derive(Default)]
struct PendingSaves {
    body: Option<glib::SourceId>,
    title: Option<glib::SourceId>,
}

#[derive(Clone)]
struct RowWidgets {
    title: gtk::Label,
    preview: gtk::Label,
    pin: gtk::Image,
    favourite: gtk::Image,
    archived: gtk::Image,
}

type RowWidgetMap = Rc<RefCell<HashMap<Uuid, RowWidgets>>>;

#[derive(Default)]
struct SignalGate {
    depth: Cell<u32>,
}

impl SignalGate {
    fn suppress(&self) -> SignalSuppression<'_> {
        self.depth.set(self.depth.get().saturating_add(1));
        SignalSuppression { gate: self }
    }

    fn is_suppressed(&self) -> bool {
        self.depth.get() > 0
    }
}

struct SignalSuppression<'a> {
    gate: &'a SignalGate,
}

impl Drop for SignalSuppression<'_> {
    fn drop(&mut self) {
        self.gate.depth.set(self.gate.depth.get().saturating_sub(1));
    }
}

#[derive(Clone)]
struct Widgets {
    window: ApplicationWindow,
    stack: gtk::Stack,
    welcome_status: gtk::Label,
    /// Vault name shown in the header switcher button.
    vault_label: gtk::Label,
    /// Session generation for the currently open vault. Every vault open/switch
    /// bumps it; a deferred callback armed under an older generation becomes
    /// inert (see [`SessionRegistry`] and `open_vault`).
    sessions: Rc<SessionRegistry>,
    /// The header multi-vault control's popover (current vault identity +
    /// Open Vault… + Open Recent).
    vault_popover: gtk::Popover,
    vault_popover_name: gtk::Label,
    vault_popover_path: gtk::Label,
    /// Read-only / migration-warning line inside the popover; hidden when empty.
    vault_popover_status: gtk::Label,
    /// Rebuilt list of recent-vault rows inside the popover.
    vault_recent_box: gtk::Box,
    /// Secure-Vault-only actions in the popover (Lock Vault, Change Vault
    /// Password…). Hidden for a Standard Vault or a locked Secure Vault.
    vault_popover_secure_actions: gtk::Box,
    /// Lock glyph in the header button, visible only for a read-only vault.
    vault_readonly_icon: gtk::Image,
    /// Padlock glyph before the vault name: closed while a Secure Vault is
    /// locked, open while unlocked. Hidden for a Standard Vault.
    vault_state_icon: gtk::Image,
    /// Header "Lock Vault" button. Locks the whole Secure Vault (distinct from
    /// the per-note lock). Visible only for an unlocked Secure Vault.
    vault_lock_button: gtk::Button,
    /// Recent-vault list on the welcome screen (hidden when there are none).
    welcome_recent_box: gtk::Box,
    welcome_recent_heading: gtk::Label,
    /// One-time first-run panel on the welcome screen.
    first_run_panel: gtk::Box,
    /// Header mutation controls, kept so `apply_read_only_ui` can disable them.
    new_note: gtk::Button,
    new_notebook: gtk::Button,
    delete_button: gtk::Button,
    notes_heading: gtk::Label,
    search: gtk::SearchEntry,
    note_list: gtk::ListBox,
    row_menu: gtk::PopoverMenu,
    note_list_stack: gtk::Stack,
    note_list_empty_title: gtk::Label,
    note_list_empty_copy: gtk::Label,
    row_widgets: RowWidgetMap,
    selection: Rc<SelectionCoordinator>,
    /// Newest requested row selection, consumed once by a coalesced dispatch so
    /// a burst of rapid clicks performs a single load of the final target rather
    /// than one full load per intermediate click.
    pending_select: Rc<Cell<Option<RowTarget>>>,
    /// The single outstanding coalesced-selection timeout, if one is armed. Held
    /// so it can be cancelled on shutdown and so the watcher poll can tell that a
    /// selection dispatch is in flight and stay out of its way.
    select_source: Rc<RefCell<Option<glib::SourceId>>>,
    editor_events: Rc<SignalGate>,
    library_split: adw::OverlaySplitView,
    content_split: adw::NavigationSplitView,
    all_notes_button: gtk::Button,
    inbox_button: gtk::Button,
    pinned_button: gtk::Button,
    recently_opened_button: gtk::Button,
    favourites_button: gtk::Button,
    archive_button: gtk::Button,
    trash_button: gtk::Button,
    /// Rebuilt list of the user's Secure Vaults in the sidebar (bounded, with
    /// a "More…" affordance). Filled by `render_secure_vaults_sidebar`.
    secure_vaults_box: gtk::Box,
    /// Dynamic list of user notebooks (everything except `Inbox`, which has
    /// its own fixed sidebar row). One `gtk::ListBox` row per notebook, in
    /// the same order as `notebook_rows`.
    notebook_list: gtk::ListBox,
    notebook_menu: gtk::PopoverMenu,
    notebook_rows: Rc<RefCell<Vec<PathBuf>>>,
    notebook_events: Rc<SignalGate>,
    /// Sidebar tag filter chips, rebuilt alongside the note list.
    tags_flow: gtk::FlowBox,
    tags_events: Rc<SignalGate>,
    document_stack: gtk::Stack,
    title: gtk::Entry,
    /// Chip row for the active note's tags, shown between the title and the
    /// formatting toolbar. Rebuilt whenever the active note or its tags
    /// change; hidden while no note is open or the open note is locked.
    tags_row: gtk::Box,
    tag_chips: gtk::Box,
    tag_add_entry: gtk::Entry,
    buffer: sourceview5::Buffer,
    editor: sourceview5::View,
    /// The single outstanding debounced Markdown live-preview style
    /// recompute, if one is armed. A separate timer from the autosave ones
    /// in `PendingSaves` - restyling and saving are independent concerns
    /// with independent delays.
    style_recompute_source: Rc<RefCell<Option<glib::SourceId>>>,
    formatting_bar: gtk::ScrolledWindow,
    /// Bold/Italic toolbar buttons. The existing theme/accent-aware
    /// `brand-accent` CSS class is toggled on whichever are active at the
    /// cursor/selection (see `active_formats_at`) - plain buttons rather
    /// than `GtkToggleButton`s, since they are also wired to
    /// `app.format-*` via `set_action_name` and a toggle button's own
    /// click-driven active state would fight this external,
    /// cursor-position-driven one.
    format_bold_button: gtk::Button,
    format_italic_button: gtk::Button,
    /// The single outstanding idle-deferred toolbar active-state update, if
    /// one is armed. See `schedule_format_toolbar_update`.
    format_toolbar_update_source: Rc<RefCell<Option<glib::SourceId>>>,
    /// The last state actually applied to the toolbar buttons, so a
    /// same-state re-notification (cursor moves within a run of identically
    /// formatted text) never re-touches the buttons' CSS classes.
    format_toolbar_state: Rc<Cell<ActiveFormats>>,
    save_status: gtk::Label,
    /// Note-header quick actions. `note_lock_button` is note-level only: it
    /// encrypts an ordinary note, unlocks a locked encrypted note, or locks an
    /// unlocked one - it never locks the whole Secure Vault.
    note_lock_button: gtk::Button,
    note_favourite_button: gtk::Button,
    note_pin_button: gtk::Button,
    note_overflow_button: gtk::MenuButton,
    locked_copy: gtk::Label,
    /// Status/error line on the whole-vault lock screen.
    vault_locked_status: gtk::Label,
    /// "Unlock Vault" button on the whole-vault lock screen.
    vault_unlock_button: gtk::Button,
    trash_detail_title: gtk::Label,
    empty_trash_button: gtk::Button,
    appearance_provider: gtk::CssProvider,
}

struct Controls {
    create_vault: gtk::Button,
    create_encrypted_vault: gtk::Button,
    open_vault: gtk::Button,
    first_run_start: gtk::Button,
    first_run_secure: gtk::Button,
    new_note: gtk::Button,
    new_notebook: gtk::Button,
    all_notes: gtk::Button,
    inbox: gtk::Button,
    pinned: gtk::Button,
    recently_opened: gtk::Button,
    favourites: gtk::Button,
    archive: gtk::Button,
    trash: gtk::Button,
    new_secure_vault: gtk::Button,
    library_toggle: gtk::Button,
    back_to_notes: gtk::Button,
    unlock: gtk::Button,
    restore: gtk::Button,
    permanently_delete: gtk::Button,
}

pub fn run() -> glib::ExitCode {
    let application = Application::builder().application_id(APP_ID).build();
    application.connect_startup(install_base_styles);
    application.connect_activate(build_application);
    application.run()
}

fn install_base_styles(_application: &Application) {
    let Some(display) = gdk::Display::default() else {
        return;
    };
    let provider = gtk::CssProvider::new();
    provider.load_from_data(include_str!("../data/style.css"));
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

fn build_application(application: &Application) {
    if let Some(window) = application.active_window() {
        window.present();
        return;
    }

    let (widgets, controls) = build_window(application);
    let (config, config_error) = match AppConfig::load() {
        Ok(config) => (config, None),
        Err(error) => (AppConfig::default(), Some(error)),
    };
    let state = Rc::new(RefCell::new(AppState {
        config,
        ..AppState::default()
    }));
    let pending = Rc::new(RefCell::new(PendingSaves::default()));

    let initial_config = { state.borrow().config.clone() };
    apply_appearance(&initial_config, &widgets);
    connect_theme_updates(&widgets);

    if let Some(error) = config_error {
        show_welcome_error(
            &widgets,
            &format!("Settings could not be loaded; defaults are in use: {error}"),
        );
    }

    connect_folder_button(
        &controls.create_vault,
        true,
        state.clone(),
        widgets.clone(),
        pending.clone(),
    );
    connect_folder_button(
        &controls.open_vault,
        false,
        state.clone(),
        widgets.clone(),
        pending.clone(),
    );

    {
        let state = state.clone();
        let widgets = widgets.clone();
        let pending = pending.clone();
        controls
            .create_encrypted_vault
            .connect_clicked(move |_| present_encrypted_vault_creator(&state, &widgets, &pending));
    }

    {
        let state = state.clone();
        let widgets = widgets.clone();
        let pending = pending.clone();
        controls.first_run_start.connect_clicked(move |_| {
            widgets.first_run_panel.set_visible(false);
            let main_path = { state.borrow().config.last_vault.clone() };
            if let Some(path) = main_path {
                open_vault(&path, false, &state, &widgets, &pending);
            }
        });
    }

    {
        let state = state.clone();
        let widgets = widgets.clone();
        let pending = pending.clone();
        controls.first_run_secure.connect_clicked(move |_| {
            present_managed_secure_vault_setup(&state, &widgets, &pending)
        });
    }

    {
        let state = state.clone();
        let widgets = widgets.clone();
        let pending = pending.clone();
        widgets
            .vault_unlock_button
            .clone()
            .connect_clicked(move |_| begin_vault_unlock(&state, &widgets, &pending));
    }

    {
        let state = state.clone();
        let widgets = widgets.clone();
        let pending = pending.clone();
        widgets.vault_lock_button.clone().connect_clicked(move |_| {
            // Vault-level lock: seals the whole Secure Vault. Never a per-note
            // operation. `lock_vault` shows the locked-vault workspace itself.
            lock_vault(&state, &widgets, &pending);
        });
    }

    {
        let state = state.clone();
        let widgets = widgets.clone();
        let pending = pending.clone();
        widgets
            .note_lock_button
            .clone()
            .connect_clicked(move |_| note_quick_lock(&state, &widgets, &pending));
    }

    {
        let state = state.clone();
        let widgets = widgets.clone();
        widgets
            .note_favourite_button
            .clone()
            .connect_clicked(move |_| {
                if let Some(id) = current_note_id(&state) {
                    toggle_note_flag(NoteFlag::Favourite, id, &state, &widgets);
                }
            });
    }

    {
        let state = state.clone();
        let widgets = widgets.clone();
        widgets.note_pin_button.clone().connect_clicked(move |_| {
            if let Some(id) = current_note_id(&state) {
                toggle_note_flag(NoteFlag::Pinned, id, &state, &widgets);
            }
        });
    }

    {
        let split = widgets.library_split.clone();
        split.set_show_sidebar(true);
        split.connect_collapsed_notify(|split| {
            split.set_show_sidebar(!split.is_collapsed());
        });
    }

    {
        let split = widgets.library_split.clone();
        controls.library_toggle.connect_clicked(move |_| {
            split.set_show_sidebar(!split.shows_sidebar());
        });
    }

    {
        let split = widgets.content_split.clone();
        split.connect_collapsed_notify(|split| {
            if split.is_collapsed() {
                split.set_show_content(false);
            }
        });
    }

    {
        let split = widgets.content_split.clone();
        controls.back_to_notes.connect_clicked(move |_| {
            split.set_show_content(false);
        });
    }

    {
        let state = state.clone();
        let widgets = widgets.clone();
        let pending = pending.clone();
        controls
            .new_note
            .connect_clicked(move |_| create_new_note(&state, &widgets, &pending));
    }

    {
        let state = state.clone();
        let widgets = widgets.clone();
        let pending = pending.clone();
        controls.all_notes.connect_clicked(move |_| {
            cancel_all_timers(&pending);
            switch_view(ViewMode::AllNotes, &state, &widgets);
        });
    }

    {
        let state = state.clone();
        let widgets = widgets.clone();
        let pending = pending.clone();
        controls.inbox.connect_clicked(move |_| {
            cancel_all_timers(&pending);
            switch_view(ViewMode::Notebook(PathBuf::from("Inbox")), &state, &widgets);
        });
    }

    {
        let state = state.clone();
        let widgets = widgets.clone();
        let pending = pending.clone();
        controls.pinned.connect_clicked(move |_| {
            cancel_all_timers(&pending);
            switch_view(ViewMode::Pinned, &state, &widgets);
        });
    }

    {
        let state = state.clone();
        let widgets = widgets.clone();
        let pending = pending.clone();
        controls.recently_opened.connect_clicked(move |_| {
            cancel_all_timers(&pending);
            switch_view(ViewMode::RecentlyOpened, &state, &widgets);
        });
    }

    {
        let state = state.clone();
        let widgets = widgets.clone();
        let pending = pending.clone();
        controls.favourites.connect_clicked(move |_| {
            cancel_all_timers(&pending);
            switch_view(ViewMode::Favourites, &state, &widgets);
        });
    }

    {
        let state = state.clone();
        let widgets = widgets.clone();
        let pending = pending.clone();
        controls.archive.connect_clicked(move |_| {
            cancel_all_timers(&pending);
            switch_view(ViewMode::Archive, &state, &widgets);
        });
    }

    {
        let state = state.clone();
        let widgets = widgets.clone();
        let pending = pending.clone();
        controls.new_secure_vault.connect_clicked(move |_| {
            present_managed_secure_vault_setup(&state, &widgets, &pending)
        });
    }

    {
        let state = state.clone();
        let widgets = widgets.clone();
        controls
            .new_notebook
            .connect_clicked(move |_| present_new_notebook_dialog(None, &state, &widgets));
    }

    {
        let state = state.clone();
        let widgets = widgets.clone();
        let notebook_list = widgets.notebook_list.clone();
        notebook_list.connect_row_selected(move |_, row| {
            if widgets.notebook_events.is_suppressed() {
                return;
            }
            let Some(row) = row else {
                return;
            };
            let path = {
                widgets
                    .notebook_rows
                    .borrow()
                    .get(row.index() as usize)
                    .cloned()
            };
            if let Some(path) = path {
                switch_view(ViewMode::Notebook(path), &state, &widgets);
            }
        });
    }

    {
        let state = state.clone();
        let widgets = widgets.clone();
        widgets
            .tag_add_entry
            .clone()
            .connect_activate(move |entry| {
                let tag = entry.text().to_string();
                entry.set_text("");
                add_tag_to_active_note(&tag, &state, &widgets);
            });
    }

    {
        let state = state.clone();
        let widgets = widgets.clone();
        let pending = pending.clone();
        controls.trash.connect_clicked(move |_| {
            cancel_all_timers(&pending);
            switch_view(ViewMode::Trash, &state, &widgets);
        });
    }

    {
        let state = state.clone();
        let widgets = widgets.clone();
        let pending = pending.clone();
        let buffer = widgets.buffer.clone();
        buffer.connect_changed(move |_| schedule_body_save(&state, &widgets, &pending));
    }

    {
        // A separate debounce from the autosave one above - restyling and
        // saving are independent concerns with independent delays. Skipped
        // during a programmatic load (editor_events suppressed): that path
        // restyles synchronously in display_document instead.
        let widgets = widgets.clone();
        let buffer = widgets.buffer.clone();
        buffer.connect_changed(move |_| {
            if widgets.editor_events.is_suppressed() {
                return;
            }
            schedule_style_recompute(&widgets);
        });
    }

    {
        // `cursor-position` is a GObject property notify, which GObject
        // always fires synchronously at the point the property changes -
        // here, from inside GtkTextBuffer::delete/insert while
        // apply_format_to_buffer is still on the stack. Mutating other
        // widgets (the toolbar buttons' CSS classes) directly from this
        // handler would do it re-entrantly, nested inside an active buffer
        // mutation; schedule_format_toolbar_update defers the actual work
        // to an idle callback so it always runs as its own top-level main
        // loop turn instead (see that function's doc comment).
        let widgets = widgets.clone();
        let buffer = widgets.buffer.clone();
        buffer.connect_cursor_position_notify(move |_| schedule_format_toolbar_update(&widgets));
    }

    {
        let state = state.clone();
        let widgets = widgets.clone();
        let pending = pending.clone();
        let title = widgets.title.clone();
        title.connect_changed(move |entry| {
            if widgets.editor_events.is_suppressed() {
                return;
            }
            let should_schedule = {
                let mut state = state.borrow_mut();
                if state.active.is_none() {
                    return;
                }
                state.title_draft = entry.text().to_string();
                state.title_dirty = true;
                touch_sensitive_activity(&mut state);
                true
            };
            if !should_schedule {
                return;
            }
            widgets.save_status.set_label("Title not yet committed");
            schedule_title_commit(&state, &widgets, &pending);
        });
    }

    {
        let state = state.clone();
        let widgets = widgets.clone();
        let pending = pending.clone();
        widgets.title.clone().connect_activate(move |_| {
            cancel_title_timer(&pending);
            persist_active(&state, &widgets, true);
        });
    }

    {
        let state = state.clone();
        let widgets = widgets.clone();
        let pending = pending.clone();
        let focus = gtk::EventControllerFocus::new();
        let widgets_for_leave = widgets.clone();
        focus.connect_leave(move |_| {
            cancel_title_timer(&pending);
            persist_active(&state, &widgets_for_leave, true);
        });
        widgets.title.add_controller(focus);
    }

    {
        let state = state.clone();
        let widgets = widgets.clone();
        let pending = pending.clone();
        let note_list = widgets.note_list.clone();
        note_list.connect_row_selected(move |_, row| {
            let Some(row) = row else {
                return;
            };
            match widgets.selection.selection_intent(row.index()) {
                SelectionIntent::Suppressed | SelectionIntent::None => {}
                SelectionIntent::Activate(target) => {
                    request_selection(target, &state, &widgets, &pending);
                }
            }
        });
    }

    {
        let state = state.clone();
        let widgets = widgets.clone();
        widgets.search.clone().connect_search_changed(move |_| {
            render_note_list(&state, &widgets);
        });
    }

    {
        let state = state.clone();
        let widgets = widgets.clone();
        controls.unlock.connect_clicked(move |_| {
            if let Some(id) = selected_row_target(&widgets).and_then(|target| match target {
                RowTarget::Note(id) => Some(id),
                RowTarget::Trash(_) => None,
            }) {
                load_note_by_id(id, &state, &widgets);
            }
        });
    }

    {
        let state = state.clone();
        let widgets = widgets.clone();
        controls
            .restore
            .connect_clicked(move |_| restore_selected(&state, &widgets));
    }

    {
        let state = state.clone();
        let widgets = widgets.clone();
        controls
            .permanently_delete
            .connect_clicked(move |_| confirm_permanent_delete(&state, &widgets));
    }

    {
        let state = state.clone();
        let widgets = widgets.clone();
        widgets
            .empty_trash_button
            .clone()
            .connect_clicked(move |_| confirm_empty_trash(&state, &widgets));
    }

    install_list_delete_key(&state, &widgets);
    install_actions(application, state.clone(), widgets.clone(), pending.clone());

    {
        let state = state.clone();
        let widgets = widgets.clone();
        let pending = pending.clone();
        widgets.window.clone().connect_close_request(move |_| {
            cancel_all_timers(&pending);
            cancel_pending_selection(&widgets);
            if let Some(source) = widgets.style_recompute_source.borrow_mut().take() {
                source.remove();
            }
            if let Some(source) = widgets.format_toolbar_update_source.borrow_mut().take() {
                source.remove();
            }
            if persist_active(&state, &widgets, true) {
                // Persist the per-vault session (last note/view, Recently
                // Opened) while a Secure Vault is still unlocked to seal it.
                persist_vault_session_state(&state, &widgets);
                clear_sensitive_documents(&state);
                // Release the advisory vault lock cleanly on a normal exit.
                {
                    state.borrow_mut().vault_lock.take();
                }
                // Detach the shared context menu before its parent is disposed.
                widgets.row_menu.unparent();
                glib::Propagation::Proceed
            } else {
                glib::Propagation::Stop
            }
        });
    }

    connect_locking_events(&state, &widgets, &pending);
    install_watcher_poll(&state, &widgets);

    // Startup. A truly fresh install (no config, no known vaults) gets a
    // managed workspace with a ready-to-use "Main" Standard Vault created and
    // opened automatically - the user can write a note immediately without a
    // folder picker. Otherwise, restore the previous vault; a missing one
    // leaves the welcome screen (never silently re-created).
    let (is_first_run, last_vault) = {
        let config = &state.borrow().config;
        let fresh = !config.first_run_done
            && config.last_vault.is_none()
            && config.recent_vaults.is_empty();
        (fresh, config.last_vault.clone())
    };
    if is_first_run {
        run_first_run_setup(&state, &widgets, &pending);
    } else {
        match last_vault {
            Some(path) if path.is_dir() => open_vault(&path, false, &state, &widgets, &pending),
            Some(path) => {
                show_welcome_error(
                    &widgets,
                    &format!(
                        "The last vault at {} is no longer there. Open another vault to continue.",
                        path.display()
                    ),
                );
            }
            None => {}
        }
    }
    // Populate the welcome-screen recent list even when no vault opened.
    render_vault_switcher(&state, &widgets, &pending);

    widgets.window.present();
}

/// Builds the header vault-switcher popover shell. The current-vault labels and
/// the recent list are filled in later by `render_vault_switcher`.
struct VaultPopover {
    popover: gtk::Popover,
    name: gtk::Label,
    path: gtk::Label,
    status: gtk::Label,
    recent_box: gtk::Box,
    /// Vault-level security actions (Lock Vault, Change Vault Password…).
    /// Shown only while the current vault is an unlocked Secure Vault - a
    /// Standard Vault has no vault-level key to lock.
    secure_actions: gtk::Box,
}

fn build_vault_popover() -> VaultPopover {
    let popover = gtk::Popover::new();
    popover.set_position(gtk::PositionType::Bottom);
    let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
    content.set_margin_start(12);
    content.set_margin_end(12);
    content.set_margin_top(12);
    content.set_margin_bottom(12);
    content.set_width_request(320);

    let name = gtk::Label::new(None);
    name.add_css_class("heading");
    name.set_xalign(0.0);
    name.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    content.append(&name);

    let path = gtk::Label::new(None);
    path.add_css_class("caption");
    path.add_css_class("dim-label");
    path.set_xalign(0.0);
    path.set_selectable(true);
    path.set_wrap(true);
    path.set_max_width_chars(40);
    content.append(&path);

    let status = gtk::Label::new(None);
    status.add_css_class("caption");
    status.set_xalign(0.0);
    status.set_wrap(true);
    status.set_max_width_chars(40);
    status.set_visible(false);
    content.append(&status);

    content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

    // Applies to any open vault. Opens the focused per-vault settings dialog
    // (rename for a Standard Vault; auto-lock + security + rename for a Secure
    // Vault) - never the generic Preferences window.
    let rename_button = gtk::Button::with_label("Vault Settings…");
    rename_button.add_css_class("flat");
    rename_button.set_halign(gtk::Align::Start);
    rename_button.set_tooltip_text(Some(
        "Rename this vault (display name only) and, for a Secure Vault, its auto-lock and security options",
    ));
    rename_button.set_action_name(Some("app.vault-settings"));
    content.append(&rename_button);

    // Vault-level security actions - a Secure Vault only.
    let secure_actions = gtk::Box::new(gtk::Orientation::Vertical, 4);
    secure_actions.set_visible(false);
    let lock_vault_button = gtk::Button::with_label("Lock Vault");
    lock_vault_button.add_css_class("flat");
    lock_vault_button.set_halign(gtk::Align::Start);
    lock_vault_button.set_tooltip_text(Some(
        "Lock this Secure Vault and clear its notes from memory",
    ));
    lock_vault_button.set_action_name(Some("app.lock-vault"));
    secure_actions.append(&lock_vault_button);
    let change_vault_password_button = gtk::Button::with_label("Change Vault Password…");
    change_vault_password_button.add_css_class("flat");
    change_vault_password_button.set_halign(gtk::Align::Start);
    change_vault_password_button.set_action_name(Some("app.change-vault-password"));
    secure_actions.append(&change_vault_password_button);
    let auto_lock_button = gtk::Button::with_label("Auto-Lock & Security…");
    auto_lock_button.add_css_class("flat");
    auto_lock_button.set_halign(gtk::Align::Start);
    auto_lock_button.set_tooltip_text(Some(
        "Auto-Lock, Change Vault Password and Rename for this Secure Vault",
    ));
    auto_lock_button.set_action_name(Some("app.vault-settings"));
    secure_actions.append(&auto_lock_button);
    content.append(&secure_actions);

    content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

    let open_button = gtk::Button::with_label("Open Vault…");
    open_button.add_css_class("flat");
    open_button.set_halign(gtk::Align::Start);
    open_button.set_action_name(Some("app.open-vault"));
    content.append(&open_button);

    let create_button = gtk::Button::with_label("Create Vault…");
    create_button.add_css_class("flat");
    create_button.set_halign(gtk::Align::Start);
    create_button.set_tooltip_text(Some("Choose a folder for a new local notes vault"));
    create_button.set_action_name(Some("app.create-vault"));
    content.append(&create_button);

    let create_encrypted_button = gtk::Button::with_label("Create Secure Vault…");
    create_encrypted_button.add_css_class("flat");
    create_encrypted_button.set_halign(gtk::Align::Start);
    create_encrypted_button.set_tooltip_text(Some(
        "Create a vault where everything is encrypted and protected by the vault password",
    ));
    create_encrypted_button.set_action_name(Some("app.create-encrypted-vault"));
    content.append(&create_encrypted_button);

    let recent_heading = gtk::Label::new(Some("Open Recent"));
    recent_heading.add_css_class("caption");
    recent_heading.add_css_class("dim-label");
    recent_heading.set_xalign(0.0);
    content.append(&recent_heading);

    let recent_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
    let recent_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .max_content_height(260)
        .propagate_natural_height(true)
        .child(&recent_box)
        .build();
    content.append(&recent_scroll);

    popover.set_child(Some(&content));
    VaultPopover {
        popover,
        name,
        path,
        status,
        recent_box,
        secure_actions,
    }
}

fn build_window(application: &Application) -> (Widgets, Controls) {
    let window = ApplicationWindow::builder()
        .application(application)
        .title(APP_NAME)
        .default_width(1100)
        .default_height(750)
        .build();
    // Smallest width that still renders the collapsed header bar (New Note +
    // navigation + menu) without an invalid allocation; smallest height that
    // clears the header, formatting toolbar and title row. Every page below
    // these is scrollable, so this is the true native minimum, not padding.
    window.set_size_request(410, 320);

    // Every page that can outgrow a very short window lives inside a scroller
    // (see `scroll_center`), and the stacks are not homogeneous, so the only
    // hard minimum left is the non-scrolling chrome. The window minimum above is
    // kept a little above that so a resize can never drive a layout smaller than
    // it can render, which is what produced the fatal `AdwApplicationWindow`
    // height warning.
    // No transition on the top-level welcome/workspace switch: a running
    // GtkStack transition measures *both* children, which let the welcome page's
    // width leak into the workspace minimum and produced the fatal
    // `AdwApplicationWindow` width warning on a narrow resize.
    let stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::None)
        .vhomogeneous(false)
        .hhomogeneous(false)
        .build();

    let welcome = gtk::Box::new(gtk::Orientation::Vertical, 16);
    welcome.add_css_class("welcome-surface");
    welcome.set_halign(gtk::Align::Center);
    welcome.set_valign(gtk::Align::Center);
    welcome.set_margin_start(32);
    welcome.set_margin_end(32);
    welcome.set_margin_top(32);
    welcome.set_margin_bottom(32);
    let welcome_icon = gtk::Image::from_icon_name("document-edit-symbolic");
    welcome_icon.set_pixel_size(72);
    welcome_icon.add_css_class("brand-accent");
    welcome_icon.update_property(&[gtk::accessible::Property::Label(
        "SenatorialNotes application icon",
    )]);
    welcome.append(&welcome_icon);
    let welcome_title = gtk::Label::new(Some("Welcome to SenatorialNotes"));
    welcome_title.add_css_class("title-1");
    welcome_title.set_wrap(true);
    welcome_title.set_justify(gtk::Justification::Center);
    welcome.append(&welcome_title);
    let welcome_copy = gtk::Label::new(Some(
        "A private writing space built from ordinary Markdown files.\nNo account, cloud service, telemetry, or network connection.",
    ));
    welcome_copy.set_justify(gtk::Justification::Center);
    welcome_copy.set_wrap(true);
    welcome_copy.add_css_class("dim-label");
    welcome.append(&welcome_copy);
    let encryption_copy = gtk::Label::new(Some(
        "Ordinary notes are plaintext. Individual notes can be encrypted with a password, with no recovery or backdoor.",
    ));
    encryption_copy.set_justify(gtk::Justification::Center);
    encryption_copy.set_wrap(true);
    encryption_copy.set_max_width_chars(66);
    encryption_copy.add_css_class("caption");
    welcome.append(&encryption_copy);
    // First-run panel: shown once, right after the managed "Main" vault is
    // created. Lets the user start writing immediately, or set up the Secure
    // Vault password. Hidden on every subsequent launch.
    let first_run_panel = gtk::Box::new(gtk::Orientation::Vertical, 10);
    first_run_panel.set_halign(gtk::Align::Center);
    first_run_panel.set_visible(false);
    let first_run_ready = gtk::Label::new(Some("Your workspace is ready."));
    first_run_ready.add_css_class("title-3");
    first_run_panel.append(&first_run_ready);
    let first_run_detail = gtk::Label::new(Some(
        "Main — standard everyday notes.\n\
         Secure — everything inside is encrypted and protected by your Vault Password.",
    ));
    first_run_detail.set_justify(gtk::Justification::Center);
    first_run_detail.set_wrap(true);
    first_run_detail.add_css_class("dim-label");
    first_run_panel.append(&first_run_detail);
    let first_run_buttons = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    first_run_buttons.set_halign(gtk::Align::Center);
    let first_run_start = gtk::Button::with_label("Start Writing");
    first_run_start.add_css_class("suggested-action");
    let first_run_secure = gtk::Button::with_label("Set Secure Vault Password");
    first_run_buttons.append(&first_run_start);
    first_run_buttons.append(&first_run_secure);
    first_run_panel.append(&first_run_buttons);
    welcome.append(&first_run_panel);

    let welcome_actions = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    welcome_actions.set_halign(gtk::Align::Center);
    let create_vault = gtk::Button::with_label("Create New Vault");
    create_vault.add_css_class("suggested-action");
    create_vault.set_tooltip_text(Some("Choose a folder for a new local notes vault"));
    let open_vault = gtk::Button::with_label("Open Existing Vault");
    open_vault.set_tooltip_text(Some("Choose an existing SenatorialNotes vault"));
    welcome_actions.append(&create_vault);
    welcome_actions.append(&open_vault);
    welcome.append(&welcome_actions);
    let create_encrypted_vault = gtk::Button::with_label("Create Secure Vault…");
    create_encrypted_vault.set_halign(gtk::Align::Center);
    create_encrypted_vault.add_css_class("flat");
    create_encrypted_vault.set_tooltip_text(Some(
        "Create a vault where everything is encrypted and protected by the vault password",
    ));
    welcome.append(&create_encrypted_vault);
    let welcome_recent_heading = gtk::Label::new(Some("Recent vaults"));
    welcome_recent_heading.add_css_class("caption");
    welcome_recent_heading.add_css_class("dim-label");
    welcome_recent_heading.set_visible(false);
    welcome.append(&welcome_recent_heading);
    let welcome_recent_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
    welcome_recent_box.set_visible(false);
    welcome_recent_box.set_halign(gtk::Align::Center);
    welcome.append(&welcome_recent_box);
    let welcome_status = gtk::Label::new(None);
    welcome_status.set_wrap(true);
    welcome_status.set_max_width_chars(66);
    welcome.append(&welcome_status);
    stack.add_named(&scroll_center(&welcome), Some("welcome"));

    let workspace = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let header = adw::HeaderBar::new();
    header.add_css_class("senatorial-header");
    let vault_label = gtk::Label::new(Some(APP_NAME));
    vault_label.add_css_class("heading");
    // The header title must not grow the window minimum for a long vault name.
    vault_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    vault_label.set_max_width_chars(20);
    let vault_readonly_icon = gtk::Image::from_icon_name("changes-prevent-symbolic");
    vault_readonly_icon.set_visible(false);
    vault_readonly_icon.set_tooltip_text(Some("This vault is open read-only"));
    // Lock-state glyph in front of the vault name: a closed padlock while a
    // Secure Vault is locked, an open one while it is unlocked. Hidden for a
    // Standard Vault, which is never locked.
    let vault_state_icon = gtk::Image::from_icon_name("channel-secure-symbolic");
    vault_state_icon.set_visible(false);
    let vault_button_box = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    vault_button_box.append(&vault_readonly_icon);
    vault_button_box.append(&vault_state_icon);
    vault_button_box.append(&vault_label);
    let VaultPopover {
        popover: vault_popover,
        name: vault_popover_name,
        path: vault_popover_path,
        status: vault_popover_status,
        recent_box: vault_recent_box,
        secure_actions: vault_popover_secure_actions,
    } = build_vault_popover();
    let vault_menu = gtk::MenuButton::builder()
        .tooltip_text("Current vault — switch or open another")
        .build();
    vault_menu.set_child(Some(&vault_button_box));
    vault_menu.set_popover(Some(&vault_popover));
    vault_menu.update_property(&[gtk::accessible::Property::Label(
        "Current vault and vault switcher",
    )]);
    header.set_title_widget(Some(&vault_menu));
    let library_toggle = gtk::Button::from_icon_name("sidebar-show-symbolic");
    library_toggle.set_visible(false);
    library_toggle.set_tooltip_text(Some("Show Library"));
    library_toggle.update_property(&[gtk::accessible::Property::Label("Show Library")]);
    header.pack_start(&library_toggle);
    let back_to_notes = gtk::Button::from_icon_name("go-previous-symbolic");
    back_to_notes.set_visible(false);
    back_to_notes.set_tooltip_text(Some("Back to note list"));
    back_to_notes.update_property(&[gtk::accessible::Property::Label("Back to note list")]);
    header.pack_start(&back_to_notes);
    // Vault-level lock control. Distinct from the per-note lock in the note
    // header: this locks the whole Secure Vault. Shown only for an unlocked
    // Secure Vault; a Standard Vault never exposes it.
    let vault_lock_button = labeled_icon_button("Lock Vault", "channel-secure-symbolic");
    vault_lock_button.set_visible(false);
    vault_lock_button.set_tooltip_text(Some("Lock this Secure Vault"));
    vault_lock_button
        .update_property(&[gtk::accessible::Property::Label("Lock this Secure Vault")]);
    header.pack_start(&vault_lock_button);
    let new_note = labeled_icon_button("New Note", "document-new-symbolic");
    new_note.add_css_class("suggested-action");
    new_note.set_tooltip_text(Some("New note (Ctrl+N)"));
    new_note.update_property(&[gtk::accessible::Property::Label("Create a new note")]);
    header.pack_start(&new_note);
    let delete_button = gtk::Button::from_icon_name("user-trash-symbolic");
    delete_button.set_action_name(Some("app.move-to-trash"));
    delete_button.set_tooltip_text(Some("Move note to Trash"));
    delete_button.update_property(&[gtk::accessible::Property::Label("Move note to Trash")]);
    header.pack_start(&delete_button);
    let note_info_button = gtk::Button::from_icon_name("dialog-information-symbolic");
    note_info_button.set_action_name(Some("app.note-info"));
    note_info_button.set_tooltip_text(Some("Note information (Alt+Enter)"));
    note_info_button.update_property(&[gtk::accessible::Property::Label("Note information")]);
    header.pack_end(&note_info_button);
    let menu = application_menu();
    let menu_button = gtk::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .menu_model(&menu)
        .tooltip_text("Main menu")
        .build();
    menu_button.update_property(&[gtk::accessible::Property::Label("Open application menu")]);
    header.pack_end(&menu_button);
    workspace.append(&header);

    let library_split = adw::OverlaySplitView::new();
    library_split.set_vexpand(true);
    library_split.set_sidebar_width_unit(adw::LengthUnit::Px);
    library_split.set_min_sidebar_width(205.0);
    library_split.set_max_sidebar_width(240.0);
    library_split.set_sidebar_width_fraction(0.2);
    library_split.set_enable_show_gesture(true);
    library_split.set_enable_hide_gesture(true);
    let sidebar = gtk::Box::new(gtk::Orientation::Vertical, 8);
    sidebar.add_css_class("senatorial-sidebar");
    sidebar.set_size_request(190, -1);
    sidebar.set_margin_start(10);
    sidebar.set_margin_end(10);
    sidebar.set_margin_top(14);
    sidebar.set_margin_bottom(14);

    let app_title = gtk::Label::new(Some(APP_NAME));
    app_title.set_xalign(0.0);
    app_title.add_css_class("heading");
    sidebar.append(&app_title);

    // Search lives at the top of the sidebar and searches the currently open
    // vault only (never a cross-vault search). Cleared on vault lock.
    let search = gtk::SearchEntry::builder()
        .placeholder_text("Search this vault")
        .margin_bottom(6)
        .build();
    search.update_property(&[gtk::accessible::Property::Label("Search this vault")]);
    sidebar.append(&search);

    let library_heading = gtk::Label::new(Some("NOTES"));
    library_heading.set_xalign(0.0);
    library_heading.add_css_class("sidebar-section-title");
    sidebar.append(&library_heading);
    let all_notes = sidebar_button("All Notes", "view-list-symbolic");
    all_notes.add_css_class("sidebar-selected");
    all_notes.set_tooltip_text(Some("Show every note in this vault"));
    sidebar.append(&all_notes);
    let recently_opened = sidebar_button("Recently Opened", "document-open-recent-symbolic");
    recently_opened.set_tooltip_text(Some("Notes you recently opened"));
    sidebar.append(&recently_opened);
    let favourites = sidebar_button("Favourites", "starred-symbolic");
    favourites.set_tooltip_text(Some("Notes you marked as a favourite"));
    sidebar.append(&favourites);
    let pinned = sidebar_button("Pinned", "view-pin-symbolic");
    pinned.set_tooltip_text(Some("Pinned notes"));
    sidebar.append(&pinned);
    let archive = sidebar_button("Archive", "folder-symbolic");
    archive.set_tooltip_text(Some("Archived notes"));
    sidebar.append(&archive);

    // "Secured Vaults" lists the user's actual Secure Vaults (whole-vault
    // encrypted). Clicking one switches to it. It is *not* a smart view of
    // individually encrypted notes.
    let secured_heading = gtk::Label::new(Some("SECURED VAULTS"));
    secured_heading.set_xalign(0.0);
    secured_heading.set_margin_top(10);
    secured_heading.add_css_class("sidebar-section-title");
    sidebar.append(&secured_heading);
    let secure_vaults_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
    sidebar.append(&secure_vaults_box);
    let new_secure_vault = sidebar_button("New Secure Vault", "list-add-symbolic");
    new_secure_vault.set_tooltip_text(Some(
        "Create a new Secure Vault (everything inside is encrypted and protected by a vault password)",
    ));
    sidebar.append(&new_secure_vault);

    let notebooks_header = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    notebooks_header.set_margin_top(10);
    let notebooks_heading = gtk::Label::new(Some("NOTEBOOKS"));
    notebooks_heading.set_xalign(0.0);
    notebooks_heading.set_hexpand(true);
    notebooks_heading.add_css_class("sidebar-section-title");
    let new_notebook = gtk::Button::from_icon_name("list-add-symbolic");
    new_notebook.add_css_class("flat");
    new_notebook.set_tooltip_text(Some("New notebook"));
    new_notebook.update_property(&[gtk::accessible::Property::Label("New notebook")]);
    notebooks_header.append(&notebooks_heading);
    notebooks_header.append(&new_notebook);
    sidebar.append(&notebooks_header);

    // Displayed as "Unfiled" - the on-disk notebook directory stays named
    // "Inbox" for backward compatibility with existing vaults; only the
    // UI-facing label changes. See `ViewMode::heading`.
    let inbox = sidebar_button("Unfiled", "mail-inbox-symbolic");
    inbox.set_tooltip_text(Some("Show notes not filed into another notebook"));
    sidebar.append(&inbox);

    let notebook_list = gtk::ListBox::new();
    notebook_list.set_selection_mode(gtk::SelectionMode::Single);
    notebook_list.add_css_class("notebook-list");
    let notebook_menu = gtk::PopoverMenu::from_model(None::<&gio::Menu>);
    notebook_menu.set_has_arrow(false);
    notebook_menu.set_halign(gtk::Align::Start);
    notebook_menu.set_parent(&notebook_list);
    sidebar.append(&notebook_list);

    let tags_heading = gtk::Label::new(Some("TAGS"));
    tags_heading.set_xalign(0.0);
    tags_heading.set_margin_top(10);
    tags_heading.add_css_class("sidebar-section-title");
    sidebar.append(&tags_heading);
    let tags_flow = gtk::FlowBox::new();
    tags_flow.set_selection_mode(gtk::SelectionMode::None);
    tags_flow.set_max_children_per_line(4);
    tags_flow.set_row_spacing(4);
    tags_flow.set_column_spacing(4);
    sidebar.append(&tags_flow);

    let trash = sidebar_button("Trash", "user-trash-symbolic");
    trash.set_margin_top(10);
    trash.set_tooltip_text(Some("Show deleted notes"));
    sidebar.append(&trash);

    let privacy_badge = gtk::Label::new(Some("Offline by design"));
    privacy_badge.set_valign(gtk::Align::End);
    privacy_badge.set_vexpand(true);
    privacy_badge.set_xalign(0.0);
    privacy_badge.add_css_class("caption");
    privacy_badge.add_css_class("dim-label");
    sidebar.append(&privacy_badge);
    // The sidebar now holds a variable-height notebook/tag list, unlike the
    // three fixed buttons of v0.1, so it must scroll rather than impose its
    // natural height as a hard minimum - the same minimum-window discipline
    // every other page in this window already follows (see the window
    // size_request comment above).
    let sidebar_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&sidebar)
        .build();
    library_split.set_sidebar(Some(&sidebar_scroll));

    let content_split = adw::NavigationSplitView::new();
    content_split.set_vexpand(true);
    content_split.set_sidebar_width_unit(adw::LengthUnit::Px);
    content_split.set_min_sidebar_width(280.0);
    content_split.set_max_sidebar_width(360.0);
    content_split.set_sidebar_width_fraction(0.34);

    let notes_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    notes_box.add_css_class("notes-column");
    notes_box.set_size_request(280, -1);
    let notes_header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    notes_header.set_margin_start(16);
    notes_header.set_margin_end(12);
    notes_header.set_margin_top(16);
    notes_header.set_margin_bottom(10);
    let notes_heading = gtk::Label::new(Some("All Notes"));
    notes_heading.set_xalign(0.0);
    notes_heading.set_hexpand(true);
    notes_heading.add_css_class("title-3");
    let empty_trash_button = gtk::Button::with_label("Empty");
    empty_trash_button.add_css_class("flat");
    empty_trash_button.add_css_class("destructive-action");
    empty_trash_button.set_visible(false);
    empty_trash_button.set_tooltip_text(Some("Permanently delete every note in Trash"));
    let sort_menu = gio::Menu::new();
    for (label, target) in [
        ("Last Edited", "last-edited"),
        ("Date Created", "date-created"),
        ("Title A–Z", "title-asc"),
        ("Title Z–A", "title-za"),
    ] {
        let item = gio::MenuItem::new(Some(label), None);
        item.set_action_and_target_value(Some("app.set-sort-order"), Some(&target.to_variant()));
        sort_menu.append_item(&item);
    }
    let sort_button = gtk::MenuButton::builder()
        .icon_name("view-sort-descending-symbolic")
        .menu_model(&sort_menu)
        .tooltip_text("Sort notes")
        .build();
    sort_button.add_css_class("flat");
    sort_button.update_property(&[gtk::accessible::Property::Label("Sort notes")]);
    notes_header.append(&notes_heading);
    notes_header.append(&sort_button);
    notes_header.append(&empty_trash_button);
    notes_box.append(&notes_header);
    let note_list = gtk::ListBox::new();
    note_list.set_selection_mode(gtk::SelectionMode::Single);
    note_list.add_css_class("note-list");
    let row_menu = gtk::PopoverMenu::from_model(None::<&gio::Menu>);
    row_menu.set_has_arrow(false);
    row_menu.set_halign(gtk::Align::Start);
    row_menu.set_parent(&note_list);
    let notes_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&note_list)
        .build();
    let note_list_stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
        .vexpand(true)
        .vhomogeneous(false)
        .build();
    note_list_stack.add_named(&notes_scroll, Some("list"));
    let note_list_empty = gtk::Box::new(gtk::Orientation::Vertical, 8);
    note_list_empty.set_halign(gtk::Align::Center);
    note_list_empty.set_valign(gtk::Align::Center);
    note_list_empty.set_margin_start(24);
    note_list_empty.set_margin_end(24);
    let empty_icon = gtk::Image::from_icon_name("document-new-symbolic");
    empty_icon.set_pixel_size(36);
    empty_icon.add_css_class("dim-label");
    note_list_empty.append(&empty_icon);
    let note_list_empty_title = gtk::Label::new(Some("No notes here"));
    note_list_empty_title.add_css_class("heading");
    note_list_empty.append(&note_list_empty_title);
    let note_list_empty_copy = gtk::Label::new(Some("Create a new note to start writing."));
    note_list_empty_copy.set_wrap(true);
    note_list_empty_copy.set_justify(gtk::Justification::Center);
    note_list_empty_copy.add_css_class("caption");
    note_list_empty_copy.add_css_class("dim-label");
    note_list_empty.append(&note_list_empty_copy);
    note_list_stack.add_named(&scroll_center(&note_list_empty), Some("empty"));
    notes_box.append(&note_list_stack);
    let notes_page = adw::NavigationPage::new(&notes_box, "Notes");
    content_split.set_sidebar(Some(&notes_page));

    let document_stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
        .vhomogeneous(false)
        .hhomogeneous(false)
        .build();
    let editor_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    editor_box.add_css_class("editor-pane");
    let title_row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    title_row.add_css_class("title-area");
    title_row.set_margin_start(16);
    title_row.set_margin_end(16);
    title_row.set_margin_top(18);
    title_row.set_margin_bottom(8);
    let title = gtk::Entry::builder()
        .placeholder_text("Note title")
        .hexpand(true)
        .build();
    title.add_css_class("note-title");
    title.update_property(&[gtk::accessible::Property::Label("Note title")]);
    let save_status = gtk::Label::new(Some("Saved"));
    save_status.add_css_class("save-status");
    save_status.add_css_class("dim-label");
    // A long "Save failed: …" message must not widen the title row (and the
    // window) past its minimum; it ellipsizes instead.
    save_status.set_ellipsize(gtk::pango::EllipsizeMode::End);
    save_status.set_max_width_chars(18);
    save_status.update_property(&[gtk::accessible::Property::Label("Save status")]);

    // Note-header quick actions: My Note [lock] [favourite] [pin] [overflow].
    // The lock button is note-level only and never locks the whole vault.
    let note_lock_button = gtk::Button::from_icon_name("changes-allow-symbolic");
    note_lock_button.add_css_class("flat");
    note_lock_button.set_valign(gtk::Align::Center);
    note_lock_button.set_tooltip_text(Some("Encrypt Note"));
    note_lock_button.update_property(&[gtk::accessible::Property::Label(
        "Encrypt, lock or unlock this note",
    )]);
    let note_favourite_button = gtk::Button::from_icon_name("non-starred-symbolic");
    note_favourite_button.add_css_class("flat");
    note_favourite_button.set_valign(gtk::Align::Center);
    note_favourite_button.set_tooltip_text(Some("Add to Favourites"));
    note_favourite_button.update_property(&[gtk::accessible::Property::Label(
        "Add to or remove from Favourites",
    )]);
    let note_pin_button = gtk::Button::from_icon_name("view-pin-symbolic");
    note_pin_button.add_css_class("flat");
    note_pin_button.set_valign(gtk::Align::Center);
    note_pin_button.set_tooltip_text(Some("Pin Note"));
    note_pin_button.update_property(&[gtk::accessible::Property::Label("Pin or unpin this note")]);
    let note_overflow_button = gtk::MenuButton::builder()
        .icon_name("view-more-symbolic")
        .tooltip_text("More note actions")
        .build();
    note_overflow_button.add_css_class("flat");
    note_overflow_button.set_valign(gtk::Align::Center);
    note_overflow_button.update_property(&[gtk::accessible::Property::Label("More note actions")]);

    title_row.append(&title);
    title_row.append(&save_status);
    title_row.append(&note_lock_button);
    title_row.append(&note_favourite_button);
    title_row.append(&note_pin_button);
    title_row.append(&note_overflow_button);
    editor_box.append(&title_row);

    let tags_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    tags_row.add_css_class("tags-row");
    tags_row.set_margin_start(16);
    tags_row.set_margin_end(16);
    tags_row.set_margin_bottom(10);
    tags_row.set_visible(false);
    let tag_chips = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    tag_chips.set_hexpand(true);
    let tag_add_entry = gtk::Entry::builder()
        .placeholder_text("Add tag…")
        .max_width_chars(14)
        .build();
    tag_add_entry.add_css_class("tag-add-entry");
    tag_add_entry.update_property(&[gtk::accessible::Property::Label("Add tag")]);
    tags_row.append(&tag_chips);
    tags_row.append(&tag_add_entry);
    editor_box.append(&tags_row);

    let (formatting_bar, format_bold_button, format_italic_button) = build_formatting_bar();
    editor_box.append(&formatting_bar);
    let buffer = sourceview5::Buffer::new(None::<&gtk::TextTagTable>);
    // Editor V2's own markdown_spans-driven tags (registered below) are now
    // the single, deliberate, tested source of Markdown visual styling.
    // GtkSourceView's built-in language-based syntax highlighting is left
    // disabled: two independent systems assigning Pango attributes (weight,
    // style) to the same text is exactly the kind of interaction that is
    // very hard to reason about precisely, and a real-machine acceptance
    // pass found Ctrl+B visually producing bold+italic instead of bold-only
    // - most plausibly this stacking, since Editor V2's own span computation
    // was verified in isolation (and by a dedicated pipeline test) to
    // produce Bold only for that exact input. `set_language` is still set
    // for GtkSourceView's other language-aware behaviour (e.g. matching
    // bracket detection), just not its highlighting.
    if let Some(language) = sourceview5::LanguageManager::default().language("markdown") {
        buffer.set_language(Some(&language));
    }
    buffer.set_highlight_syntax(false);
    buffer.set_highlight_matching_brackets(true);
    register_markdown_style_tags(&buffer);
    let editor = sourceview5::View::with_buffer(&buffer);
    editor.add_css_class("editor-view");
    editor.set_wrap_mode(gtk::WrapMode::WordChar);
    editor.set_show_line_numbers(false);
    editor.set_highlight_current_line(false);
    editor.set_top_margin(14);
    editor.set_bottom_margin(32);
    editor.set_left_margin(30);
    editor.set_right_margin(30);
    editor.set_vexpand(true);
    editor
        .upcast_ref::<gtk::Widget>()
        .update_property(&[gtk::accessible::Property::Label("Markdown note editor")]);
    let editor_scroll = gtk::ScrolledWindow::builder()
        .hexpand(true)
        .vexpand(true)
        .child(&editor)
        .build();
    editor_box.append(&editor_scroll);
    document_stack.add_named(&editor_box, Some("editor"));

    let locked_page = centered_status_page(
        "changes-prevent-symbolic",
        "Locked Note",
        "This note is encrypted on disk. Enter its password to decrypt it in memory.",
    );
    let locked_copy = locked_page
        .last_child()
        .and_downcast::<gtk::Label>()
        .unwrap_or_else(|| gtk::Label::new(None));
    let unlock = gtk::Button::with_label("Unlock Note");
    unlock.add_css_class("suggested-action");
    unlock.set_halign(gtk::Align::Center);
    locked_page.append(&unlock);
    document_stack.add_named(&scroll_center(&locked_page), Some("locked"));

    // Whole-vault lock screen. It lives inside the *content* area so the
    // application shell - header, vault identity, vault switcher, sidebar -
    // stays visible and usable while a Secure Vault is locked. The user can
    // switch to another vault (including Main) without entering this vault's
    // password; only this vault's decrypted content is withheld.
    let vault_locked_page = centered_status_page(
        "channel-secure-symbolic",
        "Secure Vault Locked",
        "Everything in this vault is encrypted and protected by your Vault Password.",
    );
    let vault_unlock_button = gtk::Button::with_label("Unlock Vault");
    vault_unlock_button.add_css_class("suggested-action");
    vault_unlock_button.add_css_class("pill");
    vault_unlock_button.set_halign(gtk::Align::Center);
    vault_locked_page.append(&vault_unlock_button);
    let vault_locked_status = gtk::Label::new(None);
    vault_locked_status.set_wrap(true);
    vault_locked_status.set_max_width_chars(54);
    vault_locked_status.set_justify(gtk::Justification::Center);
    vault_locked_status.add_css_class("dim-label");
    vault_locked_page.append(&vault_locked_status);
    document_stack.add_named(&scroll_center(&vault_locked_page), Some("vault-locked"));

    let empty_page = centered_status_page(
        "document-new-symbolic",
        "No note selected",
        "Create a note or choose one from the list.",
    );
    document_stack.add_named(&scroll_center(&empty_page), Some("empty"));

    let trash_page = gtk::Box::new(gtk::Orientation::Vertical, 14);
    trash_page.set_halign(gtk::Align::Center);
    trash_page.set_valign(gtk::Align::Center);
    trash_page.set_margin_start(32);
    trash_page.set_margin_end(32);
    let trash_icon = gtk::Image::from_icon_name("user-trash-symbolic");
    trash_icon.set_pixel_size(48);
    trash_page.append(&trash_icon);
    let trash_detail_title = gtk::Label::new(Some("Trashed Note"));
    trash_detail_title.add_css_class("title-2");
    trash_page.append(&trash_detail_title);
    let trash_copy = gtk::Label::new(Some(
        "Restore this note to its original notebook, or delete it permanently.",
    ));
    trash_copy.set_wrap(true);
    trash_copy.set_justify(gtk::Justification::Center);
    trash_copy.add_css_class("dim-label");
    trash_page.append(&trash_copy);
    let trash_actions = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    trash_actions.set_halign(gtk::Align::Center);
    let restore = gtk::Button::with_label("Restore");
    restore.add_css_class("suggested-action");
    let permanently_delete = gtk::Button::with_label("Permanently Delete");
    permanently_delete.add_css_class("destructive-action");
    trash_actions.append(&restore);
    trash_actions.append(&permanently_delete);
    trash_page.append(&trash_actions);
    document_stack.add_named(&scroll_center(&trash_page), Some("trash-detail"));
    document_stack.set_visible_child_name("empty");
    let editor_page = adw::NavigationPage::new(&document_stack, "Editor");
    content_split.set_content(Some(&editor_page));
    library_split.set_content(Some(&content_split));

    workspace.append(&library_split);
    stack.add_named(&workspace, Some("workspace"));

    window.set_content(Some(&stack));

    // AdwBreakpoints on a window are mutually exclusive: only the best-matching
    // one applies its setters, so each must be a *complete* configuration for
    // its width range. The narrow (<=760) breakpoint therefore also collapses
    // the library that the medium (<=1050) breakpoint collapses - otherwise
    // below 760 the library stays expanded and its sidebar width is added back
    // into the window minimum, which is what produced the fatal
    // `AdwApplicationWindow` width warning.
    let library_breakpoint = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
        adw::BreakpointConditionLengthType::MaxWidth,
        1050.0,
        adw::LengthUnit::Px,
    ));
    library_breakpoint.add_setters(&[(&library_split, "collapsed", true)]);
    library_breakpoint.add_setters(&[(&library_toggle, "visible", true)]);
    window.add_breakpoint(library_breakpoint);

    let content_breakpoint = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
        adw::BreakpointConditionLengthType::MaxWidth,
        760.0,
        adw::LengthUnit::Px,
    ));
    content_breakpoint.add_setters(&[(&library_split, "collapsed", true)]);
    content_breakpoint.add_setters(&[(&content_split, "collapsed", true)]);
    content_breakpoint.add_setters(&[(&library_toggle, "visible", true)]);
    content_breakpoint.add_setters(&[(&back_to_notes, "visible", true)]);
    window.add_breakpoint(content_breakpoint);

    let appearance_provider = gtk::CssProvider::new();
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &appearance_provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
        );
    }

    (
        Widgets {
            window,
            stack,
            welcome_status,
            vault_label,
            sessions: Rc::new(SessionRegistry::default()),
            vault_popover,
            vault_popover_name,
            vault_popover_path,
            vault_popover_status,
            vault_recent_box,
            vault_popover_secure_actions,
            vault_readonly_icon,
            vault_state_icon,
            vault_lock_button,
            welcome_recent_box,
            welcome_recent_heading,
            first_run_panel,
            new_note: new_note.clone(),
            new_notebook: new_notebook.clone(),
            delete_button,
            notes_heading,
            search,
            note_list,
            row_menu,
            note_list_stack,
            note_list_empty_title,
            note_list_empty_copy,
            row_widgets: Rc::new(RefCell::new(HashMap::new())),
            selection: Rc::new(SelectionCoordinator::default()),
            pending_select: Rc::new(Cell::new(None)),
            select_source: Rc::new(RefCell::new(None)),
            editor_events: Rc::new(SignalGate::default()),
            library_split,
            content_split,
            all_notes_button: all_notes.clone(),
            inbox_button: inbox.clone(),
            pinned_button: pinned.clone(),
            recently_opened_button: recently_opened.clone(),
            favourites_button: favourites.clone(),
            archive_button: archive.clone(),
            trash_button: trash.clone(),
            secure_vaults_box,
            notebook_list,
            notebook_menu,
            notebook_rows: Rc::new(RefCell::new(Vec::new())),
            notebook_events: Rc::new(SignalGate::default()),
            tags_flow,
            tags_events: Rc::new(SignalGate::default()),
            document_stack,
            title,
            tags_row,
            tag_chips,
            tag_add_entry,
            buffer,
            editor,
            style_recompute_source: Rc::new(RefCell::new(None)),
            formatting_bar,
            format_bold_button,
            format_italic_button,
            format_toolbar_update_source: Rc::new(RefCell::new(None)),
            format_toolbar_state: Rc::new(Cell::new(ActiveFormats::default())),
            save_status,
            note_lock_button,
            note_favourite_button,
            note_pin_button,
            note_overflow_button,
            locked_copy,
            vault_locked_status,
            vault_unlock_button,
            trash_detail_title,
            empty_trash_button,
            appearance_provider,
        },
        Controls {
            create_vault,
            create_encrypted_vault,
            open_vault,
            first_run_start,
            first_run_secure,
            new_note,
            new_notebook,
            all_notes,
            inbox,
            pinned,
            recently_opened,
            favourites,
            archive,
            trash,
            new_secure_vault,
            library_toggle,
            back_to_notes,
            unlock,
            restore,
            permanently_delete,
        },
    )
}

fn application_menu() -> gio::Menu {
    let menu = gio::Menu::new();
    let vaults = gio::Menu::new();
    vaults.append(Some("Open Vault…"), Some("app.open-vault"));
    vaults.append(Some("Create Vault…"), Some("app.create-vault"));
    vaults.append(
        Some("Create Secure Vault…"),
        Some("app.create-encrypted-vault"),
    );
    vaults.append(Some("Vault Settings…"), Some("app.vault-settings"));
    menu.append_section(None, &vaults);

    // Secure Vault (whole-vault) actions. The actions are disabled for a
    // Standard Vault, so "Lock Vault" cannot be invoked where there is no
    // vault-level key.
    let vault_security = gio::Menu::new();
    vault_security.append(Some("Lock Vault"), Some("app.lock-vault"));
    vault_security.append(
        Some("Change Vault Password…"),
        Some("app.change-vault-password"),
    );
    vault_security.append(
        Some("Export to Standard Vault…"),
        Some("app.export-standard-vault"),
    );
    menu.append_section(Some("Secure Vault"), &vault_security);

    menu.append(Some("Preferences"), Some("app.preferences"));
    let security = gio::Menu::new();
    security.append(Some("Encrypt Note…"), Some("app.encrypt-note"));
    security.append(Some("Lock Note"), Some("app.lock-note"));
    security.append(Some("Change Note Password…"), Some("app.change-password"));
    security.append(
        Some("Remove Note Encryption…"),
        Some("app.remove-encryption"),
    );
    menu.append_section(Some("Encrypted Note"), &security);
    let note = gio::Menu::new();
    note.append(Some("Note Information"), Some("app.note-info"));
    note.append(Some("Move to Trash"), Some("app.move-to-trash"));
    menu.append_section(Some("Note"), &note);
    menu.append(Some("About SenatorialNotes"), Some("app.about"));
    menu.append(Some("Quit"), Some("app.quit"));
    menu
}

fn build_formatting_bar() -> (gtk::ScrolledWindow, gtk::Button, gtk::Button) {
    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    bar.set_margin_start(12);
    bar.set_margin_end(12);
    // The toolbar scrolls horizontally rather than forcing the editor pane (and
    // thus the window) wider than its buttons on a narrow layout. The styling
    // (and the full-width bottom border) lives on the scroller so it still spans
    // the pane when the buttons do not.
    let bar_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .vscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .child(&bar)
        .build();
    bar_scroll.add_css_class("formatting-bar");
    bar_scroll.set_margin_bottom(8);
    let style_menu = gio::Menu::new();
    let paragraphs = gio::Menu::new();
    for (label, action) in [
        ("Normal text", "app.format-normal"),
        ("Heading 1", "app.format-heading-1"),
        ("Heading 2", "app.format-heading-2"),
        ("Heading 3", "app.format-heading-3"),
    ] {
        paragraphs.append(Some(label), Some(action));
    }
    style_menu.append_section(Some("Paragraph"), &paragraphs);
    let styles = gtk::MenuButton::builder()
        .label("Style")
        .menu_model(&style_menu)
        .tooltip_text("Paragraph style")
        .build();
    bar.append(&styles);

    fn format_button(bar: &gtk::Box, label: &str, tooltip: &str, action: &str) -> gtk::Button {
        let button = gtk::Button::with_label(label);
        button.add_css_class("flat");
        button.add_css_class("format-button");
        button.set_tooltip_text(Some(tooltip));
        button.set_action_name(Some(action));
        button.update_property(&[gtk::accessible::Property::Label(tooltip)]);
        bar.append(&button);
        button
    }
    let bold_button = format_button(&bar, "B", "Bold (Ctrl+B)", "app.format-bold");
    let italic_button = format_button(&bar, "I", "Italic (Ctrl+I)", "app.format-italic");

    let more_menu = gio::Menu::new();
    let inline = gio::Menu::new();
    for (label, action) in [
        ("Strikethrough", "app.format-strikethrough"),
        ("Highlight", "app.format-highlight"),
        ("Inline code", "app.format-inline-code"),
        ("Link", "app.format-link"),
    ] {
        inline.append(Some(label), Some(action));
    }
    more_menu.append_section(Some("Inline"), &inline);
    let blocks = gio::Menu::new();
    for (label, action) in [
        ("Code block", "app.format-code-block"),
        ("Quote", "app.format-quote"),
        ("Bulleted list", "app.format-bulleted-list"),
        ("Numbered list", "app.format-numbered-list"),
        ("Checklist", "app.format-checklist"),
        ("Horizontal divider", "app.format-divider"),
    ] {
        blocks.append(Some(label), Some(action));
    }
    more_menu.append_section(Some("Block"), &blocks);
    let more = gtk::MenuButton::builder()
        .label("More")
        .menu_model(&more_menu)
        .tooltip_text("More Markdown formatting")
        .build();
    bar.append(&more);
    (bar_scroll, bold_button, italic_button)
}

fn sidebar_button(label: &str, icon: &str) -> gtk::Button {
    let button = labeled_icon_button(label, icon);
    button.set_halign(gtk::Align::Fill);
    button.add_css_class("sidebar-item");
    button
}

fn labeled_icon_button(label: &str, icon: &str) -> gtk::Button {
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 7);
    let image = gtk::Image::from_icon_name(icon);
    image.set_pixel_size(16);
    content.append(&image);
    let text = gtk::Label::new(Some(label));
    text.set_xalign(0.0);
    content.append(&text);
    let button = gtk::Button::new();
    button.set_child(Some(&content));
    button.update_property(&[gtk::accessible::Property::Label(label)]);
    button
}

/// Wraps a page so it scrolls instead of forcing the whole window taller than
/// its own minimum. The inner box keeps the page vertically centred at normal
/// sizes and lets it scroll once the viewport is shorter than the content.
fn scroll_center(child: &impl IsA<gtk::Widget>) -> gtk::ScrolledWindow {
    let centerer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    centerer.set_valign(gtk::Align::Center);
    centerer.set_halign(gtk::Align::Center);
    centerer.set_vexpand(true);
    centerer.append(child);
    // Both scrollbars are automatic: the page must never impose its natural
    // width (or height) on the window as a hard minimum. It scrolls instead.
    gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .propagate_natural_width(false)
        .hexpand(true)
        .vexpand(true)
        .child(&centerer)
        .build()
}

fn centered_status_page(icon: &str, heading: &str, body: &str) -> gtk::Box {
    let page = gtk::Box::new(gtk::Orientation::Vertical, 12);
    page.set_halign(gtk::Align::Center);
    page.set_valign(gtk::Align::Center);
    page.set_margin_start(32);
    page.set_margin_end(32);
    let image = gtk::Image::from_icon_name(icon);
    image.set_pixel_size(52);
    image.add_css_class("dim-label");
    page.append(&image);
    let title = gtk::Label::new(Some(heading));
    title.add_css_class("title-2");
    page.append(&title);
    let copy = gtk::Label::new(Some(body));
    copy.set_wrap(true);
    copy.set_max_width_chars(54);
    copy.set_justify(gtk::Justification::Center);
    copy.add_css_class("dim-label");
    page.append(&copy);
    page
}

fn connect_folder_button(
    button: &gtk::Button,
    create: bool,
    state: Rc<RefCell<AppState>>,
    widgets: Widgets,
    pending: Rc<RefCell<PendingSaves>>,
) {
    button.connect_clicked(move |_| {
        present_vault_folder_picker(create, &state, &widgets, &pending);
    });
}

/// The native folder picker used by both welcome buttons and the
/// `app.open-vault` / `app.create-vault` actions. Cancelling it is a complete
/// no-op — the current vault/session is never touched until a folder is chosen
/// *and* `open_vault` has validated it.
fn present_vault_folder_picker(
    create: bool,
    state: &Rc<RefCell<AppState>>,
    widgets: &Widgets,
    pending: &Rc<RefCell<PendingSaves>>,
) {
    widgets.vault_popover.popdown();
    let dialog = gtk::FileDialog::builder()
        .title(if create {
            "Choose a Folder for the New Vault"
        } else {
            "Open a SenatorialNotes Vault"
        })
        .modal(true)
        .build();
    let state = state.clone();
    let widgets = widgets.clone();
    let pending = pending.clone();
    let parent = widgets.window.clone();
    dialog.select_folder(
        Some(&parent),
        None::<&gio::Cancellable>,
        move |result| match result {
            Ok(folder) => match folder.path() {
                Some(path) => open_vault(&path, create, &state, &widgets, &pending),
                None => report_vault_open_error(
                    &state,
                    &widgets,
                    "The selected folder is not a local path.",
                ),
            },
            Err(error) if !error.matches(gio::IOErrorEnum::Cancelled) => {
                report_vault_open_error(
                    &state,
                    &widgets,
                    &format!("Folder selection failed: {error}"),
                );
            }
            Err(_) => {}
        },
    );
}

/// Best-effort canonical path; falls back to the path as given.
fn canonical_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Shows a vault-open failure without disturbing the current vault: on the
/// welcome screen it uses the welcome status line, in the workspace it uses the
/// save-status line so the open vault stays fully usable.
fn report_vault_open_error(state: &Rc<RefCell<AppState>>, widgets: &Widgets, message: &str) {
    if state.borrow().vault.is_some() {
        widgets.save_status.set_label(message);
    } else {
        show_welcome_error(widgets, message);
    }
}

/// Opens (or, with `create`, creates) the vault at `path` and makes it the
/// active vault.
///
/// Ordering is deliberate so a failed switch never damages the current vault:
///
/// 1. **Validate** the target (`Vault::open`/`create`). On failure the current
///    vault/session is completely untouched.
/// 2. **Decide the lock**: `VaultLock::acquire`. `Free`/ours → writable.
///    A contended lock → a modal dialog whose outcome (read-only / take over /
///    cancel) drives the commit; failing to acquire never touches the current
///    session.
/// 3. In `commit_vault_switch`: flush the outgoing vault (abort on save
///    failure), **release the old writable lock only after that flush**, bump
///    the session generation, swap in the new vault + lock, rebuild, restore.
fn open_vault(
    path: &Path,
    create: bool,
    state: &Rc<RefCell<AppState>>,
    widgets: &Widgets,
    pending: &Rc<RefCell<PendingSaves>>,
) {
    // 1. Validate the target first. Nothing about the current session changes.
    let vault = match if create {
        Vault::create(path)
    } else {
        Vault::open(path)
    } {
        Ok(vault) => vault,
        Err(error) => {
            report_vault_open_error(
                state,
                widgets,
                &format!("Could not open the vault: {error}"),
            );
            return;
        }
    };

    // Re-selecting the vault that is already open: just close the popover.
    let already_current = {
        let state = state.borrow();
        state
            .vault
            .as_ref()
            .map(|current| canonical_path(current.root()) == canonical_path(vault.root()))
            .unwrap_or(false)
    };
    if already_current {
        widgets.vault_popover.popdown();
        return;
    }

    // 2. Decide the lock and commit. Still nothing about the current session
    // has changed until `commit_vault_switch` runs.
    proceed_with_opened_vault(vault, path.to_path_buf(), state, widgets, pending);
}

/// Acquires the advisory lock for an already-validated `vault` and commits the
/// switch (or shows the lock-contention dialog). Shared by opening an existing
/// vault and creating a new (ordinary or encrypted) one.
fn proceed_with_opened_vault(
    vault: Vault,
    path: PathBuf,
    state: &Rc<RefCell<AppState>>,
    widgets: &Widgets,
    pending: &Rc<RefCell<PendingSaves>>,
) {
    // R18: a Secure Vault whose root holds plaintext an old / incompatible
    // binary wrote. Detection has already run (in `Vault::create`); nothing has
    // been moved. Block a normal writable session and let the user decide.
    if vault.pending_quarantine().is_some() {
        present_plaintext_conflict_dialog(vault, path, state, widgets, pending);
        return;
    }
    finish_opening_vault(vault, path, state, widgets, pending);
}

/// The lock decision + commit, once any R18 plaintext conflict is resolved
/// (quarantined, or the user chose to open read-only anyway).
fn finish_opening_vault(
    vault: Vault,
    path: PathBuf,
    state: &Rc<RefCell<AppState>>,
    widgets: &Widgets,
    pending: &Rc<RefCell<PendingSaves>>,
) {
    match VaultLock::acquire(&vault) {
        Ok(LockAcquisition::Acquired(lock)) => {
            commit_vault_switch(vault, lock, path, state, widgets, pending);
        }
        Ok(LockAcquisition::Contended(_)) if vault.is_read_only() => {
            // We were opening read-only anyway - skip the dialog.
            commit_vault_switch(vault, VaultLock::read_only(), path, state, widgets, pending);
        }
        Ok(LockAcquisition::Contended(status)) => {
            present_lock_contention_dialog(vault, status, path, state, widgets, pending);
        }
        Err(error) => {
            report_vault_open_error(
                state,
                widgets,
                &format!("Could not lock the vault: {error}"),
            );
        }
    }
}

/// Commits a vault switch: the new `vault` is validated and its lock decided.
/// Releases the outgoing vault's writable lock only after the outgoing vault
/// has been safely flushed.
fn commit_vault_switch(
    vault: Vault,
    lock: VaultLock,
    _path: PathBuf,
    state: &Rc<RefCell<AppState>>,
    widgets: &Widgets,
    pending: &Rc<RefCell<PendingSaves>>,
) {
    // Flush the outgoing vault. On save failure, keep it (and its lock) and
    // release the lock we just acquired for the new one.
    let had_vault = { state.borrow().vault.is_some() };
    if had_vault {
        if !prepare_to_leave_active(state, widgets, pending) {
            drop(lock);
            widgets
                .save_status
                .set_label("The current note could not be saved - staying on this vault.");
            return;
        }
        persist_vault_session_state(state, widgets);
    }

    // Commit. The new vault is known-good and the old one is safely flushed.
    widgets.vault_popover.popdown();
    // Release the OLD writable lock now (not before the flush).
    {
        state.borrow_mut().vault_lock.take();
    }
    clear_sensitive_documents(state);
    cancel_all_timers(pending);
    cancel_pending_selection(widgets);
    cancel_editor_deferrals(widgets);

    let read_only = vault.is_read_only() || !lock.is_owner();
    let migration_warning = vault.migration().warning();

    let watcher = VaultWatcher::new(vault.root());
    let watcher_error = watcher.as_ref().err().map(ToString::to_string);
    let watcher = watcher.ok();

    let config_save_error = {
        let mut state = state.borrow_mut();
        state.config.record_vault_open(vault.root(), vault.kind());
        state.watcher = watcher;
        state.vault = Some(vault);
        state.vault_lock = Some(lock);
        state.read_only = read_only;
        state.notes.clear();
        state.trash.clear();
        state.body_dirty = false;
        state.title_dirty = false;
        state.title_draft.clear();
        state.flow = UiFlow::default();
        state.filter = FilterState::default();
        state.config.save().err().map(|error| error.to_string())
    };
    // Bump the session generation *outside* the borrow above; the registry
    // lives on `Widgets`, not in `AppState`.
    widgets.sessions.bump();

    // An encrypted vault opens locked: show the unlock screen instead of the
    // workspace. No note list, editor, or search state is built until a
    // successful unlock (see `begin_vault_unlock` → `enter_vault_workspace`).
    let encrypted_locked = {
        let state = state.borrow();
        state
            .vault
            .as_ref()
            .is_some_and(|vault| vault.is_encrypted() && vault.is_locked())
    };
    if encrypted_locked {
        show_vault_locked_screen(state, widgets, pending, None);
        return;
    }

    // Status line: read-only note first, then any softer warnings.
    if read_only {
        widgets.save_status.set_label(
            migration_warning
                .as_deref()
                .unwrap_or("This vault is open read-only."),
        );
    } else if let Some(error) = watcher_error {
        widgets
            .save_status
            .set_label(&format!("Vault opened without live updates: {error}"));
    } else if let Some(error) = config_save_error {
        widgets
            .save_status
            .set_label(&format!("Vault opened; settings were not saved: {error}"));
    } else {
        widgets.save_status.set_label("Saved");
    }

    enter_vault_workspace(state, widgets, pending);
}

/// Builds the workspace UI for the now-open (and, for an encrypted vault,
/// unlocked) vault: header, view chrome, note list, notebooks, tags, and the
/// restored selection. Shared by a plain vault switch (`commit_vault_switch`)
/// and a successful encrypted-vault unlock (`begin_vault_unlock`).
fn enter_vault_workspace(
    state: &Rc<RefCell<AppState>>,
    widgets: &Widgets,
    pending: &Rc<RefCell<PendingSaves>>,
) {
    // Pull the working session copy from the right store: the plaintext config
    // for a Standard Vault, the sealed manifest for an unlocked Secure Vault.
    load_vault_session_state(state);
    let (read_only, restore, display_name) = {
        let state = state.borrow();
        let Some(vault) = state.vault.as_ref() else {
            return;
        };
        let path = vault.root().to_path_buf();
        (
            state.read_only,
            Some(state.session.clone()),
            vault_display_name_for(&state.config, &path),
        )
    };

    widgets.vault_label.set_label(&display_name);
    widgets.stack.set_visible_child_name("workspace");
    if widgets.library_split.is_collapsed() {
        widgets.library_split.set_show_sidebar(false);
    }
    widgets.content_split.set_show_content(false);
    apply_read_only_ui(widgets, read_only);
    // Coming out of a possibly-locked state: re-enable search and note actions
    // (read-only still narrows them further via `apply_read_only_ui`).
    widgets.search.set_sensitive(true);
    set_note_actions_enabled(widgets, !read_only);
    update_vault_lock_controls(state, widgets);
    render_vault_switcher(state, widgets, pending);

    // Restore the saved view (falling back to All Notes if it is gone).
    let view = restore
        .as_ref()
        .and_then(|session| session.last_view.as_deref())
        .and_then(parse_view_token)
        .map(|view| resolve_restored_view(state, view))
        .unwrap_or(ViewMode::AllNotes);
    {
        state.borrow_mut().flow.switch_view(view.clone());
    }
    apply_view_chrome(&view, widgets);
    if !refresh_current_view(state, widgets) {
        return;
    }
    render_notebook_list(state, widgets);
    render_tags_list(state, widgets);

    // Restore the saved note if it still exists, else fall back safely.
    let restored_note = restore
        .as_ref()
        .and_then(|session| session.last_note)
        .filter(|id| state.borrow().notes.iter().any(|note| note.id == *id));
    let is_empty = { state.borrow().notes.is_empty() };
    if let Some(id) = restored_note {
        {
            state.borrow_mut().flow.select_note(id);
        }
        select_row_target(RowTarget::Note(id), widgets);
        select_note_without_prompting_if_locked(id, state, widgets);
    } else if is_empty && !read_only {
        create_new_note(state, widgets, pending);
    } else {
        select_first_row(state, widgets);
    }

    if let Some(offset) = restore.as_ref().and_then(|session| session.editor_scroll) {
        restore_editor_scroll(widgets, offset);
    }

    update_note_quick_actions(state, widgets);

    // An unlocked encrypted vault holds decrypted previews in memory, so start
    // the idle-lock clock even before the first edit.
    {
        let mut state = state.borrow_mut();
        touch_sensitive_activity(&mut state);
    }

    refresh_watch_baseline(state);
}

/// Updates the lock card's status line and re-arms its "Unlock Vault" button
/// without disturbing the rest of the (already-primed) locked-vault workspace.
/// Use this for follow-up messages such as a failed unlock attempt.
fn set_vault_locked_message(widgets: &Widgets, message: Option<&str>) {
    widgets.vault_unlock_button.set_sensitive(true);
    widgets
        .vault_locked_status
        .set_label(message.unwrap_or_default());
    widgets.vault_locked_status.set_visible(message.is_some());
    widgets.content_split.set_show_content(true);
    widgets
        .document_stack
        .set_visible_child_name("vault-locked");
}

/// Primes and shows the full locked-vault workspace. The application shell -
/// header, vault identity, vault switcher and sidebar - stays visible and
/// usable so the user can switch to another vault (including Main) *without*
/// this vault's Vault Password. Only this vault's decrypted content is
/// withheld: no note titles, notebooks, tags, search results or session state.
fn show_vault_locked_screen(
    state: &Rc<RefCell<AppState>>,
    widgets: &Widgets,
    pending: &Rc<RefCell<PendingSaves>>,
    message: Option<&str>,
) {
    let display_name = {
        let state = state.borrow();
        state
            .vault
            .as_ref()
            .map(|vault| vault_display_name_for(&state.config, vault.root()))
            .unwrap_or_else(|| APP_NAME.to_string())
    };
    widgets.vault_label.set_label(&display_name);
    widgets.stack.set_visible_child_name("workspace");

    // Nothing decrypted may survive into the locked view.
    {
        let _guard = widgets.editor_events.suppress();
        widgets.title.set_text("");
        set_buffer_text_silently(&widgets.buffer, "");
    }
    widgets.search.set_text("");
    widgets.search.set_sensitive(false);

    // `state.notes` / `state.trash` are already empty for a locked vault, so
    // these render as empty lists - never this vault's real navigation data.
    {
        let mut state = state.borrow_mut();
        state.flow.switch_view(ViewMode::AllNotes);
        state.session = VaultSessionState::default();
    }
    apply_view_chrome(&ViewMode::AllNotes, widgets);
    render_note_list(state, widgets);
    // `list_notebooks` / tag scanning fail on a locked vault and would leave
    // the *previous* vault's rows on screen - clear them outright instead.
    clear_locked_vault_navigation(widgets);
    render_vault_switcher(state, widgets, pending);
    update_vault_lock_controls(state, widgets);
    set_note_actions_enabled(widgets, false);
    set_quick_actions_visible(widgets, false);

    if widgets.library_split.is_collapsed() {
        widgets.library_split.set_show_sidebar(false);
    }

    set_vault_locked_message(widgets, message);
}

/// Empties the notebook list and tag chips so a locked Secure Vault never shows
/// navigation data - neither its own (undecryptable) nor the previous vault's
/// (left behind because `list_notebooks` errors out on a locked vault).
fn clear_locked_vault_navigation(widgets: &Widgets) {
    let _guard = widgets.notebook_events.suppress();
    while let Some(row) = widgets.notebook_list.row_at_index(0) {
        widgets.notebook_list.remove(&row);
    }
    widgets.notebook_rows.borrow_mut().clear();
    drop(_guard);

    let _guard = widgets.tags_events.suppress();
    while let Some(child) = widgets.tags_flow.first_child() {
        widgets.tags_flow.remove(&child);
    }
}

/// Shows/hides the header vault lock/unlock affordances for the open vault:
/// the padlock glyph before the name, and the "Lock Vault" button (unlocked
/// Secure Vault only - a Standard Vault never exposes a vault lock).
fn update_vault_lock_controls(state: &Rc<RefCell<AppState>>, widgets: &Widgets) {
    let state = state.borrow();
    let (is_secure, is_locked) = state
        .vault
        .as_ref()
        .map(|vault| (vault.is_encrypted(), vault.is_locked()))
        .unwrap_or((false, false));
    widgets.vault_state_icon.set_visible(is_secure);
    if is_secure {
        widgets.vault_state_icon.set_icon_name(Some(if is_locked {
            "channel-secure-symbolic"
        } else {
            "changes-allow-symbolic"
        }));
        widgets
            .vault_state_icon
            .set_tooltip_text(Some(if is_locked {
                "This Secure Vault is locked"
            } else {
                "This Secure Vault is unlocked"
            }));
    }
    widgets
        .vault_lock_button
        .set_visible(is_secure && !is_locked);
}

/// Enables/disables the header actions that only make sense with an unlocked,
/// writable vault open (New Note, Delete). Used to blank them while a Secure
/// Vault is locked.
fn set_note_actions_enabled(widgets: &Widgets, enabled: bool) {
    widgets.new_note.set_sensitive(enabled);
    widgets.new_notebook.set_sensitive(enabled);
    widgets.delete_button.set_sensitive(enabled);
}

/// Prompts for the vault password and unlocks the open encrypted vault.
/// Argon2id key derivation runs on a worker thread (`gio::spawn_blocking`) so
/// the GTK main loop is never blocked. A failed unlock changes nothing.
fn begin_vault_unlock(
    state: &Rc<RefCell<AppState>>,
    widgets: &Widgets,
    pending: &Rc<RefCell<PendingSaves>>,
) {
    let (keyfile, vault_id) = {
        let state = state.borrow();
        let Some(vault) = state.vault.as_ref() else {
            return;
        };
        if !vault.is_encrypted() || !vault.is_locked() {
            return;
        }
        match vault.encrypted_keyfile() {
            Ok(bytes) => (bytes, vault.vault_id()),
            Err(error) => {
                set_vault_locked_message(
                    widgets,
                    Some(&format!(
                        "This Secure Vault's key file could not be read: {error}"
                    )),
                );
                return;
            }
        }
    };

    let state = state.clone();
    let widgets = widgets.clone();
    let pending = pending.clone();
    present_password_dialog(
        &widgets.window.clone(),
        "Unlock Vault",
        "Enter the vault password. Deriving the key can take a moment.",
        false,
        false,
        "Unlock Vault",
        move |maybe_password| {
            let Some(password) = maybe_password else {
                return;
            };
            let session = widgets.sessions.current();
            widgets.vault_unlock_button.set_sensitive(false);
            widgets.vault_locked_status.set_visible(true);
            widgets
                .vault_locked_status
                .set_label("Deriving the vault key…");

            let worker = gio::spawn_blocking(move || -> senatorial_notes::Result<VaultKeys> {
                vault_crypto::open_keyfile(&keyfile, vault_id, password.as_str())
            });

            let state = state.clone();
            let widgets = widgets.clone();
            let pending = pending.clone();
            glib::spawn_future_local(async move {
                let outcome = worker.await;
                if !widgets.sessions.is_current(session) {
                    return;
                }
                match outcome {
                    Ok(Ok(keys)) => {
                        let finished = {
                            let state = state.borrow();
                            state.vault.as_ref().map(|vault| vault.finish_unlock(keys))
                        };
                        match finished {
                            Some(Ok(())) => {
                                widgets.save_status.set_label("Saved");
                                enter_vault_workspace(&state, &widgets, &pending);
                            }
                            Some(Err(error)) => set_vault_locked_message(
                                &widgets,
                                Some(&format!("The vault could not be unlocked: {error}")),
                            ),
                            None => {}
                        }
                    }
                    Ok(Err(_)) => set_vault_locked_message(
                        &widgets,
                        Some(
                            "That password did not unlock the vault. The vault password may be \
                             wrong, or the vault's key file may be damaged.",
                        ),
                    ),
                    Err(_) => set_vault_locked_message(
                        &widgets,
                        Some("The key-derivation task failed unexpectedly."),
                    ),
                }
            });
        },
    );
}

/// Locks the open encrypted vault: flushes the active note, drops all
/// in-memory plaintext and key material, clears the note / search / editor
/// state, and shows the lock screen. A no-op for an ordinary vault or one that
/// is already locked. Returns whether it actually locked.
fn lock_vault(
    state: &Rc<RefCell<AppState>>,
    widgets: &Widgets,
    pending: &Rc<RefCell<PendingSaves>>,
) -> bool {
    let should_lock = {
        let state = state.borrow();
        state
            .vault
            .as_ref()
            .is_some_and(|vault| vault.is_encrypted() && !vault.is_locked())
    };
    if !should_lock {
        return false;
    }

    // Flush the open note first. If it will not save, stay unlocked.
    if !persist_active(state, widgets, true) {
        widgets
            .save_status
            .set_label("The current note could not be saved - the vault stays unlocked.");
        return false;
    }
    persist_vault_session_state(state, widgets);

    cancel_all_timers(pending);
    cancel_pending_selection(widgets);
    cancel_editor_deferrals(widgets);
    clear_sensitive_documents(state);

    {
        let mut state = state.borrow_mut();
        if let Some(vault) = state.vault.as_ref() {
            vault.lock();
        }
        state.notes.clear();
        state.trash.clear();
        state.body_dirty = false;
        state.title_dirty = false;
        state.title_draft.clear();
        state.flow = UiFlow::default();
        state.filter = FilterState::default();
    }
    // Any deferred callback armed against the unlocked session is now inert.
    widgets.sessions.bump();

    {
        let _guard = widgets.editor_events.suppress();
        widgets.title.set_text("");
        set_buffer_text_silently(&widgets.buffer, "");
    }
    widgets.search.set_text("");
    // The vault is no longer unlocked: disable its Secure-Vault menu actions
    // and show the locked-vault workspace (shell stays usable).
    render_vault_switcher(state, widgets, pending);
    show_vault_locked_screen(state, widgets, pending, None);
    true
}

/// Picks a folder and a password, then creates a new encrypted vault and opens
/// it (unlocked for this session). Key derivation runs off the GTK main thread.
fn present_encrypted_vault_creator(
    state: &Rc<RefCell<AppState>>,
    widgets: &Widgets,
    pending: &Rc<RefCell<PendingSaves>>,
) {
    widgets.vault_popover.popdown();
    let dialog = gtk::FileDialog::builder()
        .title("Choose a Folder for the New Secure Vault")
        .modal(true)
        .build();
    let state = state.clone();
    let widgets = widgets.clone();
    let pending = pending.clone();
    let parent = widgets.window.clone();
    dialog.select_folder(Some(&parent), None::<&gio::Cancellable>, move |result| {
        let path = match result {
            Ok(folder) => match folder.path() {
                Some(path) => path,
                None => {
                    report_vault_open_error(
                        &state,
                        &widgets,
                        "The selected folder is not a local path.",
                    );
                    return;
                }
            },
            Err(error) if !error.matches(gio::IOErrorEnum::Cancelled) => {
                report_vault_open_error(
                    &state,
                    &widgets,
                    &format!("Folder selection failed: {error}"),
                );
                return;
            }
            Err(_) => return,
        };

        // Refuse a folder that already holds a vault or plaintext notes *before*
        // prompting for a password or deriving a key. Stamping an encrypted
        // keyfile onto an existing ordinary vault would leave its notes in
        // plaintext; conversion is not supported in this release.
        if let Err(error) = Vault::check_encrypted_target(&path) {
            report_vault_open_error(&state, &widgets, &format!("{error}"));
            return;
        }

        let state = state.clone();
        let widgets = widgets.clone();
        let pending = pending.clone();
        present_password_dialog(
            &widgets.window.clone(),
            "Vault Password",
            "Everything in this vault will be encrypted and protected by this vault password. \
             There is no recovery if it is lost.",
            true,
            true,
            "Create Secure Vault",
            move |maybe_password| {
                let Some(password) = maybe_password else {
                    return;
                };
                let vault_id = Uuid::new_v4();
                let session = widgets.sessions.current();
                widgets
                    .welcome_status
                    .set_label("Creating the Secure Vault…");

                let worker = gio::spawn_blocking(
                    move || -> senatorial_notes::Result<(Vec<u8>, VaultKeys)> {
                        vault_crypto::create_keyfile(vault_id, password.as_str())
                    },
                );
                let state = state.clone();
                let widgets = widgets.clone();
                let pending = pending.clone();
                let path = path.clone();
                glib::spawn_future_local(async move {
                    let outcome = worker.await;
                    if !widgets.sessions.is_current(session) {
                        return;
                    }
                    let created = match outcome {
                        Ok(Ok((keyfile_bytes, keys))) => {
                            Vault::finish_create_encrypted(&path, vault_id, &keyfile_bytes, keys)
                        }
                        Ok(Err(error)) => Err(error),
                        Err(_) => {
                            report_vault_open_error(
                                &state,
                                &widgets,
                                "Key derivation failed unexpectedly.",
                            );
                            return;
                        }
                    };
                    match created {
                        Ok(vault) => {
                            proceed_with_opened_vault(vault, path, &state, &widgets, &pending)
                        }
                        Err(error) => report_vault_open_error(
                            &state,
                            &widgets,
                            &format!("The Secure Vault could not be created: {error}"),
                        ),
                    }
                });
            },
        );
    });
}

/// Creates the first-run "Main" Standard Vault under the managed workspace
/// root, records it, and shows the one-time first-run panel. On any failure
/// the user is left on the ordinary welcome screen and first-run is **not**
/// marked done, so it is retried next launch.
fn run_first_run_setup(
    state: &Rc<RefCell<AppState>>,
    widgets: &Widgets,
    pending: &Rc<RefCell<PendingSaves>>,
) {
    let Some(root) = paths::default_workspace_root() else {
        show_welcome_error(
            widgets,
            "SenatorialNotes could not locate your Documents folder. \
             Use \u{201c}Create Vault\u{2026}\u{201d} to choose a location.",
        );
        return;
    };
    let main_path = root.join("Main");
    match Vault::create(&main_path) {
        Ok(vault) => {
            {
                let mut state = state.borrow_mut();
                state
                    .config
                    .record_vault_open(vault.root(), VaultKind::Ordinary);
                state.config.set_vault_display_name(vault.root(), "Main");
                state.config.first_run_done = true;
                let _ = state.config.save();
            }
            widgets.first_run_panel.set_visible(true);
            widgets.stack.set_visible_child_name("welcome");
            render_vault_switcher(state, widgets, pending);
        }
        Err(error) => {
            show_welcome_error(
                widgets,
                &format!(
                    "Your workspace could not be set up automatically ({error}). \
                     Use \u{201c}Create Vault\u{2026}\u{201d} to choose a location."
                ),
            );
        }
    }
}

/// First-run "Set Secure Vault Password" / sidebar "New Secure Vault": creates
/// a Secure Vault in the managed workspace root with **no folder picker**,
/// display name "Secure" (or "Secure 2", …). The password is asked first and
/// the vault is only created once a valid password is set (there is no
/// uninitialised on-disk vault). Key derivation runs off the GTK main thread.
fn present_managed_secure_vault_setup(
    state: &Rc<RefCell<AppState>>,
    widgets: &Widgets,
    pending: &Rc<RefCell<PendingSaves>>,
) {
    widgets.vault_popover.popdown();
    let Some(root) = paths::default_workspace_root() else {
        report_vault_open_error(
            state,
            widgets,
            "SenatorialNotes could not locate your Documents folder. \
             Use \u{201c}Create Secure Vault\u{2026}\u{201d} to choose a location.",
        );
        return;
    };

    // A clean managed folder: Secure, Secure 2, Secure 3, …
    let mut chosen: Option<(PathBuf, String)> = None;
    for n in 0..50 {
        let name = if n == 0 {
            "Secure".to_string()
        } else {
            format!("Secure {}", n + 1)
        };
        let folder = root.join(&name);
        if Vault::check_encrypted_target(&folder).is_ok() {
            chosen = Some((folder, name));
            break;
        }
    }
    let Some((target, display)) = chosen else {
        report_vault_open_error(
            state,
            widgets,
            "Could not find a free folder for a new Secure Vault. \
             Use \u{201c}Create Secure Vault\u{2026}\u{201d} to choose a location.",
        );
        return;
    };

    let state = state.clone();
    let widgets = widgets.clone();
    let pending = pending.clone();
    present_password_dialog(
        &widgets.window.clone(),
        "Set Secure Vault Password",
        "Everything in your Secure Vault is encrypted and protected by this vault password. \
         There is no recovery if it is lost.",
        true,
        true,
        "Create Secure Vault",
        move |maybe_password| {
            let Some(password) = maybe_password else {
                return;
            };
            let vault_id = Uuid::new_v4();
            let session = widgets.sessions.current();
            widgets
                .welcome_status
                .set_label("Creating your Secure Vault…");

            let worker =
                gio::spawn_blocking(move || -> senatorial_notes::Result<(Vec<u8>, VaultKeys)> {
                    vault_crypto::create_keyfile(vault_id, password.as_str())
                });
            let state = state.clone();
            let widgets = widgets.clone();
            let pending = pending.clone();
            let target = target.clone();
            let display = display.clone();
            glib::spawn_future_local(async move {
                let outcome = worker.await;
                if !widgets.sessions.is_current(session) {
                    return;
                }
                let created = match outcome {
                    Ok(Ok((keyfile_bytes, keys))) => {
                        Vault::finish_create_encrypted(&target, vault_id, &keyfile_bytes, keys)
                    }
                    Ok(Err(error)) => Err(error),
                    Err(_) => {
                        report_vault_open_error(
                            &state,
                            &widgets,
                            "Key derivation failed unexpectedly.",
                        );
                        return;
                    }
                };
                match created {
                    Ok(vault) => {
                        {
                            let mut state = state.borrow_mut();
                            state.config.set_vault_display_name(vault.root(), &display);
                            let _ = state.config.save();
                        }
                        widgets.first_run_panel.set_visible(false);
                        proceed_with_opened_vault(vault, target, &state, &widgets, &pending);
                    }
                    Err(error) => report_vault_open_error(
                        &state,
                        &widgets,
                        &format!("The Secure Vault could not be created: {error}"),
                    ),
                }
            });
        },
    );
}

/// What the dialog's third button does, if there is one.
enum ContentionAction {
    /// Proven-dead lock: a reviewed writable takeover.
    TakeOver(DeadReason),
    /// A live session on this machine: try to raise its window.
    ShowExistingWindow,
}

/// Modal dialog shown when the new vault's advisory lock is contended.
///
/// **Takeover is offered only for a [`LockStatus::ProvenDead`] lock** - one
/// where the previous owner is positively known not to be running. A
/// [`LockStatus::Blocked`] lock (a live session, a network peer we cannot
/// verify, a malformed or newer-format file) offers only Open Read-Only and
/// Cancel. In every case the current vault/session is untouched until a
/// decision is made, and a read-only fallback never modifies the blocked
/// owner's lock file.
fn present_lock_contention_dialog(
    vault: Vault,
    status: LockStatus,
    path: PathBuf,
    state: &Rc<RefCell<AppState>>,
    widgets: &Widgets,
    pending: &Rc<RefCell<PendingSaves>>,
) {
    let name = { vault_display_name_for(&state.borrow().config, &path) };
    let lock_file = vault.state_dir().join("vault.lock");

    let (detail, buttons, action): (String, Vec<&str>, Option<ContentionAction>) = match &status {
        LockStatus::ProvenDead { owner, reason } => (
            format!(
                "\u{201c}{name}\u{201d} has a lock left behind. {} You can open it read-only, or \
                 take over the lock.",
                reason.explain(owner)
            ),
            vec!["Cancel", "Open Read-Only", "Take Over"],
            Some(ContentionAction::TakeOver(*reason)),
        ),
        LockStatus::Blocked {
            owner,
            reason: BlockedReason::Live,
        } => (
            BlockedReason::Live.explain(owner.as_ref(), &lock_file),
            vec!["Cancel", "Open Read-Only", "Show Existing Window"],
            Some(ContentionAction::ShowExistingWindow),
        ),
        LockStatus::Blocked { owner, reason } => (
            reason.explain(owner.as_ref(), &lock_file),
            vec!["Cancel", "Open Read-Only"],
            None,
        ),
        // acquire never returns Free / HeldByThisProcess as Contended.
        LockStatus::Free | LockStatus::HeldByThisProcess => {
            commit_vault_switch(vault, VaultLock::read_only(), path, state, widgets, pending);
            return;
        }
    };

    let dialog = gtk::AlertDialog::builder()
        .modal(true)
        .message(format!(
            "Another session may be using \u{201c}{name}\u{201d}"
        ))
        .detail(detail)
        .buttons(buttons)
        .cancel_button(0)
        .default_button(0)
        .build();

    let state = state.clone();
    let widgets = widgets.clone();
    let pending = pending.clone();
    let parent = widgets.window.clone();
    dialog.choose(Some(&parent), None::<&gio::Cancellable>, move |result| {
        let Ok(choice) = result else {
            return;
        };
        match choice {
            // Cancel: the switch is abandoned; the current vault is untouched.
            0 => {}
            // Open read-only - a non-owning lock; the blocked owner's file is
            // never touched.
            1 => commit_vault_switch(
                vault,
                VaultLock::read_only(),
                path,
                &state,
                &widgets,
                &pending,
            ),
            2 => match &action {
                Some(ContentionAction::TakeOver(reason)) => {
                    match VaultLock::take_over(&vault, *reason) {
                        Ok(LockAcquisition::Acquired(lock)) => {
                            commit_vault_switch(vault, lock, path, &state, &widgets, &pending)
                        }
                        Ok(LockAcquisition::Contended(_)) => report_vault_open_error(
                            &state,
                            &widgets,
                            "The vault's lock changed while the dialog was open - it was not \
                             taken over.",
                        ),
                        Err(error) => report_vault_open_error(
                            &state,
                            &widgets,
                            &format!("Could not take over the vault lock: {error}"),
                        ),
                    }
                }
                Some(ContentionAction::ShowExistingWindow) => {
                    if let Some(app) = widgets.window.application() {
                        app.activate();
                    }
                }
                None => {}
            },
            _ => {}
        }
    });
}

/// R18: a Secure Vault opened with plaintext storage artifacts in its root
/// (from an old / incompatible binary). Nothing has been moved. The user
/// chooses: cancel, open read-only (artifacts untouched), or explicitly
/// quarantine the plaintext into `.senatorial-notes/quarantine/<timestamp>/`
/// and then open normally.
fn present_plaintext_conflict_dialog(
    vault: Vault,
    path: PathBuf,
    state: &Rc<RefCell<AppState>>,
    widgets: &Widgets,
    pending: &Rc<RefCell<PendingSaves>>,
) {
    let name = { vault_display_name_for(&state.borrow().config, &path) };
    let (file_count, categories): (usize, Vec<ArtifactCategory>) = vault
        .pending_quarantine()
        .map(|p| (p.file_count(), p.categories().to_vec()))
        .unwrap_or((0, Vec::new()));
    let found: Vec<&str> = categories.iter().map(|c| c.describe()).collect();

    let detail = format!(
        "\u{201c}{name}\u{201d} is a Secure Vault, but its folder also contains {file_count} \
         plaintext file(s) that an older or incompatible version of SenatorialNotes wrote \
         ({}). Your encrypted notes are safe and separate, and nothing has been changed.\n\n\
         To open this Secure Vault normally, the plaintext files must first be moved, \
         unchanged, into a quarantine folder inside the vault. Nothing is ever deleted, \
         merged, or imported.",
        found.join(", ")
    );

    let dialog = gtk::AlertDialog::builder()
        .modal(true)
        .message(format!("Plaintext files found in \u{201c}{name}\u{201d}"))
        .detail(detail)
        .buttons(vec![
            "Cancel",
            "Open Read-Only",
            "Quarantine Plaintext Files\u{2026}",
        ])
        .cancel_button(0)
        .default_button(0)
        .build();

    let state = state.clone();
    let widgets = widgets.clone();
    let pending = pending.clone();
    let parent = widgets.window.clone();
    dialog.choose(Some(&parent), None::<&gio::Cancellable>, move |result| {
        let Ok(choice) = result else {
            return;
        };
        match choice {
            // Cancel: the switch is abandoned; the current vault is untouched.
            0 => {}
            // Open read-only: the plaintext artifacts are left exactly where
            // they are; the vault opens with a non-owning lock and every write
            // path is disabled.
            1 => commit_vault_switch(
                vault,
                VaultLock::read_only(),
                path,
                &state,
                &widgets,
                &pending,
            ),
            // Quarantine, then open normally.
            2 => match vault.quarantine_plaintext() {
                Ok(report) => match Vault::open(&path) {
                    Ok(clean) => {
                        report_quarantine_success(&widgets, &report);
                        finish_opening_vault(clean, path.clone(), &state, &widgets, &pending);
                    }
                    Err(error) => report_vault_open_error(
                        &state,
                        &widgets,
                        &format!(
                            "The plaintext files were quarantined to {}, but the Secure Vault \
                             could not be reopened: {error}",
                            report.quarantine_path.display()
                        ),
                    ),
                },
                Err(error) => present_quarantine_failure_dialog(
                    vault, path, error, &state, &widgets, &pending,
                ),
            },
            _ => {}
        }
    });
}

fn report_quarantine_success(widgets: &Widgets, report: &QuarantineReport) {
    widgets.save_status.set_label(&format!(
        "Moved {} plaintext file(s) to {}",
        report.file_count,
        report.quarantine_path.display()
    ));
}

/// Quarantine could not complete. Every original file is preserved (some may
/// already be inside the quarantine folder). The vault must not open writable.
fn present_quarantine_failure_dialog(
    vault: Vault,
    path: PathBuf,
    error: senatorial_notes::Error,
    state: &Rc<RefCell<AppState>>,
    widgets: &Widgets,
    pending: &Rc<RefCell<PendingSaves>>,
) {
    let dialog = gtk::AlertDialog::builder()
        .modal(true)
        .message("The plaintext files could not be quarantined")
        .detail(format!(
            "{error}\n\nNothing was deleted and every original file is still in the vault \
             folder (or already inside the quarantine folder). The Secure Vault was not \
             opened for editing. You can open it read-only, or resolve the files yourself \
             and try again."
        ))
        .buttons(vec!["Cancel", "Open Read-Only"])
        .cancel_button(0)
        .default_button(0)
        .build();
    let state = state.clone();
    let widgets = widgets.clone();
    let pending = pending.clone();
    let parent = widgets.window.clone();
    dialog.choose(Some(&parent), None::<&gio::Cancellable>, move |result| {
        if let Ok(1) = result {
            commit_vault_switch(
                vault,
                VaultLock::read_only(),
                path,
                &state,
                &widgets,
                &pending,
            );
        }
    });
}

/// The folder's own name, for the header switcher.
/// The folder-derived fallback name for a vault at `path`.
fn vault_display_name(path: &Path) -> &str {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(APP_NAME)
}

/// The vault's display name: the user-chosen name from `config.vault_index`
/// if set, otherwise the folder basename. A rename only ever changes the
/// stored display name - never the folder, the vault, or any blob.
fn vault_display_name_for(config: &AppConfig, path: &Path) -> String {
    config
        .vault_display_name(path)
        .map(str::to_string)
        .unwrap_or_else(|| vault_display_name(path).to_string())
}

/// Serialises a `ViewMode` for `VaultSessionState::last_view`.
fn view_token(view: &ViewMode) -> String {
    match view {
        ViewMode::AllNotes => "all-notes".to_string(),
        ViewMode::RecentlyOpened => "recently-opened".to_string(),
        ViewMode::Favourites => "favourites".to_string(),
        ViewMode::Pinned => "pinned".to_string(),
        ViewMode::Archive => "archive".to_string(),
        ViewMode::EncryptedNotes => "encrypted-notes".to_string(),
        ViewMode::Trash => "trash".to_string(),
        ViewMode::Notebook(path) => format!("notebook:{}", path.display()),
    }
}

fn parse_view_token(token: &str) -> Option<ViewMode> {
    match token {
        "all-notes" => Some(ViewMode::AllNotes),
        "recently-opened" => Some(ViewMode::RecentlyOpened),
        "favourites" => Some(ViewMode::Favourites),
        "pinned" => Some(ViewMode::Pinned),
        // A vault last left on the removed "Recently Edited" view opens on
        // "Recently Opened" now.
        "recently-edited" => Some(ViewMode::RecentlyOpened),
        "archive" => Some(ViewMode::Archive),
        "encrypted-notes" => Some(ViewMode::EncryptedNotes),
        "trash" => Some(ViewMode::Trash),
        other => other
            .strip_prefix("notebook:")
            .map(|path| ViewMode::Notebook(PathBuf::from(path))),
    }
}

/// A restored `Notebook` view that no longer exists on disk falls back to All
/// Notes; every other view is used as-is.
fn resolve_restored_view(state: &Rc<RefCell<AppState>>, view: ViewMode) -> ViewMode {
    let ViewMode::Notebook(ref relative) = view else {
        return view;
    };
    let exists = {
        let state = state.borrow();
        state.vault.as_ref().is_some_and(|vault| {
            vault
                .list_notebooks()
                .map(|notebooks| {
                    notebooks
                        .iter()
                        .any(|entry| entry.relative_path == *relative)
                })
                .unwrap_or(false)
        })
    };
    if exists { view } else { ViewMode::AllNotes }
}

/// Cancels the debounced editor-presentation timers so a stale one cannot fire
/// into the next vault's editor. (They carry no vault state, but the buffer
/// they target is reused across vaults.)
fn cancel_editor_deferrals(widgets: &Widgets) {
    if let Some(source) = widgets.style_recompute_source.borrow_mut().take() {
        source.remove();
    }
    if let Some(source) = widgets.format_toolbar_update_source.borrow_mut().take() {
        source.remove();
    }
}

/// Captures the current vault's per-vault UI state (last note, last view,
/// recently-opened list, editor scroll). Never stores note *contents*.
///
/// - **Standard Vault** → the app config, keyed by `vault_id`.
/// - **Secure Vault** → its sealed encrypted manifest (never plaintext), and
///   only while it is unlocked.
fn persist_vault_session_state(state: &Rc<RefCell<AppState>>, widgets: &Widgets) {
    let scroll = editor_scroll_offset(widgets);
    let mut state = state.borrow_mut();
    let Some(vault) = state.vault.as_ref() else {
        return;
    };
    let vault_id = vault.vault_id();
    let is_encrypted = vault.is_encrypted();
    let is_locked = vault.is_locked();

    state.session.last_note = state.flow.selected_note();
    state.session.last_view = Some(view_token(state.flow.view()));
    state.session.editor_scroll = scroll;
    let session = state.session.clone();

    if is_encrypted {
        // A locked Secure Vault has no key to seal with; the working copy is
        // simply dropped. The notebook name in `last_view`, note UUIDs in
        // `recently_opened`, etc. must never reach the plaintext config.
        if !is_locked && let Some(vault) = state.vault.as_ref() {
            let _ = vault.set_encrypted_session_state(session);
        }
        return;
    }
    state.config.set_vault_session(vault_id, session);
    let _ = state.config.save();
}

/// Loads the working session-state copy for the just-opened vault.
fn load_vault_session_state(state: &Rc<RefCell<AppState>>) {
    let mut state = state.borrow_mut();
    let Some(vault) = state.vault.as_ref() else {
        state.session = VaultSessionState::default();
        return;
    };
    state.session = if vault.is_encrypted() {
        vault.encrypted_session_state().unwrap_or_default()
    } else {
        state
            .config
            .vault_session(vault.vault_id())
            .cloned()
            .unwrap_or_default()
    };
}

/// Records that the user opened/viewed note `id` (for "Recently Opened").
/// Never rewrites the note file.
fn record_note_opened(state: &mut AppState, id: Uuid) {
    state.session.record_opened(id);
}

fn editor_scroll_offset(widgets: &Widgets) -> Option<f64> {
    let value = widgets.editor.vadjustment()?.value();
    (value > 0.0).then_some(value)
}

fn restore_editor_scroll(widgets: &Widgets, offset: f64) {
    let Some(adjustment) = widgets.editor.vadjustment() else {
        return;
    };
    // The buffer has just been populated; the adjustment's upper bound is not
    // settled until GTK has laid the view out, so defer the restore.
    glib::idle_add_local_once(move || {
        let ceiling = (adjustment.upper() - adjustment.page_size()).max(0.0);
        adjustment.set_value(offset.min(ceiling));
    });
}

/// Enables or disables every control that would mutate the vault, and shows the
/// read-only indicator. Browsing (selection, view switching, search, reading a
/// note) is never disabled.
fn apply_read_only_ui(widgets: &Widgets, read_only: bool) {
    let writable = !read_only;
    widgets.vault_readonly_icon.set_visible(read_only);
    for widget in [
        widgets.new_note.upcast_ref::<gtk::Widget>(),
        widgets.new_notebook.upcast_ref::<gtk::Widget>(),
        widgets.delete_button.upcast_ref::<gtk::Widget>(),
    ] {
        widget.set_sensitive(writable);
    }
    widgets.formatting_bar.set_sensitive(writable);
    widgets.tag_add_entry.set_sensitive(writable);
    // `set_editable(false)` keeps the text selectable/scrollable for reading
    // while blocking every edit - `changed` never fires from a rejected keypress.
    widgets.title.set_editable(writable);
    widgets.editor.set_editable(writable);
}

/// Rebuilds the header vault-switcher popover (current identity + read-only
/// note + recent list) and the welcome-screen recent list.
fn render_vault_switcher(
    state: &Rc<RefCell<AppState>>,
    widgets: &Widgets,
    pending: &Rc<RefCell<PendingSaves>>,
) {
    let (secure_vault_unlocked, vault_open, session_read_only) = {
        let state = state.borrow();
        (
            state
                .vault
                .as_ref()
                .is_some_and(|vault| vault.is_encrypted() && !vault.is_locked()),
            state.vault.is_some(),
            state.read_only,
        )
    };
    widgets
        .vault_popover_secure_actions
        .set_visible(secure_vault_unlocked);
    // Disable the Secure-Vault menu actions for a Standard Vault so "Lock
    // Vault" cannot be invoked where there is no vault-level key; "Rename
    // Vault…" needs any open vault.
    if let Some(app) = widgets.window.application() {
        let toggle = |name: &str, enabled: bool| {
            if let Some(action) = app.lookup_action(name)
                && let Ok(action) = action.downcast::<gio::SimpleAction>()
            {
                action.set_enabled(enabled);
            }
        };
        toggle("lock-vault", secure_vault_unlocked);
        toggle("change-vault-password", secure_vault_unlocked);
        toggle("rename-vault", vault_open);
        // Plaintext export needs an unlocked Secure Vault and a writable
        // session (never offered while read-only, e.g. an unresolved R18
        // plaintext conflict).
        toggle(
            "export-standard-vault",
            secure_vault_unlocked && !session_read_only,
        );
    }

    let (current_root, current_name, read_only, warning, recents) = {
        let state = state.borrow();
        let current_root = state.vault.as_ref().map(|vault| vault.root().to_path_buf());
        let current_name = current_root
            .as_deref()
            .map(|path| vault_display_name_for(&state.config, path));
        let warning = state
            .vault
            .as_ref()
            .and_then(|vault| vault.migration().warning());
        (
            current_root,
            current_name,
            state.read_only,
            warning,
            state.config.recent_vaults_mru(),
        )
    };

    widgets
        .vault_popover_name
        .set_label(current_name.as_deref().unwrap_or("No vault open"));
    widgets.vault_popover_path.set_label(
        current_root
            .as_deref()
            .map(|path| path.display().to_string())
            .unwrap_or_default()
            .as_str(),
    );
    let status = match (read_only, warning.as_deref()) {
        (true, Some(reason)) => format!("Read-only · {reason}"),
        (true, None) => "Read-only".to_string(),
        (false, Some(reason)) => reason.to_string(),
        (false, None) => String::new(),
    };
    widgets.vault_popover_status.set_visible(!status.is_empty());
    widgets.vault_popover_status.set_label(&status);
    if read_only {
        widgets.vault_popover_status.add_css_class("warning");
    } else {
        widgets.vault_popover_status.remove_css_class("warning");
    }

    let current_canonical = current_root.as_deref().map(canonical_path);
    for container in [&widgets.vault_recent_box, &widgets.welcome_recent_box] {
        while let Some(child) = container.first_child() {
            container.remove(&child);
        }
    }
    let mut shown = 0usize;
    for path in recents {
        if current_canonical.as_deref() == Some(&canonical_path(&path)) {
            continue;
        }
        widgets
            .vault_recent_box
            .append(&recent_vault_row(&path, state, widgets, pending));
        widgets
            .welcome_recent_box
            .append(&recent_vault_row(&path, state, widgets, pending));
        shown += 1;
    }
    let has_recents = shown > 0;
    widgets.welcome_recent_box.set_visible(has_recents);
    widgets.welcome_recent_heading.set_visible(has_recents);
    if !has_recents {
        let empty = gtk::Label::new(Some("No other vaults yet"));
        empty.add_css_class("caption");
        empty.add_css_class("dim-label");
        empty.set_xalign(0.0);
        widgets.vault_recent_box.append(&empty);
    }

    render_secure_vaults_sidebar(state, widgets, pending);
}

/// Maximum Secure Vaults shown directly in the sidebar; the rest are behind
/// "More…" (which opens the full vault switcher).
const SIDEBAR_SECURE_VAULTS: usize = 4;

/// Rebuilds the "Secured Vaults" sidebar section: up to
/// [`SIDEBAR_SECURE_VAULTS`] Secure Vaults (most-recently-opened first), each a
/// direct switch button, plus a "More…" row when there are more. The list is
/// never unbounded.
fn render_secure_vaults_sidebar(
    state: &Rc<RefCell<AppState>>,
    widgets: &Widgets,
    pending: &Rc<RefCell<PendingSaves>>,
) {
    while let Some(child) = widgets.secure_vaults_box.first_child() {
        widgets.secure_vaults_box.remove(&child);
    }

    let (vaults, current_canonical) = {
        let state = state.borrow();
        let current = state
            .vault
            .as_ref()
            .filter(|vault| vault.is_encrypted())
            .map(|vault| canonical_path(vault.root()));
        (state.config.secure_vaults_mru(), current)
    };

    if vaults.is_empty() {
        let hint = gtk::Label::new(Some("No Secure Vaults yet"));
        hint.add_css_class("caption");
        hint.add_css_class("dim-label");
        hint.set_xalign(0.0);
        widgets.secure_vaults_box.append(&hint);
        return;
    }

    for path in vaults.iter().take(SIDEBAR_SECURE_VAULTS) {
        let display_name = { vault_display_name_for(&state.borrow().config, path) };
        let button = sidebar_button(&display_name, "channel-secure-symbolic");
        button.set_tooltip_text(Some("Switch to this Secure Vault"));
        if current_canonical.as_deref() == Some(&canonical_path(path)) {
            button.add_css_class("sidebar-selected");
        }
        {
            let state = state.clone();
            let widgets = widgets.clone();
            let pending = pending.clone();
            let path = path.clone();
            button.connect_clicked(move |_| open_vault(&path, false, &state, &widgets, &pending));
        }
        widgets.secure_vaults_box.append(&button);
    }

    if vaults.len() > SIDEBAR_SECURE_VAULTS {
        let more = sidebar_button("More…", "view-more-symbolic");
        more.set_tooltip_text(Some("Show all vaults"));
        let widgets_for_more = widgets.clone();
        more.connect_clicked(move |_| widgets_for_more.vault_popover.popup());
        widgets.secure_vaults_box.append(&more);
    }
}

/// One recent-vault row: a switch button (name + path) plus a "remove from
/// recent" button. A folder that is no longer present is shown as unavailable
/// and cannot be opened, only removed.
fn recent_vault_row(
    path: &Path,
    state: &Rc<RefCell<AppState>>,
    widgets: &Widgets,
    pending: &Rc<RefCell<PendingSaves>>,
) -> gtk::Widget {
    let available = path.is_dir();
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 4);

    let text = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let display_name = { vault_display_name_for(&state.borrow().config, path) };
    let name = gtk::Label::new(Some(&format!(
        "{}{}",
        display_name,
        if available { "" } else { "  (missing)" }
    )));
    name.set_xalign(0.0);
    name.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    let path_label = gtk::Label::new(Some(&path.display().to_string()));
    path_label.set_xalign(0.0);
    path_label.add_css_class("caption");
    path_label.add_css_class("dim-label");
    path_label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    text.append(&name);
    text.append(&path_label);

    let open = gtk::Button::builder()
        .child(&text)
        .css_classes(["flat"])
        .hexpand(true)
        .sensitive(available)
        .tooltip_text(if available {
            "Switch to this vault"
        } else {
            "This folder is no longer available"
        })
        .build();
    {
        let state = state.clone();
        let widgets = widgets.clone();
        let pending = pending.clone();
        let path = path.to_path_buf();
        open.connect_clicked(move |_| {
            open_vault(&path, false, &state, &widgets, &pending);
        });
    }
    row.append(&open);

    let forget = gtk::Button::builder()
        .icon_name("window-close-symbolic")
        .css_classes(["flat"])
        .valign(gtk::Align::Center)
        .tooltip_text("Remove from Recent")
        .build();
    {
        let state = state.clone();
        let widgets = widgets.clone();
        let pending = pending.clone();
        let path = path.to_path_buf();
        forget.connect_clicked(move |_| {
            {
                let mut state = state.borrow_mut();
                state.config.forget_vault(&path);
                let _ = state.config.save();
            }
            render_vault_switcher(&state, &widgets, &pending);
        });
    }
    row.append(&forget);
    row.upcast()
}

fn switch_view(mode: ViewMode, state: &Rc<RefCell<AppState>>, widgets: &Widgets) {
    // A locked Secure Vault has no decrypted notes to show. Keep the sidebar
    // affordances inert rather than surfacing a "could not refresh" error.
    let vault_locked = {
        let state = state.borrow();
        state
            .vault
            .as_ref()
            .is_some_and(|vault| vault.is_encrypted() && vault.is_locked())
    };
    if vault_locked {
        if widgets.library_split.is_collapsed() {
            widgets.library_split.set_show_sidebar(false);
        }
        return;
    }
    let already_selected = { state.borrow().flow.view() == &mode };
    if already_selected {
        if widgets.library_split.is_collapsed() {
            widgets.library_split.set_show_sidebar(false);
        }
        return;
    }
    if !persist_active(state, widgets, true) {
        return;
    }
    stash_or_lock_active(state);
    {
        let mut state = state.borrow_mut();
        state.flow.switch_view(mode.clone());
    }
    widgets.search.set_text("");
    apply_view_chrome(&mode, widgets);
    widgets.document_stack.set_visible_child_name("empty");
    set_quick_actions_visible(widgets, false);
    if widgets.library_split.is_collapsed() {
        widgets.library_split.set_show_sidebar(false);
    }
    widgets.content_split.set_show_content(false);
    if refresh_current_view(state, widgets) {
        select_first_row(state, widgets);
    }
}

/// Notebook new notes are created in: the currently selected real notebook,
/// or `Inbox` as the deterministic fallback for every smart view (and before
/// any vault is loaded) - see the "Inbox is special" note in
/// `Vault::is_reserved_notebook`.
fn target_notebook_for_new_note(state: &AppState) -> PathBuf {
    match state.flow.view() {
        ViewMode::Notebook(path) => path.clone(),
        _ => PathBuf::from("Inbox"),
    }
}

fn create_new_note(
    state: &Rc<RefCell<AppState>>,
    widgets: &Widgets,
    pending: &Rc<RefCell<PendingSaves>>,
) {
    let target_notebook = { target_notebook_for_new_note(&state.borrow()) };
    let already_there =
        { state.borrow().flow.view() == &ViewMode::Notebook(target_notebook.clone()) };
    if !already_there {
        cancel_all_timers(pending);
        switch_view(ViewMode::Notebook(target_notebook.clone()), state, widgets);
        let switched =
            { state.borrow().flow.view() == &ViewMode::Notebook(target_notebook.clone()) };
        if !switched {
            return;
        }
    }
    if !prepare_to_leave_active(state, widgets, pending) {
        return;
    }
    if !widgets.search.text().is_empty() {
        widgets.search.set_text("");
    }
    let vault = { state.borrow().vault.clone() };
    let Some(vault) = vault else {
        return;
    };
    match vault.create_note("Untitled", &target_notebook) {
        Ok(note) => {
            let id = note.metadata.id;
            let summary = NoteSummary::from(&note);
            {
                let mut state = state.borrow_mut();
                state.notes.insert(0, summary.clone());
                state.flow.select_note(id);
            }
            insert_note_row(0, &summary, state, widgets);
            select_row_target(RowTarget::Note(id), widgets);
            load_note_by_id(id, state, widgets);
            refresh_watch_baseline(state);
            widgets.content_split.set_show_content(true);
            widgets.title.grab_focus();
            widgets.title.select_region(0, -1);
        }
        Err(error) => widgets
            .save_status
            .set_label(&format!("Could not create note: {error}")),
    }
}

fn apply_view_chrome(mode: &ViewMode, widgets: &Widgets) {
    widgets.notes_heading.set_label(&mode.heading());
    // The sidebar search box keeps a stable product-wording placeholder; it
    // always searches the current vault, never a per-view label.
    widgets
        .empty_trash_button
        .set_visible(*mode == ViewMode::Trash);
    update_library_selection(mode, widgets);
}

fn update_library_selection(mode: &ViewMode, widgets: &Widgets) {
    for button in [
        &widgets.all_notes_button,
        &widgets.inbox_button,
        &widgets.recently_opened_button,
        &widgets.favourites_button,
        &widgets.pinned_button,
        &widgets.archive_button,
        &widgets.trash_button,
    ] {
        button.remove_css_class("sidebar-selected");
    }
    let inbox = Path::new("Inbox");
    match mode {
        ViewMode::AllNotes => widgets.all_notes_button.add_css_class("sidebar-selected"),
        ViewMode::Notebook(path) if path.as_path() == inbox => {
            widgets.inbox_button.add_css_class("sidebar-selected");
        }
        ViewMode::RecentlyOpened => widgets
            .recently_opened_button
            .add_css_class("sidebar-selected"),
        ViewMode::Favourites => widgets.favourites_button.add_css_class("sidebar-selected"),
        ViewMode::Pinned => widgets.pinned_button.add_css_class("sidebar-selected"),
        ViewMode::Archive => widgets.archive_button.add_css_class("sidebar-selected"),
        // `EncryptedNotes` has no sidebar row in this design; it is reachable
        // only via a restored session token and shows no selection highlight.
        ViewMode::EncryptedNotes => {}
        ViewMode::Trash => widgets.trash_button.add_css_class("sidebar-selected"),
        ViewMode::Notebook(_) => {}
    }
    update_notebook_list_selection(mode, widgets);
}

fn select_first_row(state: &Rc<RefCell<AppState>>, widgets: &Widgets) {
    let preferred = {
        let state = state.borrow();
        match state.flow.view() {
            ViewMode::Trash => state.flow.selected_trash().map(RowTarget::Trash),
            _ => state.flow.selected_note().map(RowTarget::Note),
        }
    };
    let target = preferred
        .filter(|target| widgets.selection.index_of(*target).is_some())
        .or_else(|| widgets.selection.target_at(0));
    let Some(target) = target else {
        clear_editor(state, widgets);
        return;
    };
    select_row_target(target, widgets);
    match target {
        RowTarget::Note(id) => select_note_without_prompting_if_locked(id, state, widgets),
        RowTarget::Trash(id) => show_trash_by_id(id, state, widgets),
    }
}

/// Loads note `id` and always prompts for its password immediately if it is
/// a locked encrypted note. Reserved for calls that come from the user
/// directly acting on *this specific* note - clicking its row, pressing
/// next/previous with it as the target, or pressing the "Unlock Note"
/// button - never from an automatic fallback selection. See
/// `open_note_by_id`'s doc comment for why that distinction matters.
fn load_note_by_id(id: Uuid, state: &Rc<RefCell<AppState>>, widgets: &Widgets) {
    open_note_by_id(id, state, widgets, true);
}

/// Loads note `id` and selects it in the sidebar, but - when it is a locked
/// encrypted note - shows the locked placeholder (with its own "Unlock
/// Note" button) without launching the password dialog.
///
/// Used for automatic fallback selection: switching to a notebook or smart
/// view (including Inbox) and picking whatever note sorts first, or picking
/// an adjacent note after the previously-selected one was removed. In both
/// cases the note that ends up selected is incidental to what the user
/// actually asked for - browsing to a view, or deleting something else -
/// not a specific note they chose to open. Auto-prompting there is exactly
/// how merely switching to Inbox (or any view) could end up launching a
/// password dialog for whichever encrypted note happened to be first: see
/// the "Inbox never requires a password" note in `SECURITY.md`.
fn select_note_without_prompting_if_locked(
    id: Uuid,
    state: &Rc<RefCell<AppState>>,
    widgets: &Widgets,
) {
    open_note_by_id(id, state, widgets, false);
}

fn open_note_by_id(
    id: Uuid,
    state: &Rc<RefCell<AppState>>,
    widgets: &Widgets,
    prompt_if_locked: bool,
) {
    let (vault, summary) = {
        let state = state.borrow();
        let Some(vault) = state.vault.clone() else {
            return;
        };
        let Some(summary) = state.notes.iter().find(|summary| summary.id == id).cloned() else {
            return;
        };
        (vault, summary)
    };
    {
        state.borrow_mut().flow.select_note(id);
    }

    if summary.encrypted {
        let cached = { state.borrow_mut().unlocked_cache.remove(&summary.id) };
        if let Some(document) = cached {
            display_document(document, state, widgets);
            return;
        }
        show_locked_placeholder(widgets);
        update_note_quick_actions(state, widgets);
        if !prompt_if_locked {
            return;
        }
        let state_for_unlock = state.clone();
        let widgets_for_unlock = widgets.clone();
        let relative = summary.relative_path.clone();
        present_password_dialog(
            &widgets.window,
            "Unlock Encrypted Note",
            "Enter the note password. SenatorialNotes cannot recover a lost password.",
            false,
            false,
            "Unlock",
            move |password| {
                let Some(password) = password else {
                    return;
                };
                match vault.load_encrypted_note(&relative, password.as_str()) {
                    Ok((note, stamp, session)) => display_document(
                        ActiveDocument::Encrypted {
                            note,
                            stamp,
                            session,
                        },
                        &state_for_unlock,
                        &widgets_for_unlock,
                    ),
                    Err(_) => {
                        widgets_for_unlock.save_status.set_label(
                            "Could not unlock the note. The password may be incorrect or the file may be damaged.",
                        );
                        show_locked_placeholder(&widgets_for_unlock);
                    }
                }
            },
        );
        return;
    }

    // Fast path: a clean, already-parsed copy of this note whose file is
    // unchanged on disk. Confirmed with a stat only - no read or parse.
    let cached = { state.borrow_mut().plain_cache.remove(&id) };
    if let Some((note, stamp)) = cached {
        let still_current = vault
            .note_path(&note.relative_path)
            .is_ok_and(|path| stamp.metadata_matches(&path));
        if still_current {
            display_document(ActiveDocument::Plain { note, stamp }, state, widgets);
            return;
        }
    }

    match vault.load_note(&summary.relative_path) {
        Ok((note, stamp)) => {
            display_document(ActiveDocument::Plain { note, stamp }, state, widgets)
        }
        Err(error) => widgets
            .save_status
            .set_label(&format!("Could not open note: {error}")),
    }
}

/// Coalescing delay for rapid row selection. Long enough to fold a burst of
/// clicks into one dispatch, short enough to stay imperceptible on a single
/// deliberate click (well under one 60 Hz frame).
const SELECTION_DISPATCH_MS: u64 = 12;

/// Records the newest requested selection and arms a single coalesced dispatch.
/// A rapid burst of row clicks collapses to one load of the final target instead
/// of one full leave-and-load per intermediate row.
///
/// A normal-priority `timeout` is used rather than a low-priority idle: under a
/// flood of click/motion events plus frame-clock ticks an idle callback can be
/// starved for a visibly long pause, which is the residual "stall" that the
/// synthetic coordinator test could not reproduce. Exactly one timeout is ever
/// outstanding (`select_source`), and it consumes only the newest `pending_select`,
/// so stale queued work cannot accumulate.
fn request_selection(
    target: RowTarget,
    state: &Rc<RefCell<AppState>>,
    widgets: &Widgets,
    pending: &Rc<RefCell<PendingSaves>>,
) {
    widgets.pending_select.set(Some(target));
    if widgets.select_source.borrow().is_some() {
        return;
    }
    let state = state.clone();
    let widgets = widgets.clone();
    let pending = pending.clone();
    let widgets_for_dispatch = widgets.clone();
    let session = widgets.sessions.current();
    let source =
        glib::timeout_add_local_once(Duration::from_millis(SELECTION_DISPATCH_MS), move || {
            widgets_for_dispatch.select_source.replace(None);
            let Some(target) = widgets_for_dispatch.pending_select.take() else {
                return;
            };
            // A vault switch since the click was queued makes this dispatch
            // stale - the target belonged to the previous vault's list.
            if !widgets_for_dispatch.sessions.is_current(session) {
                return;
            }
            // The row may already be gone if the list was rebuilt since the click.
            if widgets_for_dispatch.selection.index_of(target).is_none() {
                return;
            }
            dispatch_selection(target, &state, &widgets_for_dispatch, &pending);
        });
    widgets.select_source.replace(Some(source));
}

/// Performs the actual leave-and-load for a coalesced selection. Runs at most
/// once per burst, from the `request_selection` timeout.
fn dispatch_selection(
    target: RowTarget,
    state: &Rc<RefCell<AppState>>,
    widgets: &Widgets,
    pending: &Rc<RefCell<PendingSaves>>,
) {
    match target {
        RowTarget::Note(id) => {
            let already_active =
                { state.borrow().active.as_ref().map(ActiveDocument::id) == Some(id) };
            if already_active {
                // The burst ended back on the open note: no reload, no editor
                // replacement, no metadata walk - just keep the row highlighted.
                select_row_target(target, widgets);
                widgets.content_split.set_show_content(true);
                return;
            }
            if !prepare_to_leave_active(state, widgets, pending) {
                reselect_active_note(state, widgets);
                return;
            }
            select_row_target(target, widgets);
            load_note_by_id(id, state, widgets);
            widgets.content_split.set_show_content(true);
        }
        RowTarget::Trash(id) => {
            select_row_target(target, widgets);
            show_trash_by_id(id, state, widgets);
            widgets.content_split.set_show_content(true);
        }
    }
}

/// Cancels an armed coalesced-selection dispatch, if any. Used on shutdown so a
/// timeout cannot fire into half-disposed widgets.
fn cancel_pending_selection(widgets: &Widgets) {
    if let Some(source) = widgets.select_source.borrow_mut().take() {
        source.remove();
    }
    widgets.pending_select.set(None);
}

fn display_document(document: ActiveDocument, state: &Rc<RefCell<AppState>>, widgets: &Widgets) {
    let title = document.note().metadata.title.clone();
    let body = document.note().body.clone();
    let note_id = document.note().metadata.id;
    let encrypted = document.is_encrypted();
    {
        let mut state = state.borrow_mut();
        if let Some(mut previous) = state.active.take() {
            previous.clear_sensitive();
        }
        state.title_draft = title.clone();
        state.body_dirty = false;
        state.title_dirty = false;
        state.active = Some(document);
        state.last_sensitive_activity = None;
        // "Recently Opened" tracks the user viewing a note - never its
        // modification time, and this never rewrites the note file.
        record_note_opened(&mut state, note_id);
        touch_sensitive_activity(&mut state);
    }
    let _editor_guard = widgets.editor_events.suppress();
    widgets.title.set_sensitive(true);
    widgets.editor.set_sensitive(true);
    widgets.formatting_bar.set_sensitive(true);
    widgets.title.set_text(&title);
    set_buffer_text_silently(&widgets.buffer, &body);
    // Style the note as soon as it opens rather than waiting for the user's
    // first edit; loading is already suppressed above, so the debounced
    // recompute wired to buffer::changed would otherwise never fire here.
    recompute_markdown_styles(&widgets.buffer);
    schedule_format_toolbar_update(widgets);
    widgets.document_stack.set_visible_child_name("editor");
    widgets.save_status.set_label(if encrypted {
        "Unlocked · encrypted at rest"
    } else {
        "Saved"
    });
    render_active_tags(state, widgets);
    update_note_quick_actions(state, widgets);
}

/// Shows/updates the note-header quick actions (lock, favourite, pin, overflow)
/// for whatever note is in context. Hidden entirely when no note is selected.
fn update_note_quick_actions(state: &Rc<RefCell<AppState>>, widgets: &Widgets) {
    let (id, summary, has_active, active_encrypted, read_only) = {
        let st = state.borrow();
        let id = st
            .active
            .as_ref()
            .map(ActiveDocument::id)
            .or_else(|| st.flow.selected_note());
        (
            id,
            id.and_then(|id| st.notes.iter().find(|s| s.id == id).cloned()),
            st.active.is_some(),
            st.active.as_ref().is_some_and(ActiveDocument::is_encrypted),
            st.read_only,
        )
    };
    let (Some(id), Some(summary)) = (id, summary) else {
        set_quick_actions_visible(widgets, false);
        return;
    };
    set_quick_actions_visible(widgets, true);

    let locked_encrypted = summary.encrypted && !has_active;

    let (icon, tip) = if active_encrypted {
        ("changes-prevent-symbolic", "Lock Note")
    } else if locked_encrypted {
        ("changes-prevent-symbolic", "Unlock Note")
    } else {
        ("changes-allow-symbolic", "Encrypt Note")
    };
    widgets.note_lock_button.set_icon_name(icon);
    widgets.note_lock_button.set_tooltip_text(Some(tip));
    // Encrypting / locking need a writable vault; unlocking a locked note does
    // not.
    widgets
        .note_lock_button
        .set_sensitive(locked_encrypted || !read_only);

    widgets
        .note_favourite_button
        .set_icon_name(if summary.favourite {
            "starred-symbolic"
        } else {
            "non-starred-symbolic"
        });
    widgets
        .note_favourite_button
        .set_tooltip_text(Some(if summary.favourite {
            "Remove from Favourites"
        } else {
            "Add to Favourites"
        }));
    toggle_css_class(
        &widgets.note_favourite_button,
        "brand-accent",
        summary.favourite,
    );
    widgets
        .note_favourite_button
        .set_sensitive(!locked_encrypted && !read_only);

    widgets
        .note_pin_button
        .set_tooltip_text(Some(if summary.pinned {
            "Unpin Note"
        } else {
            "Pin Note"
        }));
    toggle_css_class(&widgets.note_pin_button, "brand-accent", summary.pinned);
    widgets
        .note_pin_button
        .set_sensitive(!locked_encrypted && !read_only);

    let menu = note_overflow_menu(id, &summary, has_active, read_only);
    widgets.note_overflow_button.set_menu_model(Some(&menu));
}

fn set_quick_actions_visible(widgets: &Widgets, visible: bool) {
    widgets.note_lock_button.set_visible(visible);
    widgets.note_favourite_button.set_visible(visible);
    widgets.note_pin_button.set_visible(visible);
    widgets.note_overflow_button.set_visible(visible);
}

fn toggle_css_class(widget: &gtk::Button, class: &str, on: bool) {
    if on {
        widget.add_css_class(class);
    } else {
        widget.remove_css_class(class);
    }
}

/// The note-header overflow menu: the uncommon actions that do not warrant a
/// dedicated quick button. Favourite / Pin / ordinary Encrypt-Lock-Unlock are
/// deliberately *not* here.
fn note_overflow_menu(
    id: Uuid,
    summary: &NoteSummary,
    has_active: bool,
    read_only: bool,
) -> gio::Menu {
    let menu = gio::Menu::new();

    let organise = gio::Menu::new();
    if !read_only {
        append_targeted_menu_item(&organise, "Rename", "app.context-rename", id);
        append_targeted_menu_item(
            &organise,
            "Move to Notebook…",
            "app.context-move-to-notebook",
            id,
        );
    }
    append_targeted_menu_item(
        &organise,
        if summary.archived {
            "Unarchive"
        } else {
            "Archive"
        },
        "app.context-toggle-archived",
        id,
    );
    if organise.n_items() > 0 {
        menu.append_section(None, &organise);
    }

    let encryption = gio::Menu::new();
    if summary.encrypted {
        if has_active {
            encryption.append(Some("Change Note Password…"), Some("app.change-password"));
            encryption.append(
                Some("Remove Note Encryption…"),
                Some("app.remove-encryption"),
            );
        }
    } else if !read_only {
        append_targeted_menu_item(&encryption, "Encrypt Note…", "app.context-encrypt", id);
    }
    if encryption.n_items() > 0 {
        menu.append_section(None, &encryption);
    }

    let info = gio::Menu::new();
    append_targeted_menu_item(&info, "Note Information", "app.context-note-info", id);
    menu.append_section(None, &info);

    if !read_only {
        let danger = gio::Menu::new();
        append_targeted_menu_item(&danger, "Delete", "app.context-move-to-trash", id);
        menu.append_section(None, &danger);
    }
    menu
}

/// Note-header lock quick action. Dispatches on the note's current state and is
/// strictly note-level: it never calls [`lock_vault`].
fn note_quick_lock(
    state: &Rc<RefCell<AppState>>,
    widgets: &Widgets,
    pending: &Rc<RefCell<PendingSaves>>,
) {
    let Some(id) = current_note_id(state) else {
        return;
    };
    let (has_active, active_encrypted, summary_encrypted) = {
        let st = state.borrow();
        (
            st.active.is_some(),
            st.active.as_ref().is_some_and(ActiveDocument::is_encrypted),
            st.notes
                .iter()
                .find(|s| s.id == id)
                .map(|s| s.encrypted)
                .unwrap_or(false),
        )
    };
    if active_encrypted {
        lock_active_note(state, widgets);
    } else if has_active {
        cancel_all_timers(pending);
        encrypt_active_note(state, widgets);
    } else if summary_encrypted {
        open_note_by_id(id, state, widgets, true);
    }
}

/// Locks only the currently open encrypted note. Leaves any other notes
/// decrypted in the session cache untouched and never touches the vault key.
fn lock_active_note(state: &Rc<RefCell<AppState>>, widgets: &Widgets) {
    if !persist_active(state, widgets, true) {
        return;
    }
    let target = {
        let mut st = state.borrow_mut();
        if !st.active.as_ref().is_some_and(ActiveDocument::is_encrypted) {
            return;
        }
        let Some(mut active) = st.active.take() else {
            return;
        };
        let id = active.id();
        let path = active.note().relative_path.clone();
        active.clear_sensitive();
        st.last_sensitive_activity = None;
        (id, path)
    };
    {
        let _editor_guard = widgets.editor_events.suppress();
        widgets.title.set_text("");
        set_buffer_text_silently(&widgets.buffer, "");
    }
    show_locked_placeholder(widgets);
    widgets.save_status.set_label("Locked · encrypted at rest");

    let (id, path) = target;
    let locked_title = {
        let mut st = state.borrow_mut();
        st.notes.iter_mut().find(|s| s.id == id).map(|summary| {
            *summary = NoteSummary::locked(id, path);
            summary.title.clone()
        })
    };
    let must_rebuild = matches!(
        state.borrow().flow.view(),
        ViewMode::Pinned | ViewMode::Favourites | ViewMode::Archive | ViewMode::RecentlyOpened
    );
    if must_rebuild {
        render_note_list(state, widgets);
    } else if let Some(title) = locked_title {
        let row_widgets = { widgets.row_widgets.borrow().get(&id).cloned() };
        if let Some(rw) = row_widgets {
            rw.title.set_label(&title);
            rw.preview.set_label("Encrypted — unlock to view");
            rw.pin.set_visible(false);
            rw.favourite.set_visible(false);
            rw.archived.set_visible(false);
        }
    }
    update_note_quick_actions(state, widgets);
}

fn show_locked_placeholder(widgets: &Widgets) {
    widgets
        .locked_copy
        .set_label("This note is encrypted on disk. Enter its password to decrypt it in memory.");
    widgets.title.set_sensitive(false);
    widgets.editor.set_sensitive(false);
    widgets.formatting_bar.set_sensitive(false);
    widgets.document_stack.set_visible_child_name("locked");
    widgets.tags_row.set_visible(false);
}

/// Rebuilds the active note's tag-chip row from `state.active`. Hidden
/// entirely when no note is open (or it is locked, in which case `active` is
/// `None` and its real tags are not in memory at all).
fn render_active_tags(state: &Rc<RefCell<AppState>>, widgets: &Widgets) {
    let tags = {
        state
            .borrow()
            .active
            .as_ref()
            .map(|active| active.note().metadata.tags.clone())
    };
    let Some(tags) = tags else {
        widgets.tags_row.set_visible(false);
        return;
    };
    widgets.tags_row.set_visible(true);
    while let Some(child) = widgets.tag_chips.first_child() {
        widgets.tag_chips.remove(&child);
    }
    for tag in tags {
        let chip = gtk::Button::new();
        chip.add_css_class("tag-chip");
        chip.add_css_class("flat");
        chip.set_label(&format!("{tag} ×"));
        chip.set_tooltip_text(Some(&format!("Remove tag \"{tag}\"")));
        chip.update_property(&[gtk::accessible::Property::Label(&format!(
            "Remove tag {tag}"
        ))]);
        let state = state.clone();
        let widgets_for_remove = widgets.clone();
        chip.connect_clicked(move |_| {
            remove_tag_from_active_note(&tag, &state, &widgets_for_remove);
        });
        widgets.tag_chips.append(&chip);
    }
}

fn schedule_body_save(
    state: &Rc<RefCell<AppState>>,
    widgets: &Widgets,
    pending: &Rc<RefCell<PendingSaves>>,
) {
    if widgets.editor_events.is_suppressed() {
        return;
    }
    let has_active = {
        let mut state = state.borrow_mut();
        if state.active.is_none() || state.read_only {
            false
        } else {
            state.body_dirty = true;
            touch_sensitive_activity(&mut state);
            true
        }
    };
    if !has_active {
        return;
    }
    widgets.save_status.set_label("Saving…");
    if let Some(source) = pending.borrow_mut().body.take() {
        source.remove();
    }
    let delay = state.borrow().config.autosave_delay_ms.clamp(500, 1_000);
    let state_for_save = state.clone();
    let widgets_for_save = widgets.clone();
    let pending_for_save = pending.clone();
    let session = widgets.sessions.current();
    let source = glib::timeout_add_local_once(Duration::from_millis(delay), move || {
        pending_for_save.borrow_mut().body.take();
        // A vault switch since this timer was armed makes it stale.
        if !widgets_for_save.sessions.is_current(session) {
            return;
        }
        persist_active(&state_for_save, &widgets_for_save, false);
    });
    pending.borrow_mut().body = Some(source);
}

fn schedule_title_commit(
    state: &Rc<RefCell<AppState>>,
    widgets: &Widgets,
    pending: &Rc<RefCell<PendingSaves>>,
) {
    if let Some(source) = pending.borrow_mut().title.take() {
        source.remove();
    }
    if state.borrow().read_only {
        return;
    }
    let delay = state
        .borrow()
        .config
        .title_commit_delay_ms
        .clamp(1_000, 5_000);
    let state_for_save = state.clone();
    let widgets_for_save = widgets.clone();
    let pending_for_save = pending.clone();
    let session = widgets.sessions.current();
    let source = glib::timeout_add_local_once(Duration::from_millis(delay), move || {
        pending_for_save.borrow_mut().title.take();
        if !widgets_for_save.sessions.is_current(session) {
            return;
        }
        persist_active(&state_for_save, &widgets_for_save, true);
    });
    pending.borrow_mut().title = Some(source);
}

fn cancel_title_timer(pending: &Rc<RefCell<PendingSaves>>) {
    if let Some(source) = pending.borrow_mut().title.take() {
        source.remove();
    }
}

fn cancel_all_timers(pending: &Rc<RefCell<PendingSaves>>) {
    let mut pending = pending.borrow_mut();
    if let Some(source) = pending.body.take() {
        source.remove();
    }
    if let Some(source) = pending.title.take() {
        source.remove();
    }
}

fn prepare_to_leave_active(
    state: &Rc<RefCell<AppState>>,
    widgets: &Widgets,
    pending: &Rc<RefCell<PendingSaves>>,
) -> bool {
    cancel_all_timers(pending);
    if !persist_active(state, widgets, true) {
        return false;
    }
    stash_or_lock_active(state);
    true
}

fn stash_or_lock_active(state: &Rc<RefCell<AppState>>) {
    let mut state = state.borrow_mut();
    let clean = !state.body_dirty && !state.title_dirty;
    let Some(mut active) = state.active.take() else {
        return;
    };
    if active.is_encrypted() {
        if state.config.encrypted_note_locking.on_note_switch {
            active.clear_sensitive();
        } else {
            state.unlocked_cache.insert(active.id(), active);
        }
        return;
    }
    // Only a clean copy is safe to reuse; a dirty note was either just persisted
    // (flags cleared) or the caller aborted the switch and kept editing.
    if clean && let ActiveDocument::Plain { note, stamp } = active {
        if state.plain_cache.len() >= PLAIN_CACHE_LIMIT {
            state.plain_cache.clear();
        }
        state.plain_cache.insert(note.metadata.id, (note, stamp));
    } else {
        active.clear_sensitive();
    }
}

/// Records "sensitive activity just happened" for the auto-lock timer, when
/// either the open note is an encrypted `.snote` or the whole vault is an
/// unlocked encrypted vault (in which case even a plaintext `.md` note in the
/// editor is sensitive at rest).
fn touch_sensitive_activity(state: &mut AppState) {
    let sensitive = state
        .active
        .as_ref()
        .is_some_and(ActiveDocument::is_encrypted)
        || state
            .vault
            .as_ref()
            .is_some_and(|vault| vault.is_encrypted() && !vault.is_locked());
    if sensitive {
        state.last_sensitive_activity = Some(Instant::now());
    }
}

fn clear_sensitive_documents(state: &Rc<RefCell<AppState>>) {
    let mut state = state.borrow_mut();
    if let Some(mut active) = state.active.take() {
        active.clear_sensitive();
    }
    for (_, mut document) in state.unlocked_cache.drain() {
        document.clear_sensitive();
    }
    state.plain_cache.clear();
    state.last_sensitive_activity = None;
}

fn persist_active(state: &Rc<RefCell<AppState>>, widgets: &Widgets, commit_title: bool) -> bool {
    let needs_body = state.borrow().body_dirty;
    let needs_title = commit_title && state.borrow().title_dirty;
    if !needs_body && !needs_title {
        return true;
    }
    let body = if needs_body {
        let start = widgets.buffer.start_iter();
        let end = widgets.buffer.end_iter();
        Some(widgets.buffer.text(&start, &end, true).to_string())
    } else {
        None
    };
    let title = needs_title.then(|| {
        let draft = state.borrow().title_draft.trim().to_owned();
        if draft.is_empty() {
            "Untitled".into()
        } else {
            draft
        }
    });

    let result = {
        let mut state = state.borrow_mut();
        let Some(vault) = state.vault.clone() else {
            return false;
        };
        let Some(active) = state.active.as_mut() else {
            return false;
        };
        if let Some(body) = body {
            match active {
                ActiveDocument::Plain { note, .. } | ActiveDocument::Encrypted { note, .. } => {
                    note.body = body
                }
            }
        }
        match active {
            ActiveDocument::Plain { note, stamp } => {
                let expected = stamp.clone();
                let save = match title.as_deref() {
                    Some(title) => vault.commit_title(note, Some(&expected), title),
                    None => vault.save_note(note, Some(&expected)),
                };
                match save {
                    Ok(next) => {
                        *stamp = next;
                        Ok(())
                    }
                    Err(error) => {
                        let recovery = vault.write_recovery(note);
                        Err((error, recovery.is_ok(), false))
                    }
                }
            }
            ActiveDocument::Encrypted {
                note,
                stamp,
                session,
            } => {
                if let Some(title) = title.as_deref() {
                    note.metadata.title = title.into();
                }
                let expected = stamp.clone();
                match vault.save_encrypted_note(note, session, Some(&expected)) {
                    Ok(next) => {
                        *stamp = next;
                        Ok(())
                    }
                    Err(error) => Err((error, false, true)),
                }
            }
        }
    };

    match result {
        Ok(()) => {
            {
                let mut state = state.borrow_mut();
                if needs_body {
                    state.body_dirty = false;
                }
                if needs_title {
                    state.title_dirty = false;
                }
            }
            update_active_summary(state, widgets);
            refresh_watch_baseline(state);
            let encrypted = {
                state
                    .borrow()
                    .active
                    .as_ref()
                    .is_some_and(ActiveDocument::is_encrypted)
            };
            widgets.save_status.set_label(if encrypted {
                "Saved · encrypted at rest"
            } else {
                "Saved"
            });
            if needs_title && widgets.title.text().trim().is_empty() {
                let _editor_guard = widgets.editor_events.suppress();
                widgets.title.set_text("Untitled");
            }
            true
        }
        Err((error, recovery_ok, encrypted)) => {
            let detail = if encrypted {
                " No plaintext recovery file was written; keep the app open and copy the text if necessary."
            } else if recovery_ok {
                " A private local recovery copy was preserved."
            } else {
                " The recovery copy also failed; copy your text before closing."
            };
            widgets
                .save_status
                .set_label(&format!("Save failed: {error}.{detail}"));
            false
        }
    }
}

fn update_active_summary(state: &Rc<RefCell<AppState>>, widgets: &Widgets) {
    let active_snapshot = {
        state.borrow().active.as_ref().map(|active| {
            let note = active.note();
            (
                note.metadata.id,
                note.metadata.title.clone(),
                note.body.clone(),
                note.relative_path.clone(),
                note.metadata.pinned,
                note.metadata.favourite,
                note.metadata.archived,
                note.metadata.created_at,
                note.metadata.updated_at,
                note.metadata.tags.clone(),
            )
        })
    };
    let Some((id, title, body, path, pinned, favourite, archived, created_at, updated_at, tags)) =
        active_snapshot
    else {
        return;
    };
    // This function only ever runs against `state.active`, which - for an
    // encrypted note - only ever holds it while unlocked. Its real,
    // already-decrypted body is exactly as available as the plaintext
    // case, so the preview is computed the same way for both; the summary
    // this function updates is a purely in-memory Vec (never written to
    // disk or cache), so showing it here never creates a plaintext side
    // channel - it only reflects what is already decrypted and displayed
    // in the open editor. See the "Locked encrypted notes" note in
    // `SECURITY.md`.
    let preview_limit = { state.borrow().config.appearance.note_preview_length };
    let preview = truncate_preview(&body, preview_limit);
    if let Some(summary) = state
        .borrow_mut()
        .notes
        .iter_mut()
        .find(|note| note.id == id)
    {
        summary.relative_path = path;
        summary.pinned = pinned;
        summary.favourite = favourite;
        summary.archived = archived;
        summary.created_at = created_at;
        summary.updated_at = updated_at;
        // The note is open and decrypted, so its true protected metadata is
        // known again - this is the one place a locked summary transitions
        // back to unlocked (the reverse happens in `lock_all_encrypted`,
        // which resets every field this function sets back to the exact
        // `NoteSummary::locked()` placeholder).
        summary.locked = false;
        summary.title = title.clone();
        summary.preview = preview.clone();
        summary.body = body.clone();
        summary.tags = tags.clone();
    }
    let row_widgets = { widgets.row_widgets.borrow().get(&id).cloned() };
    if let Some(row_widgets) = row_widgets {
        row_widgets.pin.set_visible(pinned);
        row_widgets.favourite.set_visible(favourite);
        row_widgets.archived.set_visible(archived);
        row_widgets.title.set_label(&title);
        row_widgets.preview.set_label(&preview);
    }
}

fn refresh_current_view(state: &Rc<RefCell<AppState>>, widgets: &Widgets) -> bool {
    let (vault, mode) = {
        let state = state.borrow();
        (state.vault.clone(), state.flow.view().clone())
    };
    let Some(vault) = vault else {
        return false;
    };
    let result = match mode {
        ViewMode::Trash => vault.scan_trash().map(|trash| (None, Some(trash))),
        _ => vault.scan_notes().map(|notes| (Some(notes), None)),
    };
    let (notes, trash) = match result {
        Ok(result) => result,
        Err(error) => {
            widgets
                .save_status
                .set_label(&format!("Could not refresh notes: {error}"));
            return false;
        }
    };
    {
        let mut state = state.borrow_mut();
        if let Some(notes) = notes {
            state.notes = notes;
        }
        if let Some(trash) = trash {
            state.trash = trash;
        }
    }
    render_note_list(state, widgets);
    true
}

fn render_note_list(state: &Rc<RefCell<AppState>>, widgets: &Widgets) {
    let query = widgets.search.text().to_string();
    let (mode, selected_note, selected_trash, density, notes, trash) = {
        let state = state.borrow();
        (
            state.flow.view().clone(),
            state.flow.selected_note(),
            state.flow.selected_trash(),
            state.config.appearance.note_list_density,
            filtered_notes(&state, &query),
            filtered_trash(&state, &query),
        )
    };
    let targets: Vec<RowTarget> = match mode {
        ViewMode::Trash => trash
            .iter()
            .map(|entry| RowTarget::Trash(entry.id))
            .collect(),
        _ => notes
            .iter()
            .map(|summary| RowTarget::Note(summary.id))
            .collect(),
    };
    let has_rows = !targets.is_empty();
    let _selection_guard = widgets.selection.suppress();
    widgets.selection.replace_rows(targets);
    // Remove rows by index, never by child iteration: the shared context-menu
    // popover is also a child of the list and must not be touched here.
    while let Some(row) = widgets.note_list.row_at_index(0) {
        widgets.note_list.remove(&row);
    }
    widgets.row_widgets.borrow_mut().clear();
    match mode {
        ViewMode::Trash => {
            for entry in trash {
                let subtitle = if entry.encrypted {
                    "Encrypted note".into()
                } else {
                    format!("From {}", entry.original_relative_path.display())
                };
                let (row, row_widgets) = note_row(
                    NoteRowSpec {
                        title: &entry.title,
                        preview: &subtitle,
                        encrypted: entry.encrypted,
                        pinned: false,
                        favourite: false,
                        archived: false,
                        density,
                    },
                    trash_context_menu(entry.id),
                    &widgets.row_menu,
                );
                widgets
                    .row_widgets
                    .borrow_mut()
                    .insert(entry.id, row_widgets);
                widgets.note_list.append(&row);
            }
        }
        _ => {
            for summary in notes {
                let (row, row_widgets) = note_row(
                    NoteRowSpec {
                        title: &summary.title,
                        preview: &summary.preview,
                        encrypted: summary.encrypted,
                        pinned: summary.pinned,
                        favourite: summary.favourite,
                        archived: summary.archived,
                        density,
                    },
                    note_context_menu(
                        summary.id,
                        summary.pinned,
                        summary.archived,
                        summary.encrypted,
                    ),
                    &widgets.row_menu,
                );
                widgets
                    .row_widgets
                    .borrow_mut()
                    .insert(summary.id, row_widgets);
                widgets.note_list.append(&row);
            }
        }
    }
    let selected = match mode {
        ViewMode::Trash => {
            selected_trash.and_then(|id| widgets.selection.index_of(RowTarget::Trash(id)))
        }
        _ => selected_note.and_then(|id| widgets.selection.index_of(RowTarget::Note(id))),
    };
    if let Some(index) = selected
        && let Some(row) = widgets.note_list.row_at_index(index as i32)
    {
        widgets.note_list.select_row(Some(&row));
    }
    if has_rows {
        widgets.note_list_stack.set_visible_child_name("list");
    } else {
        let searching = !query.trim().is_empty();
        let (title, copy) = if searching {
            ("No matches", "Try a different search.")
        } else {
            match mode {
                ViewMode::AllNotes => ("No notes yet", "Create a new note to start writing."),
                ViewMode::Notebook(_) => (
                    "No notes here",
                    "New notes you create here appear in this notebook.",
                ),
                ViewMode::RecentlyOpened => {
                    ("Nothing opened yet", "Notes you open will appear here.")
                }
                ViewMode::Favourites => (
                    "No favourites yet",
                    "Mark a note with the star to see it here.",
                ),
                ViewMode::Pinned => ("No pinned notes", "Pin a note to see it here."),
                ViewMode::Archive => (
                    "Nothing archived",
                    "Archive a note to remove it from your day-to-day views.",
                ),
                ViewMode::EncryptedNotes => (
                    "No encrypted notes",
                    "Encrypt a note with its own password to see it here.",
                ),
                ViewMode::Trash => ("Trash is empty", "Deleted notes will appear here."),
            }
        };
        widgets.note_list_empty_title.set_label(title);
        widgets.note_list_empty_copy.set_label(copy);
        widgets.note_list_stack.set_visible_child_name("empty");
    }
}

fn insert_note_row(
    index: usize,
    summary: &NoteSummary,
    state: &Rc<RefCell<AppState>>,
    widgets: &Widgets,
) {
    let density = { state.borrow().config.appearance.note_list_density };
    let (row, row_widgets) = note_row(
        NoteRowSpec {
            title: &summary.title,
            preview: &summary.preview,
            encrypted: summary.encrypted,
            pinned: summary.pinned,
            favourite: summary.favourite,
            archived: summary.archived,
            density,
        },
        note_context_menu(
            summary.id,
            summary.pinned,
            summary.archived,
            summary.encrypted,
        ),
        &widgets.row_menu,
    );
    let _selection_guard = widgets.selection.suppress();
    widgets
        .selection
        .insert_row(index, RowTarget::Note(summary.id));
    widgets.note_list.insert(&row, index as i32);
    widgets.note_list_stack.set_visible_child_name("list");
    widgets
        .row_widgets
        .borrow_mut()
        .insert(summary.id, row_widgets);
}

fn replace_note_row(summary: &NoteSummary, state: &Rc<RefCell<AppState>>, widgets: &Widgets) {
    let target = RowTarget::Note(summary.id);
    let Some(index) = widgets.selection.index_of(target) else {
        return;
    };
    let desired_index = {
        let query = widgets.search.text().to_string();
        let state = state.borrow();
        filtered_notes(&state, &query)
            .iter()
            .position(|candidate| candidate.id == summary.id)
            .unwrap_or(index)
    };
    let was_selected = selected_row_target(widgets) == Some(target);
    let _selection_guard = widgets.selection.suppress();
    if let Some(row) = widgets.note_list.row_at_index(index as i32) {
        widgets.note_list.remove(&row);
    }
    widgets.selection.remove_row(target);
    widgets.row_widgets.borrow_mut().remove(&summary.id);
    drop(_selection_guard);
    insert_note_row(desired_index, summary, state, widgets);
    if was_selected {
        select_row_target(target, widgets);
    }
}

fn remove_row_target(target: RowTarget, widgets: &Widgets) -> Option<usize> {
    let index = widgets.selection.index_of(target)?;
    let _selection_guard = widgets.selection.suppress();
    if let Some(row) = widgets.note_list.row_at_index(index as i32) {
        widgets.note_list.remove(&row);
    }
    widgets.selection.remove_row(target);
    let id = match target {
        RowTarget::Note(id) | RowTarget::Trash(id) => id,
    };
    widgets.row_widgets.borrow_mut().remove(&id);
    if widgets.selection.target_at(0).is_none() {
        let (title, copy) = match target {
            RowTarget::Note(_) => ("No notes here", "Create a new note to start writing."),
            RowTarget::Trash(_) => ("Trash is empty", "Deleted notes will appear here."),
        };
        widgets.note_list_empty_title.set_label(title);
        widgets.note_list_empty_copy.set_label(copy);
        widgets.note_list_stack.set_visible_child_name("empty");
    }
    Some(index)
}

/// Rebuilds the dynamic notebook list from the vault (everything except
/// `Inbox`, which has its own fixed sidebar row). Called on vault open and
/// after any notebook create/rename/delete or note move.
fn render_notebook_list(state: &Rc<RefCell<AppState>>, widgets: &Widgets) {
    let vault = { state.borrow().vault.clone() };
    let Some(vault) = vault else {
        return;
    };
    let notebooks = match vault.list_notebooks() {
        Ok(notebooks) => notebooks,
        Err(error) => {
            widgets
                .save_status
                .set_label(&format!("Could not list notebooks: {error}"));
            return;
        }
    };
    let notebooks: Vec<NotebookEntry> = notebooks
        .into_iter()
        .filter(|entry| entry.relative_path != Path::new("Inbox"))
        .collect();

    let _guard = widgets.notebook_events.suppress();
    while let Some(row) = widgets.notebook_list.row_at_index(0) {
        widgets.notebook_list.remove(&row);
    }
    let mut rows = Vec::with_capacity(notebooks.len());
    for notebook in &notebooks {
        let row = notebook_row(notebook, &widgets.notebook_menu);
        widgets.notebook_list.append(&row);
        rows.push(notebook.relative_path.clone());
    }
    *widgets.notebook_rows.borrow_mut() = rows;
    drop(_guard);
    let current_view = { state.borrow().flow.view().clone() };
    update_notebook_list_selection(&current_view, widgets);
}

fn notebook_row(notebook: &NotebookEntry, notebook_menu: &gtk::PopoverMenu) -> gtk::ListBoxRow {
    let depth = notebook
        .relative_path
        .components()
        .count()
        .saturating_sub(1);
    let name = notebook
        .relative_path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("Notebook");
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    content.set_margin_start(8 + (depth as i32) * 14);
    content.set_margin_end(8);
    content.set_margin_top(6);
    content.set_margin_bottom(6);
    let icon = gtk::Image::from_icon_name("folder-symbolic");
    icon.set_pixel_size(14);
    content.append(&icon);
    let label = gtk::Label::new(Some(name));
    label.set_xalign(0.0);
    label.set_hexpand(true);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    content.append(&label);
    let row = gtk::ListBoxRow::new();
    row.set_child(Some(&content));
    row.update_property(&[gtk::accessible::Property::Label(&format!(
        "Notebook: {name}"
    ))]);

    // Same shared-popover convention as `note_row`: the popover is owned by
    // the list, not by each row, so a removed row finalizes cleanly.
    let menu = notebook_context_menu(&notebook.relative_path);
    let gesture = gtk::GestureClick::new();
    gesture.set_button(3);
    let notebook_menu = notebook_menu.clone();
    let anchor = row.downgrade();
    gesture.connect_pressed(move |gesture, _, x, y| {
        gesture.set_state(gtk::EventSequenceState::Claimed);
        let Some(anchor) = anchor.upgrade() else {
            return;
        };
        let Some(list) = notebook_menu.parent() else {
            return;
        };
        notebook_menu.set_menu_model(Some(&menu));
        if let Some(point) =
            anchor.compute_point(&list, &gtk::graphene::Point::new(x as f32, y as f32))
        {
            notebook_menu.set_pointing_to(Some(&gdk::Rectangle::new(
                point.x() as i32,
                point.y() as i32,
                1,
                1,
            )));
        }
        notebook_menu.popup();
    });
    row.add_controller(gesture);
    row
}

fn notebook_context_menu(relative_path: &Path) -> gio::Menu {
    let path = relative_path.to_string_lossy().to_string();
    let menu = gio::Menu::new();
    append_path_targeted_menu_item(
        &menu,
        "New Child Notebook…",
        "app.new-child-notebook",
        &path,
    );
    append_path_targeted_menu_item(&menu, "Rename…", "app.rename-notebook", &path);
    append_path_targeted_menu_item(&menu, "Delete…", "app.delete-notebook", &path);
    menu
}

fn append_path_targeted_menu_item(menu: &gio::Menu, label: &str, action: &str, path: &str) {
    let item = gio::MenuItem::new(Some(label), None);
    item.set_action_and_target_value(Some(action), Some(&path.to_variant()));
    menu.append_item(&item);
}

fn update_notebook_list_selection(mode: &ViewMode, widgets: &Widgets) {
    let _guard = widgets.notebook_events.suppress();
    let inbox = Path::new("Inbox");
    if let ViewMode::Notebook(path) = mode
        && path.as_path() != inbox
    {
        let index = widgets
            .notebook_rows
            .borrow()
            .iter()
            .position(|candidate| candidate == path);
        if let Some(index) = index
            && let Some(row) = widgets.notebook_list.row_at_index(index as i32)
        {
            widgets.notebook_list.select_row(Some(&row));
            return;
        }
    }
    widgets.notebook_list.unselect_all();
}

/// Rebuilds the sidebar tag-filter chips from every distinct tag across
/// `state.notes` (locked notes contribute none - their tags are always
/// empty, see `NoteSummary::locked`).
fn render_tags_list(state: &Rc<RefCell<AppState>>, widgets: &Widgets) {
    let (mut tags, active_tag): (Vec<String>, Option<String>) = {
        let state = state.borrow();
        let tags = state
            .notes
            .iter()
            .flat_map(|summary| summary.tags.iter().cloned())
            .collect();
        (tags, state.filter.active_tag().map(str::to_string))
    };
    tags.sort_by_key(|tag| tag.to_lowercase());
    tags.dedup_by(|left, right| left.eq_ignore_ascii_case(right));

    let _guard = widgets.tags_events.suppress();
    while let Some(child) = widgets.tags_flow.first_child() {
        widgets.tags_flow.remove(&child);
    }
    for tag in tags {
        let button = gtk::ToggleButton::with_label(&tag);
        button.add_css_class("tag-filter-chip");
        button.set_active(active_tag.as_deref() == Some(tag.as_str()));
        let state = state.clone();
        let widgets_for_click = widgets.clone();
        let tag_for_click = tag.clone();
        button.connect_toggled(move |button| {
            if widgets_for_click.tags_events.is_suppressed() {
                return;
            }
            {
                let mut state = state.borrow_mut();
                if button.is_active() {
                    state.filter.set_active_tag(Some(tag_for_click.clone()));
                } else {
                    state.filter.clear();
                }
            }
            render_note_list(&state, &widgets_for_click);
            render_tags_list(&state, &widgets_for_click);
        });
        widgets.tags_flow.insert(&button, -1);
    }
}

/// Opens a small text-entry dialog to create a notebook. `parent` is the
/// notebook it will be nested under, or `None` for a new top-level notebook.
fn present_new_notebook_dialog(
    parent: Option<PathBuf>,
    state: &Rc<RefCell<AppState>>,
    widgets: &Widgets,
) {
    let heading = if parent.is_some() {
        "New Child Notebook"
    } else {
        "New Notebook"
    };
    let state = state.clone();
    let widgets_for_create = widgets.clone();
    present_text_entry_dialog(
        &widgets.window,
        heading,
        "",
        "Notebook name",
        "",
        "Create",
        move |name| {
            let Some(name) = name else {
                return;
            };
            let Ok(name) = senatorial_notes::paths::validate_notebook_name(&name) else {
                widgets_for_create
                    .save_status
                    .set_label("Notebook names can't be empty or contain a path separator.");
                return;
            };
            let vault = { state.borrow().vault.clone() };
            let Some(vault) = vault else {
                return;
            };
            let relative = parent.map_or_else(|| PathBuf::from(&name), |parent| parent.join(&name));
            match vault.create_notebook(&relative) {
                Ok(_) => {
                    render_notebook_list(&state, &widgets_for_create);
                    widgets_for_create.save_status.set_label("Notebook created");
                }
                Err(error) => widgets_for_create
                    .save_status
                    .set_label(&format!("Could not create notebook: {error}")),
            }
        },
    );
}

fn present_rename_notebook_dialog(
    relative: PathBuf,
    state: &Rc<RefCell<AppState>>,
    widgets: &Widgets,
) {
    let current_name = relative
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_string();
    let state = state.clone();
    let widgets_for_rename = widgets.clone();
    let relative_for_rename = relative.clone();
    present_text_entry_dialog(
        &widgets.window,
        "Rename Notebook",
        "",
        "Notebook name",
        &current_name,
        "Rename",
        move |name| {
            let Some(name) = name else {
                return;
            };
            // Flush any pending edit to the OLD path first: every descendant
            // note's path is about to change, and a stray autosave/title-
            // commit timer must never fire against a path that is about to
            // vanish out from under it.
            if !persist_active(&state, &widgets_for_rename, true) {
                return;
            }
            let vault = { state.borrow().vault.clone() };
            let Some(vault) = vault else {
                return;
            };
            match vault.rename_notebook(&relative_for_rename, &name) {
                Ok(next_relative) => {
                    let was_active_view = {
                        state.borrow().flow.view()
                            == &ViewMode::Notebook(relative_for_rename.clone())
                    };
                    if was_active_view {
                        state
                            .borrow_mut()
                            .flow
                            .switch_view(ViewMode::Notebook(next_relative.clone()));
                    }
                    // The active note's own path is not covered by the
                    // scan-and-merge in `refresh_after_watcher` unless it is
                    // passed as `preserve_active_id` - do that whenever the
                    // active note is inside the renamed notebook (or one of
                    // its descendants), so its relative_path is corrected
                    // without its in-memory title/body draft being clobbered
                    // by the rescan.
                    let affected_active_id = {
                        let state = state.borrow();
                        state.active.as_ref().and_then(|active| {
                            active
                                .note()
                                .relative_path
                                .starts_with(&relative_for_rename)
                                .then(|| active.id())
                        })
                    };
                    refresh_after_watcher(&state, &widgets_for_rename, affected_active_id);
                    refresh_watch_baseline(&state);
                    render_notebook_list(&state, &widgets_for_rename);
                    let mode = { state.borrow().flow.view().clone() };
                    apply_view_chrome(&mode, &widgets_for_rename);
                    widgets_for_rename.save_status.set_label("Notebook renamed");
                }
                Err(error) => widgets_for_rename
                    .save_status
                    .set_label(&format!("Could not rename notebook: {error}")),
            }
        },
    );
}

fn confirm_delete_notebook(relative: PathBuf, state: &Rc<RefCell<AppState>>, widgets: &Widgets) {
    let name = relative
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("this notebook");
    let dialog = gtk::AlertDialog::builder()
        .modal(true)
        .message(format!("Delete the notebook \"{name}\"?"))
        .detail("This only works if the notebook is completely empty. Move, archive, or delete its notes first.")
        .buttons(["Cancel", "Delete"])
        .cancel_button(0)
        .default_button(0)
        .build();
    let state = state.clone();
    let widgets = widgets.clone();
    let parent = widgets.window.clone();
    dialog.choose(Some(&parent), None::<&gio::Cancellable>, move |result| {
        if result != Ok(1) {
            return;
        }
        let vault = { state.borrow().vault.clone() };
        let Some(vault) = vault else {
            return;
        };
        match vault.delete_notebook(&relative) {
            Ok(()) => {
                render_notebook_list(&state, &widgets);
                widgets.save_status.set_label("Notebook deleted");
            }
            Err(error) => widgets
                .save_status
                .set_label(&format!("Could not delete notebook: {error}")),
        }
    });
}

fn select_row_target(target: RowTarget, widgets: &Widgets) {
    let Some(index) = widgets.selection.index_of(target) else {
        return;
    };
    let _selection_guard = widgets.selection.suppress();
    if let Some(row) = widgets.note_list.row_at_index(index as i32) {
        // Skip a redundant select_row: it still emits row-selected churn even
        // when the row is already the selected one.
        if widgets.note_list.selected_row().as_ref() != Some(&row) {
            widgets.note_list.select_row(Some(&row));
        }
    }
}

fn selected_row_target(widgets: &Widgets) -> Option<RowTarget> {
    widgets
        .note_list
        .selected_row()
        .and_then(|row| widgets.selection.target_at(row.index()))
}

/// Moves the list selection by `direction` rows (`1` for next, `-1` for
/// previous) and loads whatever it lands on. Works for both the note list
/// and Trash, since it only depends on `widgets.selection`/`RowTarget`. If
/// nothing is selected yet, selects the first row instead of moving.
fn select_adjacent_note(direction: i32, state: &Rc<RefCell<AppState>>, widgets: &Widgets) {
    let target =
        match selected_row_target(widgets).and_then(|target| widgets.selection.index_of(target)) {
            Some(current_index) => {
                let next_index = current_index as i32 + direction;
                usize::try_from(next_index)
                    .ok()
                    .and_then(|index| widgets.selection.target_at(index as i32))
            }
            None => widgets.selection.target_at(0),
        };
    let Some(target) = target else {
        return;
    };
    select_row_target(target, widgets);
    match target {
        RowTarget::Note(id) => load_note_by_id(id, state, widgets),
        RowTarget::Trash(id) => show_trash_by_id(id, state, widgets),
    }
}

/// The note the keyboard-driven Pin/Archive/Note Information actions should
/// act on: the currently open note, or - for a locked encrypted note, which
/// is never `active` - whatever is selected in the list.
fn current_note_id(state: &Rc<RefCell<AppState>>) -> Option<Uuid> {
    let state = state.borrow();
    state
        .active
        .as_ref()
        .map(ActiveDocument::id)
        .or_else(|| state.flow.selected_note())
}

fn select_adjacent_after_removal(
    removed_index: usize,
    state: &Rc<RefCell<AppState>>,
    widgets: &Widgets,
) {
    let next = widgets
        .selection
        .target_at(removed_index as i32)
        .or_else(|| {
            removed_index
                .checked_sub(1)
                .and_then(|index| widgets.selection.target_at(index as i32))
        });
    let Some(target) = next else {
        clear_editor(state, widgets);
        return;
    };
    select_row_target(target, widgets);
    match target {
        RowTarget::Note(id) => select_note_without_prompting_if_locked(id, state, widgets),
        RowTarget::Trash(id) => show_trash_by_id(id, state, widgets),
    }
}

struct NoteRowSpec<'a> {
    title: &'a str,
    preview: &'a str,
    encrypted: bool,
    pinned: bool,
    favourite: bool,
    archived: bool,
    density: NoteListDensity,
}

fn note_row(
    spec: NoteRowSpec<'_>,
    menu: gio::Menu,
    row_menu: &gtk::PopoverMenu,
) -> (gtk::ListBoxRow, RowWidgets) {
    let NoteRowSpec {
        title: title_text,
        preview: preview_text,
        encrypted,
        pinned,
        favourite,
        archived,
        density,
    } = spec;
    let row_content = gtk::Box::new(gtk::Orientation::Vertical, 4);
    row_content.add_css_class("note-card");
    let vertical_margin = match density {
        NoteListDensity::Compact => 7,
        NoteListDensity::Comfortable => 11,
        NoteListDensity::Spacious => 16,
    };
    row_content.set_margin_start(12);
    row_content.set_margin_end(12);
    row_content.set_margin_top(vertical_margin);
    row_content.set_margin_bottom(vertical_margin);
    let title_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    if encrypted {
        let lock = gtk::Image::from_icon_name("changes-prevent-symbolic");
        lock.set_pixel_size(14);
        lock.add_css_class("brand-accent");
        title_row.append(&lock);
    }
    let favourite_icon = gtk::Image::from_icon_name("starred-symbolic");
    favourite_icon.set_pixel_size(14);
    favourite_icon.set_visible(favourite);
    favourite_icon.add_css_class("brand-accent");
    favourite_icon.set_tooltip_text(Some("Favourite"));
    title_row.append(&favourite_icon);
    let pin = gtk::Image::from_icon_name("view-pin-symbolic");
    pin.set_pixel_size(14);
    pin.set_visible(pinned);
    pin.set_tooltip_text(Some("Pinned note"));
    title_row.append(&pin);
    let archived_icon = gtk::Image::from_icon_name("folder-symbolic");
    archived_icon.set_pixel_size(14);
    archived_icon.set_visible(archived);
    archived_icon.set_tooltip_text(Some("Archived note"));
    title_row.append(&archived_icon);
    let title = gtk::Label::new(Some(title_text));
    title.set_xalign(0.0);
    title.set_hexpand(true);
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    title.add_css_class("heading");
    title_row.append(&title);
    let preview = gtk::Label::new(Some(preview_text));
    preview.set_xalign(0.0);
    preview.set_ellipsize(gtk::pango::EllipsizeMode::End);
    preview.add_css_class("caption");
    preview.add_css_class("dim-label");
    row_content.append(&title_row);
    row_content.append(&preview);
    let row = gtk::ListBoxRow::new();
    row.set_activatable(true);
    row.set_child(Some(&row_content));
    row.update_property(&[gtk::accessible::Property::Label(&format!(
        "Note: {title_text}"
    ))]);

    // One shared PopoverMenu is owned by the note list, not by each row. A
    // per-row popover keeps the row alive as its parent and, when the row is
    // removed, GTK finalizes a ListBoxRow that still has a child ("Finalizing
    // GtkListBoxRow, but it still has children left"). The gesture only holds a
    // weak reference to its row so removed rows finalize cleanly.
    let gesture = gtk::GestureClick::new();
    gesture.set_button(3);
    let row_menu = row_menu.clone();
    let anchor = row.downgrade();
    gesture.connect_pressed(move |gesture, _, x, y| {
        gesture.set_state(gtk::EventSequenceState::Claimed);
        let Some(anchor) = anchor.upgrade() else {
            return;
        };
        let Some(list) = row_menu.parent() else {
            return;
        };
        row_menu.set_menu_model(Some(&menu));
        if let Some(point) =
            anchor.compute_point(&list, &gtk::graphene::Point::new(x as f32, y as f32))
        {
            row_menu.set_pointing_to(Some(&gdk::Rectangle::new(
                point.x() as i32,
                point.y() as i32,
                1,
                1,
            )));
        }
        row_menu.popup();
    });
    row.add_controller(gesture);
    (
        row,
        RowWidgets {
            title,
            preview,
            pin,
            favourite: favourite_icon,
            archived: archived_icon,
        },
    )
}

fn note_context_menu(id: Uuid, pinned: bool, archived: bool, encrypted: bool) -> gio::Menu {
    let menu = gio::Menu::new();
    append_targeted_menu_item(&menu, "Rename", "app.context-rename", id);
    append_targeted_menu_item(
        &menu,
        if pinned { "Unpin" } else { "Pin" },
        "app.context-toggle-pin",
        id,
    );
    append_targeted_menu_item(
        &menu,
        "Toggle Favourite",
        "app.context-toggle-favourite",
        id,
    );
    append_targeted_menu_item(
        &menu,
        if archived { "Unarchive" } else { "Archive" },
        "app.context-toggle-archived",
        id,
    );
    append_targeted_menu_item(
        &menu,
        "Move to Notebook…",
        "app.context-move-to-notebook",
        id,
    );
    if !encrypted {
        append_targeted_menu_item(&menu, "Encrypt Note…", "app.context-encrypt", id);
    }
    append_targeted_menu_item(&menu, "Note Information", "app.context-note-info", id);
    append_targeted_menu_item(&menu, "Move to Trash", "app.context-move-to-trash", id);
    menu
}

fn trash_context_menu(id: Uuid) -> gio::Menu {
    let menu = gio::Menu::new();
    append_targeted_menu_item(&menu, "Restore", "app.context-restore", id);
    append_targeted_menu_item(
        &menu,
        "Permanently Delete…",
        "app.context-permanently-delete",
        id,
    );
    menu
}

fn append_targeted_menu_item(menu: &gio::Menu, label: &str, action: &str, id: Uuid) {
    let item = gio::MenuItem::new(Some(label), None);
    item.set_action_and_target_value(Some(action), Some(&id.to_string().to_variant()));
    menu.append_item(&item);
}

/// Notes visible for the currently active view, tag filter, and search
/// query, in the user's chosen sort order. Handles every `ViewMode` that
/// shows notes (everything but `Trash`, which has its own list and its own
/// `filtered_trash`).
fn filtered_notes(state: &AppState, query: &str) -> Vec<NoteSummary> {
    let view = state.flow.view();
    let tag = state.filter.active_tag();
    let recently_opened = &state.session.recently_opened;
    let mut notes: Vec<NoteSummary> = state
        .notes
        .iter()
        .filter(|summary| view_includes(view, summary, recently_opened))
        .filter(|summary| {
            tag.is_none_or(|tag| {
                summary
                    .tags
                    .iter()
                    .any(|candidate| candidate.eq_ignore_ascii_case(tag))
            })
        })
        .filter(|summary| summary_matches(summary, query))
        .cloned()
        .collect();
    match view {
        ViewMode::RecentlyOpened => {
            // Order follows the recently-opened list (most-recent first),
            // never the user's general sort preference.
            let rank = |id: &Uuid| {
                recently_opened
                    .iter()
                    .position(|candidate| candidate == id)
                    .unwrap_or(usize::MAX)
            };
            notes.sort_by(|a, b| rank(&a.id).cmp(&rank(&b.id)).then(a.id.cmp(&b.id)));
        }
        _ => sort_notes(&mut notes, state.config.sort_order),
    }
    notes
}

/// Whether `summary` belongs in `view`. Every "day-to-day" view except
/// Archive excludes archived notes. Notebook membership is exact - a notebook
/// shows only notes directly inside it, never descendants of nested
/// notebooks.
fn view_includes(view: &ViewMode, summary: &NoteSummary, recently_opened: &[Uuid]) -> bool {
    match view {
        ViewMode::AllNotes => !summary.archived,
        ViewMode::Notebook(path) => {
            !summary.archived && summary.relative_path.parent() == Some(path.as_path())
        }
        ViewMode::RecentlyOpened => {
            !summary.archived && !summary.locked && recently_opened.contains(&summary.id)
        }
        ViewMode::Favourites => !summary.archived && summary.favourite,
        ViewMode::Pinned => !summary.archived && summary.pinned,
        ViewMode::Archive => summary.archived,
        // Individually encrypted notes, archived or not - this is a "find my
        // note-password-protected notes" view, not a day-to-day one.
        ViewMode::EncryptedNotes => summary.encrypted,
        ViewMode::Trash => false,
    }
}

fn filtered_trash(state: &AppState, query: &str) -> Vec<TrashEntry> {
    let query = query.trim().to_lowercase();
    state
        .trash
        .iter()
        .filter(|entry| query.is_empty() || entry.title.to_lowercase().contains(&query))
        .cloned()
        .collect()
}

fn truncate_preview(body: &str, limit: usize) -> String {
    let normalized = body.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut preview = normalized.chars().take(limit).collect::<String>();
    if normalized.chars().count() > limit {
        preview.push('…');
    }
    preview
}

fn show_trash_by_id(id: Uuid, state: &Rc<RefCell<AppState>>, widgets: &Widgets) {
    let entry = {
        state
            .borrow()
            .trash
            .iter()
            .find(|entry| entry.id == id)
            .cloned()
    };
    let Some(entry) = entry else {
        return;
    };
    state.borrow_mut().flow.select_trash(entry.id);
    widgets.trash_detail_title.set_label(&entry.title);
    widgets
        .document_stack
        .set_visible_child_name("trash-detail");
}

fn move_selected_to_trash(state: &Rc<RefCell<AppState>>, widgets: &Widgets) {
    let is_trash_view = { state.borrow().flow.view() == &ViewMode::Trash };
    if is_trash_view {
        return;
    }
    let id = selected_row_target(widgets)
        .and_then(|target| match target {
            RowTarget::Note(id) => Some(id),
            RowTarget::Trash(_) => None,
        })
        .or_else(|| state.borrow().flow.selected_note());
    let Some(id) = id else {
        return;
    };
    move_note_to_trash_by_id(id, state, widgets);
}

fn move_note_to_trash_by_id(id: Uuid, state: &Rc<RefCell<AppState>>, widgets: &Widgets) {
    if !persist_active(state, widgets, true) {
        return;
    }
    let (vault, relative, active_is_target) = {
        let state = state.borrow();
        let relative = state
            .notes
            .iter()
            .find(|summary| summary.id == id)
            .map(|summary| summary.relative_path.clone());
        (
            state.vault.clone(),
            relative,
            state
                .active
                .as_ref()
                .is_some_and(|active| active.id() == id),
        )
    };
    let (Some(vault), Some(relative)) = (vault, relative) else {
        return;
    };
    match vault.move_to_trash(&relative) {
        Ok(entry) => {
            {
                let mut state = state.borrow_mut();
                if let Some(mut document) = state.unlocked_cache.remove(&entry.id) {
                    document.clear_sensitive();
                }
                if active_is_target && let Some(mut active) = state.active.take() {
                    active.clear_sensitive();
                }
                state.notes.retain(|summary| summary.id != id);
                state.plain_cache.remove(&id);
                state.flow.note_moved_to_trash(id);
            }
            refresh_watch_baseline(state);
            let was_selected = selected_row_target(widgets) == Some(RowTarget::Note(id));
            let removed_index = remove_row_target(RowTarget::Note(id), widgets);
            if active_is_target {
                clear_editor(state, widgets);
            }
            if was_selected && let Some(index) = removed_index {
                select_adjacent_after_removal(index, state, widgets);
            }
        }
        Err(error) => widgets
            .save_status
            .set_label(&format!("Could not move note to Trash: {error}")),
    }
}

fn rename_note_by_id(
    id: Uuid,
    state: &Rc<RefCell<AppState>>,
    widgets: &Widgets,
    pending: &Rc<RefCell<PendingSaves>>,
) {
    let already_active = {
        state
            .borrow()
            .active
            .as_ref()
            .is_some_and(|active| active.id() == id)
    };
    if !already_active {
        if !prepare_to_leave_active(state, widgets, pending) {
            return;
        }
        select_row_target(RowTarget::Note(id), widgets);
        load_note_by_id(id, state, widgets);
    }
    widgets.content_split.set_show_content(true);
    if widgets.title.is_sensitive() {
        widgets.title.grab_focus();
        widgets.title.select_region(0, -1);
    }
}

/// Moves a note into `destination` and keeps every runtime structure keyed
/// by its old path coherent - see the "Notebook move/rename runtime state"
/// audit this follows: flush any pending edit *before* the filesystem move
/// (so a stray autosave timer can never fire against the vanished old path
/// and recreate it), then rebind `relative_path` everywhere it is cached, in
/// one place, immediately after. Works identically for plaintext and
/// encrypted notes - `Vault::move_note` never touches file content.
fn move_note_by_id(id: Uuid, destination: &Path, state: &Rc<RefCell<AppState>>, widgets: &Widgets) {
    if !persist_active(state, widgets, true) {
        return;
    }
    let (vault, relative) = {
        let state = state.borrow();
        (
            state.vault.clone(),
            state
                .notes
                .iter()
                .find(|summary| summary.id == id)
                .map(|summary| summary.relative_path.clone()),
        )
    };
    let (Some(vault), Some(relative)) = (vault, relative) else {
        return;
    };
    match vault.move_note(&relative, destination) {
        Ok(next_relative) => {
            {
                let mut state = state.borrow_mut();
                if let Some(active) = state.active.as_mut()
                    && active.id() == id
                {
                    active.note_mut().relative_path = next_relative.clone();
                }
                if let Some((note, _stamp)) = state.plain_cache.get_mut(&id) {
                    note.relative_path = next_relative.clone();
                }
                if let Some(document) = state.unlocked_cache.get_mut(&id) {
                    document.note_mut().relative_path = next_relative.clone();
                }
                if let Some(summary) = state.notes.iter_mut().find(|summary| summary.id == id) {
                    summary.relative_path = next_relative.clone();
                }
            }
            refresh_watch_baseline(state);
            let still_visible = {
                let state = state.borrow();
                state
                    .notes
                    .iter()
                    .find(|summary| summary.id == id)
                    .is_some_and(|summary| {
                        view_includes(state.flow.view(), summary, &state.session.recently_opened)
                    })
            };
            if still_visible {
                render_note_list(state, widgets);
            } else {
                let was_selected = selected_row_target(widgets) == Some(RowTarget::Note(id));
                let removed_index = remove_row_target(RowTarget::Note(id), widgets);
                if was_selected && let Some(index) = removed_index {
                    select_adjacent_after_removal(index, state, widgets);
                }
            }
            widgets.save_status.set_label("Moved");
        }
        Err(error) => widgets
            .save_status
            .set_label(&format!("Could not move note: {error}")),
    }
}

/// A small controlled window listing every notebook so the user can move a
/// note into one. Reuses the same "SenatorialNotes-controlled window, never
/// a self-closing dialog" convention as the password prompts.
fn present_move_to_notebook_dialog(id: Uuid, state: &Rc<RefCell<AppState>>, widgets: &Widgets) {
    let vault = { state.borrow().vault.clone() };
    let Some(vault) = vault else {
        return;
    };
    let notebooks = match vault.list_notebooks() {
        Ok(notebooks) => notebooks,
        Err(error) => {
            widgets
                .save_status
                .set_label(&format!("Could not list notebooks: {error}"));
            return;
        }
    };

    let window = adw::Window::builder()
        .transient_for(&widgets.window)
        .modal(true)
        .default_width(320)
        .default_height(400)
        .title("Move to Notebook")
        .build();
    close_on_escape(&window);
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let header = adw::HeaderBar::new();
    header.set_show_end_title_buttons(false);
    let cancel = gtk::Button::with_label("Cancel");
    header.pack_start(&cancel);
    content.append(&header);
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::None);
    list.add_css_class("boxed-list");
    list.set_margin_start(12);
    list.set_margin_end(12);
    list.set_margin_top(12);
    list.set_margin_bottom(12);
    let mut destinations = Vec::with_capacity(notebooks.len());
    for notebook in &notebooks {
        let depth = notebook
            .relative_path
            .components()
            .count()
            .saturating_sub(1);
        // Reuses the same Inbox -> "Unfiled" display mapping as the
        // sidebar/Note Information panel, so this list never shows the raw
        // on-disk "Inbox" name.
        let name = ViewMode::Notebook(notebook.relative_path.clone()).heading();
        let row = gtk::ListBoxRow::new();
        row.set_activatable(true);
        let label = gtk::Label::new(Some(&format!("{}{}", "    ".repeat(depth), name)));
        label.set_xalign(0.0);
        label.set_margin_top(8);
        label.set_margin_bottom(8);
        label.set_margin_start(8);
        label.set_margin_end(8);
        row.set_child(Some(&label));
        list.append(&row);
        destinations.push(notebook.relative_path.clone());
    }
    {
        let state = state.clone();
        let widgets_for_row = widgets.clone();
        let window_for_row = window.clone();
        list.connect_row_activated(move |_, row| {
            let Some(destination) = destinations.get(row.index() as usize) else {
                return;
            };
            move_note_by_id(id, destination, &state, &widgets_for_row);
            window_for_row.close();
        });
    }
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&list)
        .build();
    content.append(&scroll);
    window.set_content(Some(&content));
    let window_for_cancel = window.clone();
    cancel.connect_clicked(move |_| window_for_cancel.close());
    window.present();
}

/// A protected boolean note field that can be toggled either while the note
/// is the open/unlocked active document, or directly on disk for a
/// background *plaintext* note (never for a background locked encrypted one
/// - see `toggle_note_flag`).
#[derive(Clone, Copy)]
enum NoteFlag {
    Pinned,
    Favourite,
    Archived,
}

impl NoteFlag {
    fn get(self, metadata: &NoteMetadata) -> bool {
        match self {
            Self::Pinned => metadata.pinned,
            Self::Favourite => metadata.favourite,
            Self::Archived => metadata.archived,
        }
    }

    fn set(self, metadata: &mut NoteMetadata, value: bool) {
        match self {
            Self::Pinned => metadata.pinned = value,
            Self::Favourite => metadata.favourite = value,
            Self::Archived => metadata.archived = value,
        }
    }

    fn action_name(self) -> &'static str {
        match self {
            Self::Pinned => "pinned",
            Self::Favourite => "favourite",
            Self::Archived => "archived",
        }
    }

    fn status_labels(self) -> (&'static str, &'static str) {
        match self {
            Self::Pinned => ("Pinned", "Unpinned"),
            Self::Favourite => ("Added to Favourites", "Removed from Favourites"),
            Self::Archived => ("Archived", "Unarchived"),
        }
    }
}

/// Toggles `flag` on a note.
///
/// A background (not currently open) **plaintext** note is toggled directly
/// via a load/save round trip, matching how rename/pin already worked in
/// v0.1. A **locked** encrypted note is refused outright - `pinned` and
/// `archived` both live inside the encrypted payload, so changing either
/// without the password would require either a plaintext side channel (not
/// acceptable) or guessing (not acceptable either); the note must be
/// unlocked first. An **unlocked** encrypted note (the active document) is
/// toggled in memory and re-encrypted through the normal save path, exactly
/// like a plaintext note.
fn toggle_note_flag(flag: NoteFlag, id: Uuid, state: &Rc<RefCell<AppState>>, widgets: &Widgets) {
    if !persist_active(state, widgets, true) {
        return;
    }
    let (vault, summary, active_is_target) = {
        let state = state.borrow();
        (
            state.vault.clone(),
            state.notes.iter().find(|summary| summary.id == id).cloned(),
            state
                .active
                .as_ref()
                .is_some_and(|active| active.id() == id),
        )
    };
    let (Some(vault), Some(summary)) = (vault, summary) else {
        return;
    };

    if summary.encrypted && !active_is_target {
        widgets.save_status.set_label(&format!(
            "Unlock this encrypted note before changing its {} state.",
            flag.action_name()
        ));
        return;
    }

    let save_succeeded = if active_is_target {
        {
            let mut state = state.borrow_mut();
            if let Some(active) = state.active.as_mut() {
                let metadata = &mut active.note_mut().metadata;
                let next = !flag.get(metadata);
                flag.set(metadata, next);
                state.body_dirty = true;
            }
        }
        persist_active(state, widgets, false)
    } else {
        match vault.load_note(&summary.relative_path) {
            Ok((mut note, stamp)) => {
                let next = !flag.get(&note.metadata);
                flag.set(&mut note.metadata, next);
                match vault.save_note(&mut note, Some(&stamp)) {
                    Ok(_) => {
                        let updated = NoteSummary::from(&note);
                        if let Some(existing) = state
                            .borrow_mut()
                            .notes
                            .iter_mut()
                            .find(|candidate| candidate.id == id)
                        {
                            *existing = updated;
                        }
                        true
                    }
                    Err(error) => {
                        widgets.save_status.set_label(&format!(
                            "Could not change {} state: {error}",
                            flag.action_name()
                        ));
                        false
                    }
                }
            }
            Err(error) => {
                widgets.save_status.set_label(&format!(
                    "Could not open note to change its {}: {error}",
                    flag.action_name()
                ));
                false
            }
        }
    };

    if !save_succeeded {
        return;
    }
    {
        let mut state = state.borrow_mut();
        let sort_order = state.config.sort_order;
        sort_notes(&mut state.notes, sort_order);
    }
    let updated = {
        state
            .borrow()
            .notes
            .iter()
            .find(|summary| summary.id == id)
            .cloned()
    };
    if let Some(updated) = updated {
        let now_set = match flag {
            NoteFlag::Pinned => updated.pinned,
            NoteFlag::Favourite => updated.favourite,
            NoteFlag::Archived => updated.archived,
        };
        let visible_in_current_view = {
            let st = state.borrow();
            view_includes(st.flow.view(), &updated, &st.session.recently_opened)
        };
        if visible_in_current_view {
            replace_note_row(&updated, state, widgets);
        } else {
            let was_selected = selected_row_target(widgets) == Some(RowTarget::Note(id));
            let removed_index = remove_row_target(RowTarget::Note(id), widgets);
            if was_selected && let Some(index) = removed_index {
                select_adjacent_after_removal(index, state, widgets);
            }
        }
        let (on_label, off_label) = flag.status_labels();
        widgets
            .save_status
            .set_label(if now_set { on_label } else { off_label });
        render_tags_list(state, widgets);
        update_note_quick_actions(state, widgets);
    }
}

fn encrypt_note_by_id(
    id: Uuid,
    state: &Rc<RefCell<AppState>>,
    widgets: &Widgets,
    pending: &Rc<RefCell<PendingSaves>>,
) {
    let already_active = {
        state
            .borrow()
            .active
            .as_ref()
            .is_some_and(|active| active.id() == id)
    };
    if !already_active {
        if !prepare_to_leave_active(state, widgets, pending) {
            return;
        }
        select_row_target(RowTarget::Note(id), widgets);
        load_note_by_id(id, state, widgets);
    }
    encrypt_active_note(state, widgets);
}

fn restore_selected(state: &Rc<RefCell<AppState>>, widgets: &Widgets) {
    let id = selected_row_target(widgets)
        .and_then(|target| match target {
            RowTarget::Trash(id) => Some(id),
            RowTarget::Note(_) => None,
        })
        .or_else(|| state.borrow().flow.selected_trash());
    let Some(id) = id else {
        return;
    };
    restore_note_by_id(id, state, widgets);
}

fn restore_note_by_id(id: Uuid, state: &Rc<RefCell<AppState>>, widgets: &Widgets) {
    let vault = { state.borrow().vault.clone() };
    let Some(vault) = vault else {
        return;
    };
    match vault.restore_from_trash(id) {
        Ok(_) => {
            let was_selected = selected_row_target(widgets) == Some(RowTarget::Trash(id));
            {
                let mut state = state.borrow_mut();
                state.trash.retain(|entry| entry.id != id);
                state.flow.note_restored(id);
            }
            let removed_index = remove_row_target(RowTarget::Trash(id), widgets);
            if was_selected && let Some(index) = removed_index {
                select_adjacent_after_removal(index, state, widgets);
            }
            widgets.save_status.set_label("Note restored");
        }
        Err(error) => widgets
            .save_status
            .set_label(&format!("Could not restore note: {error}")),
    }
}

fn confirm_permanent_delete(state: &Rc<RefCell<AppState>>, widgets: &Widgets) {
    let id = selected_row_target(widgets)
        .and_then(|target| match target {
            RowTarget::Trash(id) => Some(id),
            RowTarget::Note(_) => None,
        })
        .or_else(|| state.borrow().flow.selected_trash());
    let Some(id) = id else {
        return;
    };
    confirm_permanent_delete_by_id(id, state, widgets);
}

fn confirm_permanent_delete_by_id(id: Uuid, state: &Rc<RefCell<AppState>>, widgets: &Widgets) {
    let dialog = gtk::AlertDialog::builder()
        .modal(true)
        .message("Permanently delete this note?")
        .detail("This cannot be undone.")
        .buttons(["Cancel", "Permanently Delete"])
        .cancel_button(0)
        .default_button(0)
        .build();
    let state = state.clone();
    let widgets = widgets.clone();
    let parent = widgets.window.clone();
    dialog.choose(Some(&parent), None::<&gio::Cancellable>, move |result| {
        if result != Ok(1) {
            return;
        }
        let vault = { state.borrow().vault.clone() };
        let Some(vault) = vault else {
            return;
        };
        match vault.permanently_delete(id) {
            Ok(()) => {
                let was_selected = selected_row_target(&widgets) == Some(RowTarget::Trash(id));
                {
                    let mut state = state.borrow_mut();
                    state.trash.retain(|entry| entry.id != id);
                    state.flow.note_restored(id);
                }
                let removed_index = remove_row_target(RowTarget::Trash(id), &widgets);
                if was_selected && let Some(index) = removed_index {
                    select_adjacent_after_removal(index, &state, &widgets);
                }
            }
            Err(error) => widgets
                .save_status
                .set_label(&format!("Could not permanently delete note: {error}")),
        }
    });
}

fn confirm_empty_trash(state: &Rc<RefCell<AppState>>, widgets: &Widgets) {
    let is_empty = { state.borrow().trash.is_empty() };
    if is_empty {
        return;
    }
    let dialog = gtk::AlertDialog::builder()
        .modal(true)
        .message("Empty Trash?")
        .detail("Every note in Trash will be permanently deleted. This cannot be undone.")
        .buttons(["Cancel", "Empty Trash"])
        .cancel_button(0)
        .default_button(0)
        .build();
    let state = state.clone();
    let widgets = widgets.clone();
    let parent = widgets.window.clone();
    dialog.choose(Some(&parent), None::<&gio::Cancellable>, move |result| {
        if result != Ok(1) {
            return;
        }
        let vault = { state.borrow().vault.clone() };
        let Some(vault) = vault else {
            return;
        };
        match vault.empty_trash() {
            Ok(_) => {
                {
                    let mut state = state.borrow_mut();
                    state.trash.clear();
                    state.flow.clear_selection();
                }
                widgets.document_stack.set_visible_child_name("empty");
                render_note_list(&state, &widgets);
            }
            Err(error) => widgets
                .save_status
                .set_label(&format!("Could not empty Trash: {error}")),
        }
    });
}

fn clear_editor(state: &Rc<RefCell<AppState>>, widgets: &Widgets) {
    {
        let mut state = state.borrow_mut();
        state.body_dirty = false;
        state.title_dirty = false;
        state.title_draft.clear();
    }
    let _editor_guard = widgets.editor_events.suppress();
    widgets.title.set_text("");
    set_buffer_text_silently(&widgets.buffer, "");
    widgets.document_stack.set_visible_child_name("empty");
    widgets.tags_row.set_visible(false);
}

/// Adds a tag to the currently active note. Requires a note to be open (and,
/// for an encrypted note, unlocked - `state.active` only ever holds
/// decrypted content), matching the same "must be unlocked" rule already
/// enforced for pin/archive (see `toggle_note_flag`); there is no path from
/// here to a locked note's protected metadata.
fn add_tag_to_active_note(tag: &str, state: &Rc<RefCell<AppState>>, widgets: &Widgets) {
    let trimmed = tag.trim();
    if trimmed.is_empty() {
        return;
    }
    let added = {
        let mut state = state.borrow_mut();
        let Some(active) = state.active.as_mut() else {
            return;
        };
        let added = active.note_mut().metadata.add_tag(trimmed);
        if added {
            state.body_dirty = true;
        }
        added
    };
    if added && persist_active(state, widgets, false) {
        render_active_tags(state, widgets);
        update_active_summary(state, widgets);
        render_tags_list(state, widgets);
    }
}

fn remove_tag_from_active_note(tag: &str, state: &Rc<RefCell<AppState>>, widgets: &Widgets) {
    let removed = {
        let mut state = state.borrow_mut();
        let Some(active) = state.active.as_mut() else {
            return;
        };
        let removed = active.note_mut().metadata.remove_tag(tag);
        if removed {
            state.body_dirty = true;
        }
        removed
    };
    if removed && persist_active(state, widgets, false) {
        render_active_tags(state, widgets);
        update_active_summary(state, widgets);
        render_tags_list(state, widgets);
    }
}

fn install_list_delete_key(state: &Rc<RefCell<AppState>>, widgets: &Widgets) {
    let controller = gtk::EventControllerKey::new();
    let state = state.clone();
    let widgets_for_key = widgets.clone();
    controller.connect_key_pressed(move |_, key, _, _| {
        let is_trash_view = { state.borrow().flow.view() == &ViewMode::Trash };
        if key == gdk::Key::Delete && !is_trash_view {
            move_selected_to_trash(&state, &widgets_for_key);
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    widgets.note_list.add_controller(controller);
}

fn install_watcher_poll(state: &Rc<RefCell<AppState>>, widgets: &Widgets) {
    let state = state.clone();
    let widgets = widgets.clone();
    glib::timeout_add_local(Duration::from_millis(500), move || {
        // Stay out of the way of an in-flight selection burst: a vault-wide
        // rescan or note-list rebuild here would fight the coalesced dispatch
        // and reintroduce the stall. The events stay queued for the next tick.
        if widgets.select_source.borrow().is_some() {
            return glib::ControlFlow::Continue;
        }
        let changed = {
            let state = state.borrow();
            state.watcher.as_ref().map(VaultWatcher::drain_changes)
        };
        let editor_is_clean = {
            let state = state.borrow();
            !state.body_dirty && !state.title_dirty
        };
        // A locked encrypted vault has no in-memory plaintext to reconcile.
        // External ciphertext churn is picked up by the full rescan that
        // `enter_vault_workspace` runs on the next unlock; parsing the
        // encrypted store here would be meaningless (and impossible).
        let vault_locked = {
            let state = state.borrow();
            state.vault.as_ref().is_some_and(Vault::is_locked)
        };
        match changed {
            Some(Ok(true)) if editor_is_clean && !vault_locked => {
                // Distinguish our own just-committed atomic writes from real
                // external edits with a cheap stat-only comparison. If the tree
                // matches the baseline SenatorialNotes last wrote, the event is
                // ours and no vault-wide rescan is needed.
                let (vault, baseline) = {
                    let state = state.borrow();
                    (state.vault.clone(), state.watch_baseline.clone())
                };
                if let Some(vault) = vault {
                    let current = note_tree_snapshot(&vault);
                    if current != baseline {
                        reload_after_external_change(&state, &widgets);
                        state.borrow_mut().watch_baseline = note_tree_snapshot(&vault);
                    }
                }
            }
            Some(Err(error)) => {
                state.borrow_mut().watcher = None;
                widgets
                    .save_status
                    .set_label(&format!("Filesystem watcher failed: {error}"));
            }
            _ => {}
        }
        glib::ControlFlow::Continue
    });
}

/// Cheap stat-only snapshot of the directories the vault says to watch:
/// (path, mtime, length) for every file, sorted. No file contents are read.
/// For an ordinary vault these are the notes and trash trees; for an encrypted
/// vault, the opaque ciphertext store. Encrypted payloads are never parsed.
fn note_tree_snapshot(vault: &Vault) -> Vec<(std::path::PathBuf, std::time::SystemTime, u64)> {
    fn walk(directory: &Path, out: &mut Vec<(std::path::PathBuf, std::time::SystemTime, u64)>) {
        let Ok(entries) = std::fs::read_dir(directory) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            let path = entry.path();
            if file_type.is_dir() {
                walk(&path, out);
                continue;
            }
            if let Ok(metadata) = entry.metadata() {
                let modified = metadata
                    .modified()
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                out.push((path, modified, metadata.len()));
            }
        }
    }

    let mut out = Vec::new();
    for directory in vault.watch_paths() {
        walk(&directory, &mut out);
    }
    out.sort();
    out
}

/// Resets the watcher baseline to the tree as it exists right now, so the next
/// poll treats SenatorialNotes' own writes as already accounted for.
fn refresh_watch_baseline(state: &Rc<RefCell<AppState>>) {
    let vault = { state.borrow().vault.clone() };
    if let Some(vault) = vault {
        let snapshot = note_tree_snapshot(&vault);
        state.borrow_mut().watch_baseline = snapshot;
    }
}

fn reload_after_external_change(state: &Rc<RefCell<AppState>>, widgets: &Widgets) {
    let active = {
        state.borrow().active.as_ref().map(|active| {
            (
                active.id(),
                active.note().relative_path.clone(),
                active.stamp().clone(),
                active.is_encrypted(),
            )
        })
    };
    let active_changed = active
        .as_ref()
        .and_then(|(_, path, stamp, _)| {
            state
                .borrow()
                .vault
                .as_ref()
                .map(|vault| vault.current_stamp(path).map(|current| current != *stamp))
        })
        .transpose();
    match active_changed {
        Ok(Some(true)) => {
            widgets.save_status.set_label(
                "The open note changed on disk. Save is blocked until you reload or preserve your editor text.",
            );
            return;
        }
        Err(error) => {
            widgets
                .save_status
                .set_label(&format!("Could not verify external changes: {error}"));
            return;
        }
        _ => {}
    }
    // Internal saves produce watcher events too. Refreshing the list is safe,
    // but the active editor/title are deliberately never reconstructed when
    // their file stamp matches the just-saved stamp.
    refresh_after_watcher(state, widgets, active.as_ref().map(|(id, _, _, _)| *id));
    if let Some((id, _, _, _)) = active {
        select_row_target(RowTarget::Note(id), widgets);
    }
}

fn refresh_after_watcher(
    state: &Rc<RefCell<AppState>>,
    widgets: &Widgets,
    preserve_active_id: Option<Uuid>,
) {
    let (vault, mode, query) = {
        let state = state.borrow();
        (
            state.vault.clone(),
            state.flow.view().clone(),
            widgets.search.text().to_string(),
        )
    };
    let Some(vault) = vault else {
        return;
    };
    match mode {
        ViewMode::Trash => {
            let Ok(mut scanned) = vault.scan_trash() else {
                widgets
                    .save_status
                    .set_label("Trash changed on disk, but the updated list could not be read.");
                return;
            };
            {
                let mut state = state.borrow_mut();
                let previous = std::mem::take(&mut state.trash);
                let mut merged = Vec::with_capacity(scanned.len());
                for old in previous {
                    if let Some(index) = scanned.iter().position(|new| new.id == old.id) {
                        merged.push(scanned.remove(index));
                    }
                }
                merged.extend(scanned);
                state.trash = merged;
            }
            let entries = { filtered_trash(&state.borrow(), &query) };
            let targets = entries
                .iter()
                .map(|entry| RowTarget::Trash(entry.id))
                .collect::<Vec<_>>();
            if targets != widgets.selection.rows() {
                render_note_list(state, widgets);
                return;
            }
            for entry in entries {
                let row_widgets = { widgets.row_widgets.borrow().get(&entry.id).cloned() };
                if let Some(row_widgets) = row_widgets {
                    let subtitle = if entry.encrypted {
                        "Encrypted note".to_owned()
                    } else {
                        format!("From {}", entry.original_relative_path.display())
                    };
                    row_widgets.title.set_label(&entry.title);
                    row_widgets.preview.set_label(&subtitle);
                }
            }
        }
        _ => {
            let Ok(mut scanned) = vault.scan_notes() else {
                widgets
                    .save_status
                    .set_label("Notes changed on disk, but the updated list could not be read.");
                return;
            };
            let flag_changes = {
                let mut state = state.borrow_mut();
                let previous = std::mem::take(&mut state.notes);
                let mut merged = Vec::with_capacity(scanned.len());
                let mut flag_changes = Vec::new();
                for old in previous {
                    if let Some(index) = scanned.iter().position(|new| new.id == old.id) {
                        let new = scanned.remove(index);
                        if old.pinned != new.pinned || old.archived != new.archived {
                            flag_changes.push(new.id);
                        }
                        if preserve_active_id == Some(old.id) {
                            // Keep the in-memory title/body the user may be
                            // editing, but always take the freshly scanned
                            // location - a notebook rename must never leave
                            // the active note's own summary pointing at a
                            // path that no longer exists on disk.
                            let mut preserved = old;
                            preserved.relative_path = new.relative_path;
                            merged.push(preserved);
                        } else {
                            merged.push(new);
                        }
                    }
                }
                merged.extend(scanned);
                state.notes = merged;
                flag_changes
            };
            let (targets, summaries) = {
                let state = state.borrow();
                let summaries = filtered_notes(&state, &query);
                let targets = summaries
                    .iter()
                    .map(|summary| RowTarget::Note(summary.id))
                    .collect::<Vec<_>>();
                (targets, summaries)
            };
            if targets != widgets.selection.rows() {
                render_note_list(state, widgets);
                return;
            }
            for summary in summaries {
                if flag_changes.contains(&summary.id) {
                    replace_note_row(&summary, state, widgets);
                    continue;
                }
                let row_widgets = { widgets.row_widgets.borrow().get(&summary.id).cloned() };
                if let Some(row_widgets) = row_widgets {
                    row_widgets.title.set_label(&summary.title);
                    row_widgets.preview.set_label(&summary.preview);
                    row_widgets.pin.set_visible(summary.pinned);
                    row_widgets.favourite.set_visible(summary.favourite);
                    row_widgets.archived.set_visible(summary.archived);
                }
            }
        }
    }
}

fn reselect_active_note(state: &Rc<RefCell<AppState>>, widgets: &Widgets) {
    let active_id = {
        let state = state.borrow();
        state
            .active
            .as_ref()
            .map(ActiveDocument::id)
            .or_else(|| state.flow.selected_note())
    };
    let Some(id) = active_id else {
        return;
    };
    select_row_target(RowTarget::Note(id), widgets);
}

fn connect_locking_events(
    state: &Rc<RefCell<AppState>>,
    widgets: &Widgets,
    pending: &Rc<RefCell<PendingSaves>>,
) {
    {
        let state = state.clone();
        let widgets = widgets.clone();
        let pending = pending.clone();
        widgets
            .window
            .clone()
            .connect_is_active_notify(move |window| {
                if window.is_active() {
                    return;
                }
                let config = { state.borrow().config.encrypted_note_locking.clone() };
                if config.on_focus_loss || config.on_minimize {
                    lock_all_encrypted(&state, &widgets);
                    lock_vault(&state, &widgets, &pending);
                }
            });
    }
    {
        let state = state.clone();
        let widgets = widgets.clone();
        let pending = pending.clone();
        glib::timeout_add_local(Duration::from_secs(30), move || {
            let (minutes, last_sensitive_activity) = {
                let state = state.borrow();
                (
                    state.config.encrypted_note_locking.after_minutes,
                    state.last_sensitive_activity,
                )
            };
            let expired = minutes > 0
                && last_sensitive_activity
                    .is_some_and(|last| last.elapsed() >= Duration::from_secs(minutes as u64 * 60));
            if expired {
                lock_all_encrypted(&state, &widgets);
                lock_vault(&state, &widgets, &pending);
            }
            glib::ControlFlow::Continue
        });
    }
}

fn lock_all_encrypted(state: &Rc<RefCell<AppState>>, widgets: &Widgets) {
    if !persist_active(state, widgets, true) {
        return;
    }
    let mut newly_locked: Vec<(Uuid, PathBuf)> = Vec::new();
    {
        let mut state = state.borrow_mut();
        for (id, mut document) in state.unlocked_cache.drain() {
            newly_locked.push((id, document.note().relative_path.clone()));
            document.clear_sensitive();
        }
    }
    let active_encrypted = {
        state
            .borrow()
            .active
            .as_ref()
            .is_some_and(ActiveDocument::is_encrypted)
    };
    if active_encrypted {
        {
            let mut state = state.borrow_mut();
            if let Some(mut active) = state.active.take() {
                newly_locked.push((active.id(), active.note().relative_path.clone()));
                active.clear_sensitive();
            }
            state.last_sensitive_activity = None;
        }
        let _editor_guard = widgets.editor_events.suppress();
        widgets.title.set_text("");
        set_buffer_text_silently(&widgets.buffer, "");
        show_locked_placeholder(widgets);
        widgets.save_status.set_label("Locked · encrypted at rest");
    }
    if newly_locked.is_empty() {
        return;
    }
    // A locked note's protected metadata (pinned/archived/recency) is no
    // longer known - reset every summary this call locked back to the exact
    // same non-committal placeholder a fresh scan would produce, so nothing
    // that was true a moment ago is still displayed as true. See the "Locked
    // encrypted notes" note in SECURITY.md.
    let mut locked_titles = std::collections::HashMap::new();
    {
        let mut state = state.borrow_mut();
        for (id, relative_path) in &newly_locked {
            if let Some(summary) = state.notes.iter_mut().find(|summary| summary.id == *id) {
                *summary = NoteSummary::locked(*id, relative_path.clone());
                locked_titles.insert(*id, summary.title.clone());
            }
        }
    }
    let must_rebuild = matches!(
        state.borrow().flow.view(),
        ViewMode::Pinned | ViewMode::Favourites | ViewMode::Archive | ViewMode::RecentlyOpened
    );
    if must_rebuild {
        // The note no longer qualifies for a protected-field smart view
        // (unknown now defaults to false), so it must actually leave the
        // list, not just have its row relabeled.
        render_note_list(state, widgets);
    } else {
        for (id, _) in &newly_locked {
            let row_widgets = { widgets.row_widgets.borrow().get(id).cloned() };
            if let Some(row_widgets) = row_widgets {
                // Reuse the exact label `NoteSummary::locked` just computed
                // above (its anonymous, UUID-derived suffix) rather than a
                // bare "Locked Note" literal, so this row stays
                // distinguishable from every other locked note in the list.
                if let Some(title) = locked_titles.get(id) {
                    row_widgets.title.set_label(title);
                }
                row_widgets.preview.set_label("Encrypted — unlock to view");
                row_widgets.pin.set_visible(false);
                row_widgets.favourite.set_visible(false);
                row_widgets.archived.set_visible(false);
            }
        }
    }
    update_note_quick_actions(state, widgets);
}

fn encrypt_active_note(state: &Rc<RefCell<AppState>>, widgets: &Widgets) {
    if !persist_active(state, widgets, true) {
        return;
    }
    let cannot_encrypt = {
        state
            .borrow()
            .active
            .as_ref()
            .is_none_or(ActiveDocument::is_encrypted)
    };
    if cannot_encrypt {
        widgets
            .save_status
            .set_label("Select an ordinary Markdown note to encrypt.");
        return;
    }
    let state_for_encrypt = state.clone();
    let widgets_for_encrypt = widgets.clone();
    present_password_dialog(
        &widgets.window,
        "Encrypt Note",
        "Choose a strong passphrase. There is no password recovery, backdoor, or master key.",
        true,
        true,
        "Encrypt",
        move |password| {
            let Some(password) = password else {
                return;
            };
            let vault = { state_for_encrypt.borrow().vault.clone() };
            let Some(vault) = vault else {
                return;
            };
            let active = { state_for_encrypt.borrow_mut().active.take() };
            let Some(ActiveDocument::Plain { mut note, stamp }) = active else {
                return;
            };
            match vault.encrypt_note(&mut note, Some(&stamp), password.as_str()) {
                Ok((next_stamp, session)) => {
                    {
                        let mut state = state_for_encrypt.borrow_mut();
                        state.active = Some(ActiveDocument::Encrypted {
                            note,
                            stamp: next_stamp,
                            session,
                        });
                        state.last_sensitive_activity = Some(Instant::now());
                    }
                    widgets_for_encrypt
                        .save_status
                        .set_label("Encrypted and saved · no recovery password exists");
                    refresh_current_view(&state_for_encrypt, &widgets_for_encrypt);
                    reselect_active_note(&state_for_encrypt, &widgets_for_encrypt);
                    refresh_watch_baseline(&state_for_encrypt);
                }
                Err(error) => {
                    {
                        state_for_encrypt.borrow_mut().active =
                            Some(ActiveDocument::Plain { note, stamp });
                    }
                    widgets_for_encrypt
                        .save_status
                        .set_label(&format!("Could not encrypt note: {error}"));
                }
            }
        },
    );
}

/// Renames the currently open vault. **Display name only** - stored in
/// `config.vault_index`; the folder, the vault, its manifest, and every
/// encrypted blob (and their AAD) are untouched.
fn rename_current_vault(
    state: &Rc<RefCell<AppState>>,
    widgets: &Widgets,
    pending: &Rc<RefCell<PendingSaves>>,
) {
    let (root, current) = {
        let state = state.borrow();
        let Some(vault) = state.vault.as_ref() else {
            return;
        };
        let root = vault.root().to_path_buf();
        let current = vault_display_name_for(&state.config, &root);
        (root, current)
    };

    let state = state.clone();
    let widgets = widgets.clone();
    let pending = pending.clone();
    present_text_entry_dialog(
        &widgets.window.clone(),
        "Rename Vault",
        "This changes only the name shown in SenatorialNotes. The folder on disk is not moved \
         or renamed, and nothing is re-encrypted.",
        "Vault name",
        &current,
        "Rename",
        move |maybe_name| {
            let Some(name) = maybe_name else {
                return;
            };
            if name.trim().is_empty() {
                return;
            }
            {
                let mut state = state.borrow_mut();
                state.config.set_vault_display_name(&root, &name);
                let _ = state.config.save();
            }
            let display = { vault_display_name_for(&state.borrow().config, &root) };
            widgets.vault_label.set_label(&display);
            render_vault_switcher(&state, &widgets, &pending);
        },
    );
}

/// Changes the current Secure Vault's password. Only meaningful for an
/// unlocked Secure Vault. The Argon2id re-wrap runs on a worker thread; the
/// unlocked session stays valid (the vault master key does not change).
fn change_vault_password_flow(
    state: &Rc<RefCell<AppState>>,
    widgets: &Widgets,
    pending: &Rc<RefCell<PendingSaves>>,
) {
    let (state_dir, vault_id) = {
        let state = state.borrow();
        let Some(vault) = state.vault.as_ref() else {
            return;
        };
        if !vault.is_encrypted() || vault.is_locked() {
            widgets
                .save_status
                .set_label("Unlock the Secure Vault before changing its password.");
            return;
        }
        (vault.state_dir(), vault.vault_id())
    };

    let state = state.clone();
    let widgets = widgets.clone();
    let pending = pending.clone();
    present_change_password_dialog(
        &widgets.window.clone(),
        "Change Vault Password",
        move |passwords| {
            let Some((old_password, new_password)) = passwords else {
                return;
            };
            let session = widgets.sessions.current();
            widgets
                .save_status
                .set_label("Changing the vault password…");
            let state_dir = state_dir.clone();
            let worker = gio::spawn_blocking(move || -> senatorial_notes::Result<()> {
                Vault::rewrap_encrypted_keyfile(
                    &state_dir,
                    vault_id,
                    old_password.as_str(),
                    new_password.as_str(),
                )
            });

            let state = state.clone();
            let widgets = widgets.clone();
            let pending = pending.clone();
            glib::spawn_future_local(async move {
                let outcome = worker.await;
                if !widgets.sessions.is_current(session) {
                    return;
                }
                match outcome {
                    Ok(Ok(())) => {
                        widgets.save_status.set_label(
                            "Vault password changed. Use it next time you unlock this vault.",
                        );
                        refresh_watch_baseline(&state);
                        render_vault_switcher(&state, &widgets, &pending);
                    }
                    Ok(Err(error)) => widgets
                        .save_status
                        .set_label(&format!("The vault password was not changed: {error}")),
                    Err(_) => widgets
                        .save_status
                        .set_label("The vault password change failed unexpectedly."),
                }
            });
        },
    );
}

/// Secure \u{2192} Standard **safe export**. Only for an unlocked Secure Vault
/// with a writable session. The user re-enters the Vault Password (used only to
/// derive the worker's key material), picks an empty destination folder, and
/// confirms that the copy will be unencrypted plaintext. The decrypt/build runs
/// on a worker thread; the source Secure Vault is never modified.
fn present_export_to_standard(
    state: &Rc<RefCell<AppState>>,
    widgets: &Widgets,
    pending: &Rc<RefCell<PendingSaves>>,
) {
    let source = {
        let st = state.borrow();
        let Some(vault) = st.vault.as_ref() else {
            return;
        };
        if !vault.is_encrypted() || vault.is_locked() {
            widgets
                .save_status
                .set_label("Unlock the Secure Vault before exporting it.");
            return;
        }
        if st.read_only {
            widgets
                .save_status
                .set_label("This Secure Vault is open read-only; it cannot be exported yet.");
            return;
        }
        match vault.encrypted_keyfile() {
            Ok(bytes) => ExportSource {
                root: vault.root().to_path_buf(),
                state_dir: vault.state_dir(),
                vault_id: vault.vault_id(),
                keyfile_bytes: bytes,
            },
            Err(error) => {
                widgets
                    .save_status
                    .set_label(&format!("The Secure Vault could not be read: {error}"));
                return;
            }
        }
    };

    let state = state.clone();
    let widgets = widgets.clone();
    let pending = pending.clone();
    let explainer = gtk::AlertDialog::builder()
        .modal(true)
        .message("Export to a Standard Vault")
        .detail(
            "This creates a new, separate Standard Vault containing unencrypted plaintext \
             Markdown copies of every note in this Secure Vault, including trashed notes and \
             the notebook structure. This Secure Vault is not changed.\n\n\
             Individually encrypted (.snote) notes are copied exactly as they are and keep \
             their own passwords. You will be asked for the Vault Password to continue.",
        )
        .buttons(vec!["Cancel", "Continue"])
        .cancel_button(0)
        .default_button(1)
        .build();
    let parent = widgets.window.clone();
    explainer.choose(Some(&parent), None::<&gio::Cancellable>, move |result| {
        if !matches!(result, Ok(1)) {
            return;
        }
        export_step_password(source.clone(), &state, &widgets, &pending);
    });
}

/// The `Send`-owned Secure Vault facts the export worker needs, carried through
/// the dialog chain so no helper takes an unwieldy argument list.
#[derive(Clone)]
struct ExportSource {
    root: PathBuf,
    state_dir: PathBuf,
    vault_id: Uuid,
    keyfile_bytes: Vec<u8>,
}

fn export_step_password(
    source: ExportSource,
    state: &Rc<RefCell<AppState>>,
    widgets: &Widgets,
    pending: &Rc<RefCell<PendingSaves>>,
) {
    let state = state.clone();
    let widgets = widgets.clone();
    let pending = pending.clone();
    present_password_dialog(
        &widgets.window.clone(),
        "Confirm Vault Password",
        "Re-enter this Secure Vault's password. It is used only to unlock the export and is \
         not stored.",
        false,
        false,
        "Continue",
        move |maybe_password| {
            let Some(password) = maybe_password else {
                return;
            };
            export_step_choose_folder(source.clone(), password, &state, &widgets, &pending);
        },
    );
}

fn export_step_choose_folder(
    source: ExportSource,
    password: Zeroizing<String>,
    state: &Rc<RefCell<AppState>>,
    widgets: &Widgets,
    pending: &Rc<RefCell<PendingSaves>>,
) {
    let dialog = gtk::FileDialog::builder()
        .title("Choose an Empty Folder for the Standard Vault")
        .modal(true)
        .build();
    let state = state.clone();
    let widgets = widgets.clone();
    let pending = pending.clone();
    let parent = widgets.window.clone();
    dialog.select_folder(Some(&parent), None::<&gio::Cancellable>, move |result| {
        let destination = match result {
            Ok(folder) => match folder.path() {
                Some(path) => path,
                None => {
                    widgets
                        .save_status
                        .set_label("The selected folder is not a local path.");
                    return;
                }
            },
            Err(error) if !error.matches(gio::IOErrorEnum::Cancelled) => {
                widgets
                    .save_status
                    .set_label(&format!("Folder selection failed: {error}"));
                return;
            }
            Err(_) => return,
        };
        export_step_confirm(
            source.clone(),
            password.clone(),
            destination,
            &state,
            &widgets,
            &pending,
        );
    });
}

fn export_step_confirm(
    source: ExportSource,
    password: Zeroizing<String>,
    destination: PathBuf,
    state: &Rc<RefCell<AppState>>,
    widgets: &Widgets,
    pending: &Rc<RefCell<PendingSaves>>,
) {
    let dialog = gtk::AlertDialog::builder()
        .modal(true)
        .message("Create an unencrypted copy?")
        .detail(format!(
            "The exported Standard Vault at {} will contain your notes as unencrypted \
             plaintext on disk. Anyone with access to that folder can read them.",
            destination.display()
        ))
        .buttons(vec!["Cancel", "Export"])
        .cancel_button(0)
        .default_button(0)
        .build();
    let state = state.clone();
    let widgets = widgets.clone();
    let pending = pending.clone();
    let parent = widgets.window.clone();
    dialog.choose(Some(&parent), None::<&gio::Cancellable>, move |result| {
        if !matches!(result, Ok(1)) {
            return;
        }
        run_export_worker(
            ExportParams {
                source_root: source.root.clone(),
                source_state_dir: source.state_dir.clone(),
                vault_id: source.vault_id,
                keyfile_bytes: source.keyfile_bytes.clone(),
                password: password.clone(),
                destination: destination.clone(),
            },
            &state,
            &widgets,
            &pending,
        );
    });
}

fn run_export_worker(
    params: ExportParams,
    state: &Rc<RefCell<AppState>>,
    widgets: &Widgets,
    pending: &Rc<RefCell<PendingSaves>>,
) {
    let session = widgets.sessions.current();
    let progress = ExportProgress::new();

    // Modal progress window with a Cancel button.
    let window = adw::Window::builder()
        .transient_for(&widgets.window)
        .modal(true)
        .title("Exporting…")
        .default_width(360)
        .resizable(false)
        .build();
    let header = adw::HeaderBar::new();
    header.set_show_start_title_buttons(false);
    header.set_show_end_title_buttons(false);
    let cancel = gtk::Button::with_label("Cancel");
    header.pack_start(&cancel);
    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_margin_start(18);
    content.set_margin_end(18);
    content.set_margin_top(18);
    content.set_margin_bottom(18);
    let spinner = gtk::Spinner::new();
    spinner.start();
    let label = gtk::Label::new(Some("Preparing…"));
    label.set_wrap(true);
    label.set_xalign(0.0);
    content.append(&spinner);
    content.append(&label);
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&content));
    window.set_content(Some(&toolbar));
    {
        let progress = progress.clone();
        let cancel_btn = cancel.clone();
        cancel.connect_clicked(move |_| {
            progress.request_cancel();
            cancel_btn.set_sensitive(false);
            cancel_btn.set_label("Cancelling…");
        });
    }
    window.present();

    // Poll progress into the label.
    let poll = {
        let progress = progress.clone();
        let label = label.clone();
        glib::timeout_add_local(Duration::from_millis(120), move || {
            let total = progress.total();
            if progress.is_cancelled() {
                label.set_text("Cancelling…");
            } else if total == 0 {
                label.set_text("Preparing…");
            } else {
                label.set_text(&format!("Exporting notes… {} / {}", progress.done(), total));
            }
            glib::ControlFlow::Continue
        })
    };
    let poll = Rc::new(RefCell::new(Some(poll)));

    let worker = {
        let progress = progress.clone();
        gio::spawn_blocking(move || export_secure_vault_to_standard(params, progress))
    };

    let state = state.clone();
    let widgets = widgets.clone();
    let pending = pending.clone();
    glib::spawn_future_local(async move {
        let outcome = worker.await;
        if let Some(source) = poll.borrow_mut().take() {
            source.remove();
        }
        window.close();
        if !widgets.sessions.is_current(session) {
            return;
        }
        match outcome {
            Ok(Ok(report)) => {
                present_export_success_dialog(report, &state, &widgets, &pending);
            }
            Ok(Err(senatorial_notes::Error::ExportCancelled)) => {
                widgets
                    .save_status
                    .set_label("Export cancelled. Nothing was written.");
            }
            Ok(Err(error)) => {
                let detail = format!("{error}");
                let dialog = gtk::AlertDialog::builder()
                    .modal(true)
                    .message("The export did not finish")
                    .detail(format!("{detail}\n\nThe Secure Vault was not changed."))
                    .buttons(vec!["OK"])
                    .build();
                dialog.choose(
                    Some(&widgets.window),
                    None::<&gio::Cancellable>,
                    move |_| {},
                );
            }
            Err(_) => {
                widgets
                    .save_status
                    .set_label("The export failed unexpectedly. The Secure Vault was not changed.");
            }
        }
    });
}

fn present_export_success_dialog(
    report: ExportReport,
    state: &Rc<RefCell<AppState>>,
    widgets: &Widgets,
    pending: &Rc<RefCell<PendingSaves>>,
) {
    let live = report.notes + report.snotes;
    let detail = format!(
        "Exported {live} note(s){} to {}. The Secure Vault is unchanged.",
        if report.trashed > 0 {
            format!(" and {} in Trash", report.trashed)
        } else {
            String::new()
        },
        report.destination.display()
    );
    let dialog = gtk::AlertDialog::builder()
        .modal(true)
        .message("Export complete")
        .detail(detail)
        .buttons(vec!["Close", "Open Exported Vault"])
        .cancel_button(0)
        .default_button(0)
        .build();
    let state = state.clone();
    let widgets = widgets.clone();
    let pending = pending.clone();
    let parent = widgets.window.clone();
    let destination = report.destination.clone();
    dialog.choose(Some(&parent), None::<&gio::Cancellable>, move |result| {
        if let Ok(1) = result {
            open_vault(&destination, false, &state, &widgets, &pending);
        }
    });
}

fn change_active_password(state: &Rc<RefCell<AppState>>, widgets: &Widgets) {
    if !persist_active(state, widgets, true) {
        return;
    }
    let relative = {
        state.borrow().active.as_ref().and_then(|active| {
            active
                .is_encrypted()
                .then(|| active.note().relative_path.clone())
        })
    };
    let Some(relative) = relative else {
        widgets
            .save_status
            .set_label("Unlock an encrypted note before changing its password.");
        return;
    };
    let state_for_change = state.clone();
    let widgets_for_change = widgets.clone();
    present_change_password_dialog(&widgets.window, "Change Note Password", move |passwords| {
        let Some((old_password, new_password)) = passwords else {
            return;
        };
        let vault = { state_for_change.borrow().vault.clone() };
        let Some(vault) = vault else {
            return;
        };
        match vault.change_encrypted_password(
            &relative,
            old_password.as_str(),
            new_password.as_str(),
        ) {
            Ok((note, stamp, session)) => {
                let id = note.metadata.id;
                {
                    let mut state = state_for_change.borrow_mut();
                    if let Some(mut previous) = state.active.take() {
                        previous.clear_sensitive();
                    }
                    if let Some(mut stale) = state.unlocked_cache.remove(&id) {
                        stale.clear_sensitive();
                    }
                    state.body_dirty = false;
                    state.title_dirty = false;
                    state.last_sensitive_activity = Some(Instant::now());
                    state.active = Some(ActiveDocument::Encrypted {
                        note,
                        stamp,
                        session,
                    });
                }
                refresh_watch_baseline(&state_for_change);
                widgets_for_change
                    .save_status
                    .set_label("Password changed · re-encrypted with a new key");
            }
            Err(error) => {
                let message = match error {
                    senatorial_notes::Error::DecryptionFailed => {
                        "The current password is incorrect. The note was not changed.".to_owned()
                    }
                    senatorial_notes::Error::WeakPassword(detail) => {
                        format!("The new password is too short. {detail}.")
                    }
                    other => format!("The password could not be changed: {other}"),
                };
                widgets_for_change.save_status.set_label(&message);
                show_error_dialog(&widgets_for_change.window, "Change Password", &message);
            }
        }
    });
}

fn remove_active_encryption(state: &Rc<RefCell<AppState>>, widgets: &Widgets) {
    if !persist_active(state, widgets, true) {
        return;
    }
    let relative = {
        state.borrow().active.as_ref().and_then(|active| {
            active
                .is_encrypted()
                .then(|| active.note().relative_path.clone())
        })
    };
    let Some(relative) = relative else {
        widgets
            .save_status
            .set_label("Unlock an encrypted note before removing encryption.");
        return;
    };
    let state_for_remove = state.clone();
    let widgets_for_remove = widgets.clone();
    let warning = gtk::AlertDialog::builder()
        .modal(true)
        .message("Remove encryption?")
        .detail(
            "This note will be stored as readable plaintext Markdown on disk. Anyone with access \
             to the file may be able to read its contents. You will be asked for the current \
             password to continue.",
        )
        .buttons(["Cancel", "Remove Encryption"])
        .cancel_button(0)
        .default_button(0)
        .build();
    let parent = widgets.window.clone();
    warning.choose(
        Some(&widgets.window),
        None::<&gio::Cancellable>,
        move |result| {
            if result != Ok(1) {
                return;
            }
            let state_for_remove = state_for_remove.clone();
            let widgets_for_remove = widgets_for_remove.clone();
            let relative = relative.clone();
            present_password_dialog(
                &parent,
                "Remove Encryption",
                "Enter the current password. This note will then be written as plaintext Markdown.",
                false,
                false,
                "Remove Encryption",
                move |password| {
                    let Some(password) = password else {
                        return;
                    };
                    let vault = { state_for_remove.borrow().vault.clone() };
                    let Some(vault) = vault else {
                        return;
                    };
                    match vault.remove_encryption(&relative, password.as_str()) {
                        Ok((note, stamp)) => {
                            let id = note.metadata.id;
                            {
                                let mut state = state_for_remove.borrow_mut();
                                if let Some(mut previous) = state.active.take() {
                                    previous.clear_sensitive();
                                }
                                if let Some(mut stale) = state.unlocked_cache.remove(&id) {
                                    stale.clear_sensitive();
                                }
                                state.body_dirty = false;
                                state.title_dirty = false;
                                state.active = Some(ActiveDocument::Plain { note, stamp });
                                state.last_sensitive_activity = None;
                            }
                            widgets_for_remove
                                .save_status
                                .set_label("Encryption removed · this note is plaintext on disk");
                            refresh_current_view(&state_for_remove, &widgets_for_remove);
                            reselect_active_note(&state_for_remove, &widgets_for_remove);
                            refresh_watch_baseline(&state_for_remove);
                        }
                        Err(error) => {
                            let message = match error {
                                senatorial_notes::Error::DecryptionFailed => {
                                    "The password is incorrect. Encryption was not removed."
                                        .to_owned()
                                }
                                other => {
                                    format!("Could not remove encryption: {other}")
                                }
                            };
                            widgets_for_remove.save_status.set_label(&message);
                            show_error_dialog(
                                &widgets_for_remove.window,
                                "Remove Encryption",
                                &message,
                            );
                        }
                    }
                },
            );
        },
    );
}

/// A modal password prompt that SenatorialNotes fully controls.
///
/// `adw::MessageDialog`/`adw::AlertDialog` close themselves the instant a
/// response button is activated and only then emit their signal, so in-dialog
/// re-validation is impossible: a rejected password would silently do nothing.
/// This prompt keeps itself open until the input is valid (or cancelled) and
/// shows the specific reason for every rejection.
fn present_password_dialog<F>(
    parent: &ApplicationWindow,
    heading: &str,
    body: &str,
    require_confirmation: bool,
    enforce_policy: bool,
    confirm_label: &str,
    callback: F,
) where
    F: FnOnce(Option<Zeroizing<String>>) + 'static,
{
    let window = adw::Window::builder()
        .transient_for(parent)
        .modal(true)
        .title(heading)
        .default_width(430)
        .resizable(false)
        .build();
    let header = adw::HeaderBar::new();
    header.set_show_start_title_buttons(false);
    header.set_show_end_title_buttons(false);
    let cancel = gtk::Button::with_label("Cancel");
    let confirm = gtk::Button::with_label(confirm_label);
    confirm.add_css_class("suggested-action");
    header.pack_start(&cancel);
    header.pack_end(&confirm);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 10);
    content.set_margin_start(18);
    content.set_margin_end(18);
    content.set_margin_top(18);
    content.set_margin_bottom(18);
    let body_label = gtk::Label::new(Some(body));
    body_label.set_wrap(true);
    body_label.set_xalign(0.0);
    content.append(&body_label);
    if enforce_policy {
        let policy = gtk::Label::new(Some(&format!(
            "Use a passphrase of at least {MIN_PASSWORD_LENGTH} characters.",
        )));
        policy.set_wrap(true);
        policy.set_xalign(0.0);
        policy.add_css_class("caption");
        policy.add_css_class("dim-label");
        content.append(&policy);
    }
    let password = password_entry("Password");
    content.append(&password);
    let confirmation = require_confirmation.then(|| {
        let entry = password_entry("Confirm password");
        content.append(&entry);
        entry
    });
    let error = gtk::Label::new(None);
    error.add_css_class("error");
    error.set_wrap(true);
    error.set_xalign(0.0);
    error.set_visible(false);
    content.append(&error);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&content));
    window.set_content(Some(&toolbar));
    window.set_default_widget(Some(&confirm));

    let callback = Rc::new(RefCell::new(Some(callback)));
    finish_on_close(&window, &callback);
    close_on_escape(&window);
    {
        let window = window.clone();
        let callback = callback.clone();
        cancel.connect_clicked(move |_| {
            // Drop the RefCell borrow (via take_completion) before touching GTK:
            // window.close() synchronously re-emits close-request into
            // finish_on_close, which borrows the same cell.
            let completion = take_completion(&callback);
            window.close();
            if let Some(completion) = completion {
                completion(None);
            }
        });
    }
    {
        let window = window.clone();
        let callback = callback.clone();
        let submit: Rc<dyn Fn()> = Rc::new(move || {
            let value = Zeroizing::new(password.text().to_string());
            if value.is_empty() {
                show_dialog_error(&error, "Enter a password.");
                return;
            }
            if let Some(entry) = &confirmation
                && entry.text() != password.text()
            {
                show_dialog_error(&error, "The passwords do not match.");
                return;
            }
            if enforce_policy && value.chars().count() < MIN_PASSWORD_LENGTH {
                show_dialog_error(
                    &error,
                    &format!(
                        "Password is too short. SenatorialNotes requires at least \
                         {MIN_PASSWORD_LENGTH} characters.",
                    ),
                );
                return;
            }
            // Take the completion (releasing the borrow) before window.close():
            // closing re-enters finish_on_close, which borrows the same cell.
            let completion = take_completion(&callback);
            if let Some(completion) = completion {
                window.close();
                completion(Some(value));
            }
        });
        confirm.connect_clicked(move |_| submit());
    }
    window.present();
}

/// A small controlled window collecting one line of text (a notebook name),
/// following the same "never a self-closing dialog" convention as
/// `present_password_dialog`.
fn present_text_entry_dialog<F>(
    parent: &ApplicationWindow,
    heading: &str,
    body: &str,
    placeholder: &str,
    initial_value: &str,
    confirm_label: &str,
    callback: F,
) where
    F: FnOnce(Option<String>) + 'static,
{
    let window = adw::Window::builder()
        .transient_for(parent)
        .modal(true)
        .title(heading)
        .default_width(380)
        .resizable(false)
        .build();
    let header = adw::HeaderBar::new();
    header.set_show_start_title_buttons(false);
    header.set_show_end_title_buttons(false);
    let cancel = gtk::Button::with_label("Cancel");
    let confirm = gtk::Button::with_label(confirm_label);
    confirm.add_css_class("suggested-action");
    header.pack_start(&cancel);
    header.pack_end(&confirm);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 10);
    content.set_margin_start(18);
    content.set_margin_end(18);
    content.set_margin_top(18);
    content.set_margin_bottom(18);
    if !body.is_empty() {
        let body_label = gtk::Label::new(Some(body));
        body_label.set_wrap(true);
        body_label.set_xalign(0.0);
        content.append(&body_label);
    }
    let entry = gtk::Entry::builder()
        .placeholder_text(placeholder)
        .text(initial_value)
        .build();
    content.append(&entry);
    let error = gtk::Label::new(None);
    error.add_css_class("error");
    error.set_wrap(true);
    error.set_xalign(0.0);
    error.set_visible(false);
    content.append(&error);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&content));
    window.set_content(Some(&toolbar));
    window.set_default_widget(Some(&confirm));

    let callback = Rc::new(RefCell::new(Some(callback)));
    finish_on_close(&window, &callback);
    close_on_escape(&window);
    {
        let window = window.clone();
        let callback = callback.clone();
        cancel.connect_clicked(move |_| {
            let completion = take_completion(&callback);
            window.close();
            if let Some(completion) = completion {
                completion(None);
            }
        });
    }
    {
        let window = window.clone();
        let callback = callback.clone();
        let entry_for_submit = entry.clone();
        let submit: Rc<dyn Fn()> = Rc::new(move || {
            let value = entry_for_submit.text().to_string();
            if value.trim().is_empty() {
                show_dialog_error(&error, "Enter a name.");
                return;
            }
            let completion = take_completion(&callback);
            if let Some(completion) = completion {
                window.close();
                completion(Some(value));
            }
        });
        confirm.connect_clicked({
            let submit = submit.clone();
            move |_| submit()
        });
        entry.connect_activate(move |_| submit());
    }
    window.present();
}

fn present_change_password_dialog<F>(parent: &ApplicationWindow, heading: &str, callback: F)
where
    F: FnOnce(Option<(Zeroizing<String>, Zeroizing<String>)>) + 'static,
{
    let window = adw::Window::builder()
        .transient_for(parent)
        .modal(true)
        .title(heading)
        .default_width(430)
        .resizable(false)
        .build();
    let header = adw::HeaderBar::new();
    header.set_show_start_title_buttons(false);
    header.set_show_end_title_buttons(false);
    let cancel = gtk::Button::with_label("Cancel");
    let confirm = gtk::Button::with_label(heading);
    confirm.add_css_class("suggested-action");
    header.pack_start(&cancel);
    header.pack_end(&confirm);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 10);
    content.set_margin_start(18);
    content.set_margin_end(18);
    content.set_margin_top(18);
    content.set_margin_bottom(18);
    let intro = gtk::Label::new(Some(
        "The note is re-encrypted with a new salt, key, and nonce. There is no password recovery.",
    ));
    intro.set_wrap(true);
    intro.set_xalign(0.0);
    content.append(&intro);
    let policy = gtk::Label::new(Some(&format!(
        "The new passphrase must be at least {MIN_PASSWORD_LENGTH} characters.",
    )));
    policy.set_wrap(true);
    policy.set_xalign(0.0);
    policy.add_css_class("caption");
    policy.add_css_class("dim-label");
    content.append(&policy);
    let old = password_entry("Current password");
    let new = password_entry("New password");
    let confirmation = password_entry("Confirm new password");
    content.append(&old);
    content.append(&new);
    content.append(&confirmation);
    let error = gtk::Label::new(None);
    error.add_css_class("error");
    error.set_wrap(true);
    error.set_xalign(0.0);
    error.set_visible(false);
    content.append(&error);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&content));
    window.set_content(Some(&toolbar));
    window.set_default_widget(Some(&confirm));

    let callback: Rc<RefCell<Option<F>>> = Rc::new(RefCell::new(Some(callback)));
    close_on_escape(&window);
    {
        let callback = callback.clone();
        window.connect_close_request(move |_| {
            if let Some(completion) = take_completion(&callback) {
                completion(None);
            }
            glib::Propagation::Proceed
        });
    }
    {
        let window = window.clone();
        let callback = callback.clone();
        cancel.connect_clicked(move |_| {
            let completion = take_completion(&callback);
            window.close();
            if let Some(completion) = completion {
                completion(None);
            }
        });
    }
    {
        let window = window.clone();
        let callback = callback.clone();
        let submit: Rc<dyn Fn()> = Rc::new(move || {
            let old_value = Zeroizing::new(old.text().to_string());
            let new_value = Zeroizing::new(new.text().to_string());
            if old_value.is_empty() {
                show_dialog_error(&error, "Enter the current password.");
                return;
            }
            if new_value.chars().count() < MIN_PASSWORD_LENGTH {
                show_dialog_error(
                    &error,
                    &format!(
                        "The new password is too short. SenatorialNotes requires at least \
                         {MIN_PASSWORD_LENGTH} characters.",
                    ),
                );
                return;
            }
            if new.text() != confirmation.text() {
                show_dialog_error(&error, "The new passwords do not match.");
                return;
            }
            let completion = take_completion(&callback);
            if let Some(completion) = completion {
                window.close();
                completion(Some((old_value, new_value)));
            }
        });
        confirm.connect_clicked(move |_| submit());
    }
    window.present();
}

fn finish_on_close<T, F>(window: &adw::Window, callback: &Rc<RefCell<Option<F>>>)
where
    F: FnOnce(Option<T>) + 'static,
{
    let callback = callback.clone();
    window.connect_close_request(move |_| {
        if let Some(completion) = take_completion(&callback) {
            completion(None);
        }
        glib::Propagation::Proceed
    });
}

/// Takes the pending completion out of its cell, ending the `RefCell` borrow
/// before the caller does anything re-entrant. `window.close()` synchronously
/// re-emits `close-request` (reaching `finish_on_close`), and a completion may
/// mutate widgets that emit further signals, so no borrow may outlive this call.
fn take_completion<T>(slot: &Rc<RefCell<Option<T>>>) -> Option<T> {
    slot.borrow_mut().take()
}

fn close_on_escape(window: &adw::Window) {
    let controller = gtk::EventControllerKey::new();
    let target = window.clone();
    controller.connect_key_pressed(move |_, key, _, _| {
        if key == gdk::Key::Escape {
            target.close();
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    window.add_controller(controller);
}

fn show_dialog_error(label: &gtk::Label, message: &str) {
    label.set_label(message);
    label.set_visible(true);
}

fn show_error_dialog(parent: &ApplicationWindow, title: &str, message: &str) {
    let dialog = gtk::AlertDialog::builder()
        .modal(true)
        .message(title)
        .detail(message)
        .build();
    dialog.show(Some(parent));
}

fn password_entry(placeholder: &str) -> gtk::PasswordEntry {
    let entry = gtk::PasswordEntry::builder()
        .placeholder_text(placeholder)
        .show_peek_icon(true)
        .activates_default(true)
        .build();
    entry.update_property(&[gtk::accessible::Property::Label(placeholder)]);
    entry
}

fn apply_appearance(config: &AppConfig, widgets: &Widgets) {
    let manager = adw::StyleManager::default();
    manager.set_color_scheme(match config.appearance.theme {
        Theme::System => adw::ColorScheme::Default,
        Theme::Light => adw::ColorScheme::ForceLight,
        Theme::Dark => adw::ColorScheme::ForceDark,
    });
    widgets
        .editor
        .set_show_line_numbers(config.appearance.show_line_numbers);
    widgets
        .editor
        .set_pixels_above_lines((config.appearance.editor_line_spacing / 2) as i32);
    widgets
        .editor
        .set_pixels_below_lines((config.appearance.editor_line_spacing / 2) as i32);
    let side_margin = match config.appearance.editor_content_width {
        0..=90 => 44,
        91..=114 => 26,
        _ => 14,
    };
    widgets.editor.set_left_margin(side_margin);
    widgets.editor.set_right_margin(side_margin);
    update_editor_scheme(&widgets.buffer, manager.is_dark());

    let font = config
        .appearance
        .editor_font_family
        .replace(['"', '\'', '\\'], "");
    let size = config.appearance.editor_font_size.clamp(10, 36);
    let accent = accent_colors(config.appearance.accent);
    let css = format!(
        ".editor-view {{ font-family: \"{font}\"; font-size: {size}px; }}\n\
         .brand-accent {{ color: {}; }}\n\
         .note-list row:selected {{ background-color: {}; }}\n\
         .sidebar-selected {{ background-color: {}; }}",
        accent.0, accent.1, accent.1
    );
    widgets.appearance_provider.load_from_data(&css);
}

fn accent_colors(accent: Accent) -> (&'static str, &'static str) {
    match accent {
        Accent::Blue => ("#3584e4", "alpha(#3584e4, 0.20)"),
        Accent::Teal => ("#2190a4", "alpha(#2190a4, 0.20)"),
        Accent::Green => ("#3a944a", "alpha(#3a944a, 0.20)"),
        Accent::Purple => ("#9141ac", "alpha(#9141ac, 0.20)"),
        Accent::Orange => ("#e66100", "alpha(#e66100, 0.20)"),
    }
}

fn update_editor_scheme(buffer: &sourceview5::Buffer, dark: bool) {
    let manager = sourceview5::StyleSchemeManager::default();
    let preferred = if dark { "Adwaita-dark" } else { "Adwaita" };
    let fallback = if dark { "oblivion" } else { "classic" };
    let scheme = manager
        .scheme(preferred)
        .or_else(|| manager.scheme(fallback));
    buffer.set_style_scheme(scheme.as_ref());
}

fn connect_theme_updates(widgets: &Widgets) {
    let buffer = widgets.buffer.clone();
    adw::StyleManager::default().connect_dark_notify(move |manager| {
        update_editor_scheme(&buffer, manager.is_dark());
    });
}

/// Shows title/notebook/tags/timestamps/pinned/archived/encryption/word-and-
/// character-count/vault-relative-path for the currently open (or, for a
/// locked encrypted note, currently selected) note, with the UUID tucked
/// into an "Advanced" expander out of the normal reading flow. A locked
/// note shows only what a locked `NoteSummary` actually knows - never a
/// guess at its protected fields (see the "Locked encrypted notes" note in
/// `SECURITY.md`).
fn present_note_info_dialog(id: Uuid, state: &Rc<RefCell<AppState>>, widgets: &Widgets) {
    let summary = {
        state
            .borrow()
            .notes
            .iter()
            .find(|summary| summary.id == id)
            .cloned()
    };
    let Some(summary) = summary else {
        return;
    };
    // A primitive-only snapshot, not a clone of the whole `Note` - the same
    // discipline `update_active_summary` already uses, so this dialog does
    // not keep an extra long-lived copy of decrypted content around.
    let active_snapshot = {
        state.borrow().active.as_ref().and_then(|active| {
            (active.id() == id).then(|| {
                let note = active.note();
                (
                    note.metadata.title.clone(),
                    note.relative_path.clone(),
                    note.metadata.tags.clone(),
                    note.metadata.created_at,
                    note.metadata.updated_at,
                    note.metadata.pinned,
                    note.metadata.archived,
                    note.body.split_whitespace().count(),
                    note.body.chars().count(),
                    active.is_encrypted(),
                )
            })
        })
    };

    let window = ApplicationWindow::builder()
        .transient_for(&widgets.window)
        .modal(true)
        .title("Note Information")
        .default_width(380)
        .default_height(if active_snapshot.is_some() { 580 } else { 260 })
        .build();
    {
        let target = window.clone();
        let controller = gtk::EventControllerKey::new();
        controller.connect_key_pressed(move |_, key, _, _| {
            if key == gdk::Key::Escape {
                target.close();
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        window.add_controller(controller);
    }

    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.append(&adw::HeaderBar::new());
    let content = gtk::Box::new(gtk::Orientation::Vertical, 10);
    content.set_margin_start(20);
    content.set_margin_end(20);
    content.set_margin_top(14);
    content.set_margin_bottom(20);

    match active_snapshot {
        None => {
            let notice = gtk::Label::new(Some(
                "This note is encrypted and locked. Unlock it to see its details.",
            ));
            notice.set_wrap(true);
            notice.set_xalign(0.0);
            content.append(&notice);
            content.append(&preference_row(
                "Encryption",
                &gtk::Label::new(Some("Encrypted · locked")),
            ));
            content.append(&info_advanced_expander(&summary.relative_path, id));
        }
        Some((
            title,
            relative_path,
            tags,
            created_at,
            updated_at,
            pinned,
            archived,
            words,
            characters,
            encrypted,
        )) => {
            content.append(&preference_row("Title", &gtk::Label::new(Some(&title))));
            // Reuses `ViewMode::heading`'s Inbox -> "Unfiled" display mapping
            // so this panel never shows the raw on-disk "Inbox" name the
            // sidebar no longer does.
            let notebook = relative_path
                .parent()
                .map(|parent| ViewMode::Notebook(parent.to_path_buf()).heading())
                .unwrap_or_else(|| ViewMode::Notebook(PathBuf::from("Inbox")).heading());
            content.append(&preference_row(
                "Notebook",
                &gtk::Label::new(Some(&notebook)),
            ));
            let tags = if tags.is_empty() {
                "None".to_string()
            } else {
                tags.join(", ")
            };
            content.append(&preference_row("Tags", &gtk::Label::new(Some(&tags))));
            content.append(&preference_row(
                "Created",
                &gtk::Label::new(Some(&format_timestamp(created_at))),
            ));
            content.append(&preference_row(
                "Modified",
                &gtk::Label::new(Some(&format_timestamp(updated_at))),
            ));
            content.append(&preference_row(
                "Pinned",
                &gtk::Label::new(Some(if pinned { "Yes" } else { "No" })),
            ));
            content.append(&preference_row(
                "Archived",
                &gtk::Label::new(Some(if archived { "Yes" } else { "No" })),
            ));
            content.append(&preference_row(
                "Encryption",
                &gtk::Label::new(Some(if encrypted {
                    "Encrypted · unlocked"
                } else {
                    "Not encrypted"
                })),
            ));
            content.append(&preference_row(
                "Word count",
                &gtk::Label::new(Some(&words.to_string())),
            ));
            content.append(&preference_row(
                "Character count",
                &gtk::Label::new(Some(&characters.to_string())),
            ));
            content.append(&info_advanced_expander(&relative_path, id));
        }
    }

    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&content)
        .build();
    root.append(&scroll);
    window.set_content(Some(&root));
    window.present();
}

/// The note's vault-relative location and UUID, tucked into a collapsed
/// expander since they matter far less often than the fields above it.
fn info_advanced_expander(relative_path: &Path, id: Uuid) -> gtk::Expander {
    let details = gtk::Box::new(gtk::Orientation::Vertical, 6);
    details.set_margin_top(8);
    let location = gtk::Label::new(Some(&relative_path.display().to_string()));
    location.set_wrap(true);
    location.set_xalign(0.0);
    details.append(&preference_row("Location in vault", &location));
    let uuid_label = gtk::Label::new(Some(&id.to_string()));
    uuid_label.set_selectable(true);
    uuid_label.set_xalign(0.0);
    details.append(&preference_row("UUID", &uuid_label));
    let expander = gtk::Expander::new(Some("Advanced"));
    expander.set_child(Some(&details));
    expander
}

fn format_timestamp(value: chrono::DateTime<chrono::Utc>) -> String {
    value.format("%Y-%m-%d %H:%M UTC").to_string()
}

/// A focused settings dialog for the open vault. A Secure Vault gets Auto-Lock,
/// Security and General groups; a Standard Vault gets only the General group
/// (display-name rename) and never any Secure-Vault security controls.
fn show_vault_settings(
    _application: &Application,
    state: &Rc<RefCell<AppState>>,
    widgets: &Widgets,
    _pending: &Rc<RefCell<PendingSaves>>,
) {
    let (has_vault, is_secure) = {
        let st = state.borrow();
        st.vault
            .as_ref()
            .map(|vault| (true, vault.is_encrypted()))
            .unwrap_or((false, false))
    };
    if !has_vault {
        return;
    }

    let window = adw::PreferencesWindow::builder()
        .transient_for(&widgets.window)
        .modal(true)
        .title(if is_secure {
            "Secure Vault Settings"
        } else {
            "Vault Settings"
        })
        .search_enabled(false)
        .default_width(460)
        .default_height(if is_secure { 520 } else { 220 })
        .build();
    let page = adw::PreferencesPage::new();

    if is_secure {
        let current = { state.borrow().config.clone() };
        let locking = current.encrypted_note_locking;

        let auto_lock = adw::PreferencesGroup::builder()
            .title("Auto-Lock")
            .description(
                "Automatically lock this Secure Vault (and any encrypted notes open in it).",
            )
            .build();

        let after = adw::SpinRow::builder()
            .title("Lock after inactivity")
            .subtitle("Minutes of inactivity before locking — 0 turns this off")
            .adjustment(&gtk::Adjustment::new(
                locking.after_minutes as f64,
                0.0,
                240.0,
                1.0,
                5.0,
                0.0,
            ))
            .build();
        {
            let state = state.clone();
            let widgets = widgets.clone();
            after.connect_value_notify(move |row| {
                state
                    .borrow_mut()
                    .config
                    .encrypted_note_locking
                    .after_minutes = row.value() as u32;
                save_and_apply_config(&state, &widgets);
            });
        }
        auto_lock.add(&after);

        let on_focus = adw::SwitchRow::builder()
            .title("Lock when the app loses focus")
            .active(locking.on_focus_loss)
            .build();
        {
            let state = state.clone();
            let widgets = widgets.clone();
            on_focus.connect_active_notify(move |row| {
                state
                    .borrow_mut()
                    .config
                    .encrypted_note_locking
                    .on_focus_loss = row.is_active();
                save_and_apply_config(&state, &widgets);
            });
        }
        auto_lock.add(&on_focus);

        let on_minimize = adw::SwitchRow::builder()
            .title("Lock when minimized")
            .active(locking.on_minimize)
            .build();
        {
            let state = state.clone();
            let widgets = widgets.clone();
            on_minimize.connect_active_notify(move |row| {
                state.borrow_mut().config.encrypted_note_locking.on_minimize = row.is_active();
                save_and_apply_config(&state, &widgets);
            });
        }
        auto_lock.add(&on_minimize);
        page.add(&auto_lock);

        let security = adw::PreferencesGroup::builder().title("Security").build();
        let change_password = adw::ActionRow::builder()
            .title("Change Vault Password…")
            .subtitle("Re-wraps the vault key; note contents are not re-encrypted")
            .activatable(true)
            .build();
        change_password.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
        {
            let window = window.clone();
            change_password.connect_activated(move |_| {
                let _ = window.activate_action("app.change-vault-password", None);
            });
        }
        security.add(&change_password);

        let export = adw::ActionRow::builder()
            .title("Export to Standard Vault…")
            .subtitle("Creates a new, unencrypted copy of every note in a folder you choose")
            .activatable(true)
            .build();
        export.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
        {
            let window = window.clone();
            export.connect_activated(move |_| {
                let _ = window.activate_action("app.export-standard-vault", None);
            });
        }
        security.add(&export);
        page.add(&security);
    }

    let general = adw::PreferencesGroup::builder().title("General").build();
    let rename = adw::ActionRow::builder()
        .title("Rename Vault…")
        .subtitle("Changes only the name shown in SenatorialNotes — the folder is not moved")
        .activatable(true)
        .build();
    rename.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
    {
        let window = window.clone();
        rename.connect_activated(move |_| {
            let _ = window.activate_action("app.rename-vault", None);
        });
    }
    general.add(&rename);
    page.add(&general);

    window.add(&page);
    window.present();
}

fn show_preferences(application: &Application, state: &Rc<RefCell<AppState>>, widgets: &Widgets) {
    let window = ApplicationWindow::builder()
        .application(application)
        .transient_for(&widgets.window)
        .modal(true)
        .title("Preferences")
        .default_width(620)
        .default_height(700)
        .build();
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let header = adw::HeaderBar::new();
    root.append(&header);
    let content = gtk::Box::new(gtk::Orientation::Vertical, 18);
    content.set_margin_start(24);
    content.set_margin_end(24);
    content.set_margin_top(20);
    content.set_margin_bottom(24);
    let appearance_heading = preference_heading("Appearance");
    content.append(&appearance_heading);
    let current = { state.borrow().config.clone() };

    let theme = gtk::DropDown::from_strings(&["Follow System", "Light", "Dark"]);
    theme.set_selected(match current.appearance.theme {
        Theme::System => 0,
        Theme::Light => 1,
        Theme::Dark => 2,
    });
    content.append(&preference_row("Theme", &theme));
    {
        let state = state.clone();
        let widgets = widgets.clone();
        theme.connect_selected_notify(move |drop_down| {
            {
                state.borrow_mut().config.appearance.theme = match drop_down.selected() {
                    1 => Theme::Light,
                    2 => Theme::Dark,
                    _ => Theme::System,
                };
            }
            save_and_apply_config(&state, &widgets);
        });
    }

    let font = gtk::Entry::builder()
        .text(&current.appearance.editor_font_family)
        .placeholder_text("Locally installed font family")
        .hexpand(true)
        .build();
    content.append(&preference_row("Editor font", &font));
    {
        let state = state.clone();
        let widgets = widgets.clone();
        font.connect_changed(move |entry| {
            let value = entry.text().trim().to_owned();
            if !value.is_empty() {
                {
                    state.borrow_mut().config.appearance.editor_font_family = value;
                }
                save_and_apply_config(&state, &widgets);
            }
        });
    }

    let font_size = gtk::SpinButton::with_range(10.0, 36.0, 1.0);
    font_size.set_value(current.appearance.editor_font_size as f64);
    content.append(&preference_row("Editor font size", &font_size));
    connect_u32_spin(&font_size, state, widgets, |config, value| {
        config.appearance.editor_font_size = value
    });
    let line_spacing = gtk::SpinButton::with_range(0.0, 16.0, 1.0);
    line_spacing.set_value(current.appearance.editor_line_spacing as f64);
    content.append(&preference_row("Editor line spacing", &line_spacing));
    connect_u32_spin(&line_spacing, state, widgets, |config, value| {
        config.appearance.editor_line_spacing = value
    });
    let content_width = gtk::DropDown::from_strings(&["Comfortable", "Wide", "Full width"]);
    content_width.set_selected(match current.appearance.editor_content_width {
        0..=90 => 0,
        91..=114 => 1,
        _ => 2,
    });
    content.append(&preference_row("Editor width", &content_width));
    {
        let state = state.clone();
        let widgets = widgets.clone();
        content_width.connect_selected_notify(move |drop_down| {
            {
                let mut state = state.borrow_mut();
                state.config.appearance.editor_content_width = match drop_down.selected() {
                    0 => 88,
                    2 => 120,
                    _ => 108,
                };
            }
            save_and_apply_config(&state, &widgets);
        });
    }
    let line_numbers = gtk::Switch::builder()
        .active(current.appearance.show_line_numbers)
        .valign(gtk::Align::Center)
        .build();
    content.append(&preference_row("Show line numbers", &line_numbers));
    {
        let state = state.clone();
        let widgets = widgets.clone();
        line_numbers.connect_active_notify(move |toggle| {
            {
                state.borrow_mut().config.appearance.show_line_numbers = toggle.is_active();
            }
            save_and_apply_config(&state, &widgets);
        });
    }
    let density = gtk::DropDown::from_strings(&["Compact", "Comfortable", "Spacious"]);
    density.set_selected(match current.appearance.note_list_density {
        NoteListDensity::Compact => 0,
        NoteListDensity::Comfortable => 1,
        NoteListDensity::Spacious => 2,
    });
    content.append(&preference_row("Note-list density", &density));
    {
        let state = state.clone();
        let widgets = widgets.clone();
        density.connect_selected_notify(move |drop_down| {
            {
                state.borrow_mut().config.appearance.note_list_density = match drop_down.selected()
                {
                    0 => NoteListDensity::Compact,
                    2 => NoteListDensity::Spacious,
                    _ => NoteListDensity::Comfortable,
                };
            }
            save_and_apply_config(&state, &widgets);
            render_note_list(&state, &widgets);
        });
    }
    let preview_length = gtk::SpinButton::with_range(40.0, 300.0, 10.0);
    preview_length.set_value(current.appearance.note_preview_length as f64);
    content.append(&preference_row("Note preview length", &preview_length));
    {
        let state = state.clone();
        let widgets = widgets.clone();
        preview_length.connect_value_changed(move |spin| {
            {
                state.borrow_mut().config.appearance.note_preview_length =
                    spin.value_as_int() as usize;
            }
            save_and_apply_config(&state, &widgets);
        });
    }
    let accent = gtk::DropDown::from_strings(&["Blue", "Teal", "Green", "Purple", "Orange"]);
    accent.set_selected(match current.appearance.accent {
        Accent::Blue => 0,
        Accent::Teal => 1,
        Accent::Green => 2,
        Accent::Purple => 3,
        Accent::Orange => 4,
    });
    content.append(&preference_row("Accent", &accent));
    {
        let state = state.clone();
        let widgets = widgets.clone();
        accent.connect_selected_notify(move |drop_down| {
            {
                state.borrow_mut().config.appearance.accent = match drop_down.selected() {
                    1 => Accent::Teal,
                    2 => Accent::Green,
                    3 => Accent::Purple,
                    4 => Accent::Orange,
                    _ => Accent::Blue,
                };
            }
            save_and_apply_config(&state, &widgets);
        });
    }

    content.append(&preference_heading("Secure Vault Auto-Lock"));
    let auto_lock_hint = gtk::Label::new(Some(
        "Automatically lock a Secure Vault (and any encrypted notes open in it) after \
         inactivity, focus loss, or minimize.",
    ));
    auto_lock_hint.set_wrap(true);
    auto_lock_hint.set_xalign(0.0);
    auto_lock_hint.add_css_class("dim-label");
    auto_lock_hint.add_css_class("caption");
    content.append(&auto_lock_hint);
    add_lock_switch(
        &content,
        "When the application loses focus",
        current.encrypted_note_locking.on_focus_loss,
        state,
        widgets,
        |config, value| config.encrypted_note_locking.on_focus_loss = value,
    );
    add_lock_switch(
        &content,
        "When the application is minimized",
        current.encrypted_note_locking.on_minimize,
        state,
        widgets,
        |config, value| config.encrypted_note_locking.on_minimize = value,
    );
    let minutes = gtk::SpinButton::with_range(0.0, 240.0, 1.0);
    minutes.set_value(current.encrypted_note_locking.after_minutes as f64);
    minutes.set_tooltip_text(Some("0 disables timed locking"));
    content.append(&preference_row("Lock after minutes (0 = off)", &minutes));
    connect_u32_spin(&minutes, state, widgets, |config, value| {
        config.encrypted_note_locking.after_minutes = value;
    });
    let exits = gtk::Switch::builder()
        .active(true)
        .sensitive(false)
        .valign(gtk::Align::Center)
        .build();
    content.append(&preference_row("When the application exits", &exits));

    content.append(&preference_heading("Encrypted Note Locking"));
    let note_lock_hint =
        gtk::Label::new(Some("Applies to notes that have their own note password."));
    note_lock_hint.set_wrap(true);
    note_lock_hint.set_xalign(0.0);
    note_lock_hint.add_css_class("dim-label");
    note_lock_hint.add_css_class("caption");
    content.append(&note_lock_hint);
    add_lock_switch(
        &content,
        "When switching away from the note",
        current.encrypted_note_locking.on_note_switch,
        state,
        widgets,
        |config, value| config.encrypted_note_locking.on_note_switch = value,
    );

    content.append(&preference_heading("Privacy"));
    let privacy = gtk::Label::new(Some(PRIVACY_STATEMENT));
    privacy.set_wrap(true);
    privacy.set_xalign(0.0);
    privacy.add_css_class("dim-label");
    content.append(&privacy);
    let password_policy = gtk::Label::new(Some(
        "Encrypted notes are protected at rest with Argon2id and XChaCha20-Poly1305. Passwords are never stored. A lost password makes the note unrecoverable. Full-disk encryption is still recommended.",
    ));
    password_policy.set_wrap(true);
    password_policy.set_xalign(0.0);
    password_policy.add_css_class("dim-label");
    content.append(&password_policy);

    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&content)
        .build();
    root.append(&scroll);
    window.set_content(Some(&root));
    window.present();
}

fn preference_heading(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.add_css_class("title-3");
    label.add_css_class("brand-accent");
    label
}

fn preference_row(label: &str, control: &impl IsA<gtk::Widget>) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 16);
    row.add_css_class("preference-row");
    let title = gtk::Label::new(Some(label));
    title.set_xalign(0.0);
    title.set_hexpand(true);
    row.append(&title);
    row.append(control);
    row
}

fn connect_u32_spin<F>(
    spin: &gtk::SpinButton,
    state: &Rc<RefCell<AppState>>,
    widgets: &Widgets,
    update: F,
) where
    F: Fn(&mut AppConfig, u32) + 'static,
{
    let state = state.clone();
    let widgets = widgets.clone();
    spin.connect_value_changed(move |spin| {
        {
            update(&mut state.borrow_mut().config, spin.value_as_int() as u32);
        }
        save_and_apply_config(&state, &widgets);
    });
}

fn add_lock_switch<F>(
    content: &gtk::Box,
    label: &str,
    initial: bool,
    state: &Rc<RefCell<AppState>>,
    widgets: &Widgets,
    update: F,
) where
    F: Fn(&mut AppConfig, bool) + 'static,
{
    let toggle = gtk::Switch::builder()
        .active(initial)
        .valign(gtk::Align::Center)
        .build();
    content.append(&preference_row(label, &toggle));
    let state = state.clone();
    let widgets = widgets.clone();
    toggle.connect_active_notify(move |toggle| {
        {
            update(&mut state.borrow_mut().config, toggle.is_active());
        }
        save_and_apply_config(&state, &widgets);
    });
}

fn save_and_apply_config(state: &Rc<RefCell<AppState>>, widgets: &Widgets) {
    let config = { state.borrow().config.clone() };
    apply_appearance(&config, widgets);
    if let Err(error) = config.save() {
        widgets.save_status.set_label(&format!(
            "Preferences changed but could not be saved: {error}"
        ));
    }
}

fn install_actions(
    application: &Application,
    state: Rc<RefCell<AppState>>,
    widgets: Widgets,
    pending: Rc<RefCell<PendingSaves>>,
) {
    let new_note = gio::SimpleAction::new("new-note", None);
    {
        let state = state.clone();
        let widgets = widgets.clone();
        let pending = pending.clone();
        new_note.connect_activate(move |_, _| create_new_note(&state, &widgets, &pending));
    }
    application.add_action(&new_note);
    application.set_accels_for_action("app.new-note", &["<Primary>n"]);

    let save = gio::SimpleAction::new("save", None);
    {
        let state = state.clone();
        let widgets = widgets.clone();
        let pending = pending.clone();
        save.connect_activate(move |_, _| {
            cancel_all_timers(&pending);
            persist_active(&state, &widgets, true);
        });
    }
    application.add_action(&save);
    application.set_accels_for_action("app.save", &["<Primary>s"]);

    let open = gio::SimpleAction::new("open-vault", None);
    {
        let state = state.clone();
        let widgets = widgets.clone();
        let pending = pending.clone();
        open.connect_activate(move |_, _| {
            present_vault_folder_picker(false, &state, &widgets, &pending);
        });
    }
    application.add_action(&open);
    application.set_accels_for_action("app.open-vault", &["<Primary>o"]);

    let create_vault = gio::SimpleAction::new("create-vault", None);
    {
        let state = state.clone();
        let widgets = widgets.clone();
        let pending = pending.clone();
        create_vault.connect_activate(move |_, _| {
            present_vault_folder_picker(true, &state, &widgets, &pending);
        });
    }
    application.add_action(&create_vault);

    let create_encrypted_vault = gio::SimpleAction::new("create-encrypted-vault", None);
    {
        let state = state.clone();
        let widgets = widgets.clone();
        let pending = pending.clone();
        create_encrypted_vault.connect_activate(move |_, _| {
            present_encrypted_vault_creator(&state, &widgets, &pending);
        });
    }
    application.add_action(&create_encrypted_vault);

    let preferences = gio::SimpleAction::new("preferences", None);
    {
        let application = application.clone();
        let state = state.clone();
        let widgets = widgets.clone();
        preferences.connect_activate(move |_, _| show_preferences(&application, &state, &widgets));
    }
    application.add_action(&preferences);
    application.set_accels_for_action("app.preferences", &["<Primary>comma"]);

    // A focused, per-vault settings dialog. For a Secure Vault it carries the
    // Auto-Lock, Security and General groups; for a Standard Vault it is just
    // the display-name rename - never any Secure-Vault security controls.
    let vault_settings = gio::SimpleAction::new("vault-settings", None);
    {
        let application = application.clone();
        let state = state.clone();
        let widgets = widgets.clone();
        let pending = pending.clone();
        vault_settings.connect_activate(move |_, _| {
            show_vault_settings(&application, &state, &widgets, &pending)
        });
    }
    application.add_action(&vault_settings);

    let focus_search = gio::SimpleAction::new("focus-search", None);
    {
        let search = widgets.search.clone();
        focus_search.connect_activate(move |_, _| {
            search.grab_focus();
        });
    }
    application.add_action(&focus_search);
    application.set_accels_for_action("app.focus-search", &["<Primary><Shift>f"]);

    let focus_note_list = gio::SimpleAction::new("focus-note-list", None);
    {
        let note_list = widgets.note_list.clone();
        focus_note_list.connect_activate(move |_, _| {
            note_list.grab_focus();
        });
    }
    application.add_action(&focus_note_list);
    application.set_accels_for_action("app.focus-note-list", &["<Primary>1"]);

    let focus_editor = gio::SimpleAction::new("focus-editor", None);
    {
        let editor = widgets.editor.clone();
        focus_editor.connect_activate(move |_, _| {
            editor.grab_focus();
        });
    }
    application.add_action(&focus_editor);
    application.set_accels_for_action("app.focus-editor", &["<Primary>2"]);

    let next_note = gio::SimpleAction::new("next-note", None);
    {
        let state = state.clone();
        let widgets = widgets.clone();
        next_note.connect_activate(move |_, _| select_adjacent_note(1, &state, &widgets));
    }
    application.add_action(&next_note);
    // Not <Alt>Down: GtkSourceView already binds Alt+Up/Down to its own
    // move-lines action, so a global accelerator there would only fire when
    // focus happens to be outside the editor - confusing and inconsistent.
    application.set_accels_for_action("app.next-note", &["<Primary>bracketright"]);

    let previous_note = gio::SimpleAction::new("previous-note", None);
    {
        let state = state.clone();
        let widgets = widgets.clone();
        previous_note.connect_activate(move |_, _| select_adjacent_note(-1, &state, &widgets));
    }
    application.add_action(&previous_note);
    application.set_accels_for_action("app.previous-note", &["<Primary>bracketleft"]);

    let toggle_pin = gio::SimpleAction::new("toggle-pin", None);
    {
        let state = state.clone();
        let widgets = widgets.clone();
        toggle_pin.connect_activate(move |_, _| {
            if let Some(id) = current_note_id(&state) {
                toggle_note_flag(NoteFlag::Pinned, id, &state, &widgets);
            }
        });
    }
    application.add_action(&toggle_pin);
    application.set_accels_for_action("app.toggle-pin", &["<Primary><Shift>p"]);

    let toggle_archived = gio::SimpleAction::new("toggle-archived", None);
    {
        let state = state.clone();
        let widgets = widgets.clone();
        toggle_archived.connect_activate(move |_, _| {
            if let Some(id) = current_note_id(&state) {
                toggle_note_flag(NoteFlag::Archived, id, &state, &widgets);
            }
        });
    }
    application.add_action(&toggle_archived);
    application.set_accels_for_action("app.toggle-archived", &["<Primary><Shift>a"]);

    let note_info = gio::SimpleAction::new("note-info", None);
    {
        let state = state.clone();
        let widgets = widgets.clone();
        note_info.connect_activate(move |_, _| {
            if let Some(id) = current_note_id(&state) {
                present_note_info_dialog(id, &state, &widgets);
            }
        });
    }
    application.add_action(&note_info);
    application.set_accels_for_action("app.note-info", &["<Alt>Return"]);

    let context_note_info =
        gio::SimpleAction::new("context-note-info", Some(glib::VariantTy::STRING));
    {
        let state = state.clone();
        let widgets = widgets.clone();
        context_note_info.connect_activate(move |_, parameter| {
            if let Some(id) = uuid_parameter(parameter) {
                present_note_info_dialog(id, &state, &widgets);
            }
        });
    }
    application.add_action(&context_note_info);

    let move_to_trash = gio::SimpleAction::new("move-to-trash", None);
    {
        let state = state.clone();
        let widgets = widgets.clone();
        let pending = pending.clone();
        move_to_trash.connect_activate(move |_, _| {
            cancel_all_timers(&pending);
            move_selected_to_trash(&state, &widgets);
        });
    }
    application.add_action(&move_to_trash);

    let restore = gio::SimpleAction::new("restore-note", None);
    {
        let state = state.clone();
        let widgets = widgets.clone();
        restore.connect_activate(move |_, _| restore_selected(&state, &widgets));
    }
    application.add_action(&restore);

    let permanent = gio::SimpleAction::new("permanently-delete", None);
    {
        let state = state.clone();
        let widgets = widgets.clone();
        permanent.connect_activate(move |_, _| confirm_permanent_delete(&state, &widgets));
    }
    application.add_action(&permanent);

    let empty = gio::SimpleAction::new("empty-trash", None);
    {
        let state = state.clone();
        let widgets = widgets.clone();
        empty.connect_activate(move |_, _| confirm_empty_trash(&state, &widgets));
    }
    application.add_action(&empty);

    let encrypt = gio::SimpleAction::new("encrypt-note", None);
    {
        let state = state.clone();
        let widgets = widgets.clone();
        let pending = pending.clone();
        encrypt.connect_activate(move |_, _| {
            cancel_all_timers(&pending);
            encrypt_active_note(&state, &widgets);
        });
    }
    application.add_action(&encrypt);

    // Vault-level: lock the whole Secure Vault. Disabled (and so not
    // invokable) unless the current vault is an unlocked Secure Vault.
    let lock_vault_action = gio::SimpleAction::new("lock-vault", None);
    {
        let state = state.clone();
        let widgets = widgets.clone();
        let pending = pending.clone();
        lock_vault_action.connect_activate(move |_, _| {
            cancel_all_timers(&pending);
            lock_vault(&state, &widgets, &pending);
        });
    }
    application.add_action(&lock_vault_action);

    // Note-level: lock any individually encrypted notes that are currently
    // unlocked in this session. Never touches the Secure Vault key.
    let lock_note_action = gio::SimpleAction::new("lock-note", None);
    {
        let state = state.clone();
        let widgets = widgets.clone();
        let pending = pending.clone();
        lock_note_action.connect_activate(move |_, _| {
            cancel_all_timers(&pending);
            lock_all_encrypted(&state, &widgets);
        });
    }
    application.add_action(&lock_note_action);

    let change_password = gio::SimpleAction::new("change-password", None);
    {
        let state = state.clone();
        let widgets = widgets.clone();
        let pending = pending.clone();
        change_password.connect_activate(move |_, _| {
            cancel_all_timers(&pending);
            change_active_password(&state, &widgets);
        });
    }
    application.add_action(&change_password);

    let change_vault_password = gio::SimpleAction::new("change-vault-password", None);
    {
        let state = state.clone();
        let widgets = widgets.clone();
        let pending = pending.clone();
        change_vault_password.connect_activate(move |_, _| {
            cancel_all_timers(&pending);
            change_vault_password_flow(&state, &widgets, &pending);
        });
    }
    application.add_action(&change_vault_password);

    let rename_vault = gio::SimpleAction::new("rename-vault", None);
    {
        let state = state.clone();
        let widgets = widgets.clone();
        let pending = pending.clone();
        rename_vault.connect_activate(move |_, _| {
            rename_current_vault(&state, &widgets, &pending);
        });
    }
    application.add_action(&rename_vault);

    let export_standard_vault = gio::SimpleAction::new("export-standard-vault", None);
    {
        let state = state.clone();
        let widgets = widgets.clone();
        let pending = pending.clone();
        export_standard_vault.connect_activate(move |_, _| {
            cancel_all_timers(&pending);
            present_export_to_standard(&state, &widgets, &pending);
        });
    }
    application.add_action(&export_standard_vault);

    let remove_encryption = gio::SimpleAction::new("remove-encryption", None);
    {
        let state = state.clone();
        let widgets = widgets.clone();
        let pending = pending.clone();
        remove_encryption.connect_activate(move |_, _| {
            cancel_all_timers(&pending);
            remove_active_encryption(&state, &widgets);
        });
    }
    application.add_action(&remove_encryption);

    let context_rename = gio::SimpleAction::new("context-rename", Some(glib::VariantTy::STRING));
    {
        let state = state.clone();
        let widgets = widgets.clone();
        let pending = pending.clone();
        context_rename.connect_activate(move |_, parameter| {
            if let Some(id) = uuid_parameter(parameter) {
                rename_note_by_id(id, &state, &widgets, &pending);
            }
        });
    }
    application.add_action(&context_rename);

    let context_toggle_pin =
        gio::SimpleAction::new("context-toggle-pin", Some(glib::VariantTy::STRING));
    {
        let state = state.clone();
        let widgets = widgets.clone();
        context_toggle_pin.connect_activate(move |_, parameter| {
            if let Some(id) = uuid_parameter(parameter) {
                toggle_note_flag(NoteFlag::Pinned, id, &state, &widgets);
            }
        });
    }
    application.add_action(&context_toggle_pin);

    let context_toggle_favourite =
        gio::SimpleAction::new("context-toggle-favourite", Some(glib::VariantTy::STRING));
    {
        let state = state.clone();
        let widgets = widgets.clone();
        context_toggle_favourite.connect_activate(move |_, parameter| {
            if let Some(id) = uuid_parameter(parameter) {
                toggle_note_flag(NoteFlag::Favourite, id, &state, &widgets);
            }
        });
    }
    application.add_action(&context_toggle_favourite);

    let context_toggle_archived =
        gio::SimpleAction::new("context-toggle-archived", Some(glib::VariantTy::STRING));
    {
        let state = state.clone();
        let widgets = widgets.clone();
        context_toggle_archived.connect_activate(move |_, parameter| {
            if let Some(id) = uuid_parameter(parameter) {
                toggle_note_flag(NoteFlag::Archived, id, &state, &widgets);
            }
        });
    }
    application.add_action(&context_toggle_archived);

    let context_move_to_notebook =
        gio::SimpleAction::new("context-move-to-notebook", Some(glib::VariantTy::STRING));
    {
        let state = state.clone();
        let widgets = widgets.clone();
        context_move_to_notebook.connect_activate(move |_, parameter| {
            if let Some(id) = uuid_parameter(parameter) {
                present_move_to_notebook_dialog(id, &state, &widgets);
            }
        });
    }
    application.add_action(&context_move_to_notebook);

    let new_child_notebook =
        gio::SimpleAction::new("new-child-notebook", Some(glib::VariantTy::STRING));
    {
        let state = state.clone();
        let widgets = widgets.clone();
        new_child_notebook.connect_activate(move |_, parameter| {
            if let Some(path) = parameter.and_then(|value| value.str()) {
                present_new_notebook_dialog(Some(PathBuf::from(path)), &state, &widgets);
            }
        });
    }
    application.add_action(&new_child_notebook);

    let rename_notebook_action =
        gio::SimpleAction::new("rename-notebook", Some(glib::VariantTy::STRING));
    {
        let state = state.clone();
        let widgets = widgets.clone();
        rename_notebook_action.connect_activate(move |_, parameter| {
            if let Some(path) = parameter.and_then(|value| value.str()) {
                present_rename_notebook_dialog(PathBuf::from(path), &state, &widgets);
            }
        });
    }
    application.add_action(&rename_notebook_action);

    let delete_notebook_action =
        gio::SimpleAction::new("delete-notebook", Some(glib::VariantTy::STRING));
    {
        let state = state.clone();
        let widgets = widgets.clone();
        delete_notebook_action.connect_activate(move |_, parameter| {
            if let Some(path) = parameter.and_then(|value| value.str()) {
                confirm_delete_notebook(PathBuf::from(path), &state, &widgets);
            }
        });
    }
    application.add_action(&delete_notebook_action);

    let new_notebook = gio::SimpleAction::new("new-notebook", None);
    {
        let state = state.clone();
        let widgets = widgets.clone();
        new_notebook.connect_activate(move |_, _| {
            present_new_notebook_dialog(None, &state, &widgets);
        });
    }
    application.add_action(&new_notebook);
    application.set_accels_for_action("app.new-notebook", &["<Primary><Shift>n"]);

    let initial_sort_target = match state.borrow().config.sort_order {
        Some(SortOrder::LastEdited) | None => "last-edited",
        Some(SortOrder::DateCreated) => "date-created",
        Some(SortOrder::TitleAsc) => "title-asc",
        Some(SortOrder::TitleZa) => "title-za",
    };
    let set_sort_order = gio::SimpleAction::new_stateful(
        "set-sort-order",
        Some(glib::VariantTy::STRING),
        &initial_sort_target.to_variant(),
    );
    {
        let state = state.clone();
        let widgets = widgets.clone();
        set_sort_order.connect_activate(move |action, parameter| {
            let Some(target) = parameter.and_then(|value| value.str()) else {
                return;
            };
            let order = match target {
                "last-edited" => SortOrder::LastEdited,
                "date-created" => SortOrder::DateCreated,
                "title-asc" => SortOrder::TitleAsc,
                "title-za" => SortOrder::TitleZa,
                _ => return,
            };
            action.set_state(&target.to_variant());
            {
                let mut state = state.borrow_mut();
                state.config.sort_order = Some(order);
                let _ = state.config.save();
            }
            render_note_list(&state, &widgets);
        });
    }
    application.add_action(&set_sort_order);

    let context_encrypt = gio::SimpleAction::new("context-encrypt", Some(glib::VariantTy::STRING));
    {
        let state = state.clone();
        let widgets = widgets.clone();
        let pending = pending.clone();
        context_encrypt.connect_activate(move |_, parameter| {
            if let Some(id) = uuid_parameter(parameter) {
                encrypt_note_by_id(id, &state, &widgets, &pending);
            }
        });
    }
    application.add_action(&context_encrypt);

    let context_move_to_trash =
        gio::SimpleAction::new("context-move-to-trash", Some(glib::VariantTy::STRING));
    {
        let state = state.clone();
        let widgets = widgets.clone();
        context_move_to_trash.connect_activate(move |_, parameter| {
            if let Some(id) = uuid_parameter(parameter) {
                move_note_to_trash_by_id(id, &state, &widgets);
            }
        });
    }
    application.add_action(&context_move_to_trash);

    let context_restore = gio::SimpleAction::new("context-restore", Some(glib::VariantTy::STRING));
    {
        let state = state.clone();
        let widgets = widgets.clone();
        context_restore.connect_activate(move |_, parameter| {
            if let Some(id) = uuid_parameter(parameter) {
                restore_note_by_id(id, &state, &widgets);
            }
        });
    }
    application.add_action(&context_restore);

    let context_permanently_delete =
        gio::SimpleAction::new("context-permanently-delete", Some(glib::VariantTy::STRING));
    {
        let state = state.clone();
        let widgets = widgets.clone();
        context_permanently_delete.connect_activate(move |_, parameter| {
            if let Some(id) = uuid_parameter(parameter) {
                confirm_permanent_delete_by_id(id, &state, &widgets);
            }
        });
    }
    application.add_action(&context_permanently_delete);

    install_format_actions(application, &widgets);

    let about = gio::SimpleAction::new("about", None);
    {
        let window = widgets.window.clone();
        about.connect_activate(move |_, _| {
            let dialog = gtk::AboutDialog::builder()
                .program_name(APP_NAME)
                .version(env!("CARGO_PKG_VERSION"))
                .comments(PRIVACY_STATEMENT)
                .website("https://github.com/SenatorialNotes/SenatorialNotes")
                .license_type(gtk::License::Gpl30)
                .transient_for(&window)
                .modal(true)
                .build();
            dialog.present();
        });
    }
    application.add_action(&about);

    let quit = gio::SimpleAction::new("quit", None);
    {
        let window = widgets.window.clone();
        quit.connect_activate(move |_, _| window.close());
    }
    application.add_action(&quit);
    application.set_accels_for_action("app.quit", &["<Primary>q"]);
}

fn uuid_parameter(parameter: Option<&glib::Variant>) -> Option<Uuid> {
    parameter
        .and_then(|value| value.str())
        .and_then(|value| Uuid::parse_str(value).ok())
}

fn install_format_actions(application: &Application, widgets: &Widgets) {
    let actions = [
        ("format-normal", FormatAction::Normal),
        ("format-heading-1", FormatAction::Heading1),
        ("format-heading-2", FormatAction::Heading2),
        ("format-heading-3", FormatAction::Heading3),
        ("format-bold", FormatAction::Bold),
        ("format-italic", FormatAction::Italic),
        ("format-strikethrough", FormatAction::Strikethrough),
        ("format-highlight", FormatAction::Highlight),
        ("format-inline-code", FormatAction::InlineCode),
        ("format-code-block", FormatAction::CodeBlock),
        ("format-quote", FormatAction::Quote),
        ("format-bulleted-list", FormatAction::BulletedList),
        ("format-numbered-list", FormatAction::NumberedList),
        ("format-checklist", FormatAction::Checklist),
        ("format-link", FormatAction::Link),
        ("format-divider", FormatAction::HorizontalDivider),
    ];
    for (name, format) in actions {
        let action = gio::SimpleAction::new(name, None);
        let buffer = widgets.buffer.clone();
        let editor = widgets.editor.clone();
        action.connect_activate(move |_, _| {
            apply_format_to_buffer(&buffer, &editor, format);
        });
        application.add_action(&action);
    }
    application.set_accels_for_action("app.format-bold", &["<Primary>b"]);
    application.set_accels_for_action("app.format-italic", &["<Primary>i"]);
    application.set_accels_for_action("app.format-link", &["<Primary>k"]);
}

/// Replaces the whole buffer without entering the user's undo history.
///
/// Programmatic note loading runs inside selection/gesture callbacks where
/// GtkSourceView still considers a user action active. `set_text` begins an
/// irreversible action, which warns ("Cannot begin irreversible action while in
/// user action") when a user action is open. Disabling undo for the duration
/// avoids both the warning and polluting the undo stack, then the previous
/// setting is restored immediately.
fn set_buffer_text_silently(buffer: &sourceview5::Buffer, text: &str) {
    let undo_was_enabled = buffer.enables_undo();
    buffer.set_enable_undo(false);
    buffer.set_text(text);
    buffer.set_enable_undo(undo_was_enabled);
}

fn apply_format_to_buffer(
    buffer: &sourceview5::Buffer,
    editor: &sourceview5::View,
    action: FormatAction,
) {
    let start_iter = buffer.start_iter();
    let end_iter = buffer.end_iter();
    let source = buffer.text(&start_iter, &end_iter, true).to_string();
    let (start_chars, end_chars) = buffer
        .selection_bounds()
        .map(|(start, end)| (start.offset() as usize, end.offset() as usize))
        .unwrap_or_else(|| {
            let cursor = buffer.cursor_position() as usize;
            (cursor, cursor)
        });
    let start = char_to_byte(&source, start_chars);
    let end = char_to_byte(&source, end_chars);
    let edit = apply_markdown_format(&source, start, end, action);

    // Replace only the span that actually changed, as one user action. A full
    // `set_text` here would clear the undo history and nest an irreversible
    // action inside the active user action.
    let (mut prefix, mut suffix) = common_affix_bytes(&source, &edit.text);
    while prefix > 0 && !source.is_char_boundary(prefix) {
        prefix -= 1;
    }
    while suffix > 0
        && (!source.is_char_boundary(source.len() - suffix)
            || !edit.text.is_char_boundary(edit.text.len() - suffix))
    {
        suffix -= 1;
    }
    let replaced_start = byte_to_char(&source, prefix) as i32;
    let replaced_end = byte_to_char(&source, source.len() - suffix) as i32;
    let replacement = &edit.text[prefix..edit.text.len() - suffix];
    let selection_start = byte_to_char(&edit.text, edit.selection_start) as i32;
    let selection_end = byte_to_char(&edit.text, edit.selection_end) as i32;

    buffer.begin_user_action();
    let mut delete_start = buffer.iter_at_offset(replaced_start);
    let mut delete_end = buffer.iter_at_offset(replaced_end);
    buffer.delete(&mut delete_start, &mut delete_end);
    let mut insert_at = buffer.iter_at_offset(replaced_start);
    buffer.insert(&mut insert_at, replacement);
    buffer.end_user_action();

    let start = buffer.iter_at_offset(selection_start);
    let end = buffer.iter_at_offset(selection_end);
    buffer.select_range(&start, &end);
    editor.grab_focus();
}

fn common_affix_bytes(a: &str, b: &str) -> (usize, usize) {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let max = a.len().min(b.len());
    let mut prefix = 0;
    while prefix < max && a[prefix] == b[prefix] {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < max - prefix && a[a.len() - 1 - suffix] == b[b.len() - 1 - suffix] {
        suffix += 1;
    }
    (prefix, suffix)
}

fn char_to_byte(value: &str, char_index: usize) -> usize {
    value
        .char_indices()
        .nth(char_index)
        .map_or(value.len(), |(index, _)| index)
}

fn byte_to_char(value: &str, byte_index: usize) -> usize {
    value[..byte_index.min(value.len())].chars().count()
}

// --- Editor V2: live-preview Markdown rendering ---------------------------
//
// The GtkSourceView buffer holds literal Markdown at all times, exactly as
// it always has - saving, loading, undo/redo, and encryption are completely
// unaffected by anything below. This is a purely presentational layer: a
// debounced pass reads the buffer's text, asks `markdown_spans` what should
// be styled, and applies a fixed set of `GtkTextTag`s. Marker punctuation is
// dimmed, never hidden - Stage C.0 found that `GtkTextTag:invisible` crashes
// GTK4's own text b-tree when combined with programmatic cursor movement
// (see the plan/completion report), so this design uses no invisible text,
// no custom cursor handling, and no buffer transformation of any kind.

const MARKDOWN_MARKER_TAG: &str = "md-marker";
const MARKDOWN_STYLE_TAGS: &[&str] = &[
    "md-h1",
    "md-h2",
    "md-h3",
    "md-bold",
    "md-italic",
    "md-strike",
    "md-highlight",
    "md-code",
    "md-codeblock",
    "md-quote",
    "md-link",
    "md-checked",
];

/// Registers the fixed tag set once, at buffer construction. Every property
/// used here is purely visual (weight, style, scale, colour, strikethrough,
/// underline, margin) - never `invisible`. Marker punctuation is dimmed by
/// reducing the alpha of an otherwise mid-grey foreground, which reads as
/// "muted" against both light and dark themes without hardcoding a colour
/// that would look wrong in one of them.
fn register_markdown_style_tags(buffer: &sourceview5::Buffer) {
    let table = buffer.tag_table();

    let marker = gtk::TextTag::builder().name(MARKDOWN_MARKER_TAG).build();
    marker.set_foreground_rgba(Some(&gdk::RGBA::new(0.5, 0.5, 0.5, 0.6)));
    table.add(&marker);

    let h1 = gtk::TextTag::builder()
        .name("md-h1")
        .weight(700)
        .scale(1.42)
        .build();
    table.add(&h1);
    let h2 = gtk::TextTag::builder()
        .name("md-h2")
        .weight(700)
        .scale(1.24)
        .build();
    table.add(&h2);
    let h3 = gtk::TextTag::builder()
        .name("md-h3")
        .weight(700)
        .scale(1.1)
        .build();
    table.add(&h3);
    let bold = gtk::TextTag::builder().name("md-bold").weight(700).build();
    table.add(&bold);
    let italic = gtk::TextTag::builder()
        .name("md-italic")
        .style(gtk::pango::Style::Italic)
        .build();
    table.add(&italic);
    let strike = gtk::TextTag::builder()
        .name("md-strike")
        .strikethrough(true)
        .build();
    table.add(&strike);
    let highlight = gtk::TextTag::builder().name("md-highlight").build();
    highlight.set_background_rgba(Some(&gdk::RGBA::new(0.95, 0.83, 0.25, 0.35)));
    table.add(&highlight);
    let code = gtk::TextTag::builder()
        .name("md-code")
        .family("monospace")
        .build();
    code.set_background_rgba(Some(&gdk::RGBA::new(0.5, 0.5, 0.5, 0.16)));
    table.add(&code);
    let code_block = gtk::TextTag::builder()
        .name("md-codeblock")
        .family("monospace")
        .build();
    code_block.set_background_rgba(Some(&gdk::RGBA::new(0.5, 0.5, 0.5, 0.16)));
    table.add(&code_block);
    let quote = gtk::TextTag::builder()
        .name("md-quote")
        .style(gtk::pango::Style::Italic)
        .left_margin(16)
        .build();
    quote.set_foreground_rgba(Some(&gdk::RGBA::new(0.5, 0.5, 0.5, 0.85)));
    table.add(&quote);
    let link = gtk::TextTag::builder()
        .name("md-link")
        .underline(gtk::pango::Underline::Single)
        .build();
    link.set_foreground_rgba(Some(&gdk::RGBA::new(0.32, 0.5, 0.86, 1.0)));
    table.add(&link);
    let checked = gtk::TextTag::builder()
        .name("md-checked")
        .strikethrough(true)
        .build();
    checked.set_foreground_rgba(Some(&gdk::RGBA::new(0.5, 0.5, 0.5, 0.85)));
    table.add(&checked);
}

fn content_tag_names(kind: SpanKind) -> &'static [&'static str] {
    match kind {
        SpanKind::Heading1 => &["md-h1"],
        SpanKind::Heading2 => &["md-h2"],
        SpanKind::Heading3 => &["md-h3"],
        SpanKind::Bold => &["md-bold"],
        SpanKind::Italic => &["md-italic"],
        SpanKind::Strikethrough => &["md-strike"],
        SpanKind::Highlight => &["md-highlight"],
        SpanKind::InlineCode => &["md-code"],
        SpanKind::CodeBlock => &["md-codeblock"],
        SpanKind::Quote => &["md-quote"],
        SpanKind::Link => &["md-link"],
        SpanKind::ChecklistItem { checked: true } => &["md-checked"],
        SpanKind::ChecklistItem { checked: false }
        | SpanKind::BulletItem
        | SpanKind::NumberedItem
        | SpanKind::Divider => &[],
    }
}

/// Recomputes every Markdown style tag over the whole buffer. Whole-buffer
/// rather than incrementally invalidated on purpose: a delimiter change can
/// affect styling beyond the edit point, and a stale tag is worse than a
/// slightly wider recompute. Only ever calls `apply_tag_by_name`/
/// `remove_tag_by_name` - GTK documents `changed` as firing for content
/// changes and `apply-tag`/`remove-tag` as separate signals with their own
/// default handlers, and the modified bit likewise only tracks content
/// edits, so this can never mark the buffer modified, trigger autosave, or
/// create an undo step for the user's actual text.
fn recompute_markdown_styles(buffer: &sourceview5::Buffer) {
    let start = buffer.start_iter();
    let end = buffer.end_iter();
    let text = buffer.text(&start, &end, true).to_string();

    let clear_start = buffer.start_iter();
    let clear_end = buffer.end_iter();
    buffer.remove_tag_by_name(MARKDOWN_MARKER_TAG, &clear_start, &clear_end);
    for tag in MARKDOWN_STYLE_TAGS {
        buffer.remove_tag_by_name(tag, &clear_start, &clear_end);
    }

    for span in markdown_spans::compute_spans(&text) {
        for marker in &span.marker_ranges {
            apply_tag_range(buffer, MARKDOWN_MARKER_TAG, &text, marker.clone());
        }
        for tag in content_tag_names(span.kind) {
            apply_tag_range(buffer, tag, &text, span.content_range.clone());
        }
    }
}

fn apply_tag_range(
    buffer: &sourceview5::Buffer,
    tag: &str,
    text: &str,
    range: std::ops::Range<usize>,
) {
    if range.start >= range.end {
        return;
    }
    let start_char = byte_to_char(text, range.start) as i32;
    let end_char = byte_to_char(text, range.end) as i32;
    let start_iter = buffer.iter_at_offset(start_char);
    let end_iter = buffer.iter_at_offset(end_char);
    buffer.apply_tag_by_name(tag, &start_iter, &end_iter);
}

/// Debounces a style recompute after a real text edit. A short delay -
/// quick enough to feel immediate, long enough that fast typing does not
/// trigger a rescan on every keystroke.
fn schedule_style_recompute(widgets: &Widgets) {
    if let Some(source) = widgets.style_recompute_source.borrow_mut().take() {
        source.remove();
    }
    let buffer = widgets.buffer.clone();
    let source_slot = widgets.style_recompute_source.clone();
    let source = glib::timeout_add_local_once(Duration::from_millis(120), move || {
        source_slot.borrow_mut().take();
        recompute_markdown_styles(&buffer);
    });
    *widgets.style_recompute_source.borrow_mut() = Some(source);
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ActiveFormats {
    bold: bool,
    italic: bool,
}

/// Determines which formats the toolbar should show as active, following
/// the agreed rules precisely rather than a naive tag check at the raw
/// cursor offset:
///
/// - A non-empty selection: active only if the format applies uniformly
///   across the *entire* selection (the same rule `toggle_wrap` in
///   `formatting.rs` already uses to decide "already applied").
/// - No selection: the character immediately *before* the cursor
///   (left-gravity, matching GTK's own `insert_at_cursor` tag-inheritance
///   convention), falling back to the character immediately after only at
///   the very start of a line/buffer.
/// - An empty paragraph has no tagged character to inspect either way, so
///   it always reads as inactive here; a future "sticky" toolbar state
///   would be separate, explicit UI state, not something inferred from
///   absent tags.
/// - Always re-derived from the buffer's current tags, including after
///   undo/redo - no separate toolbar state can desync from the buffer.
fn active_formats_at(buffer: &sourceview5::Buffer) -> ActiveFormats {
    if let Some((start, end)) = buffer.selection_bounds() {
        return ActiveFormats {
            bold: tag_covers_range(buffer, "md-bold", &start, &end),
            italic: tag_covers_range(buffer, "md-italic", &start, &end),
        };
    }
    let cursor = buffer.iter_at_mark(&buffer.get_insert());
    let probe = if cursor.starts_line() {
        cursor
    } else {
        let mut before = cursor;
        before.backward_char();
        before
    };
    ActiveFormats {
        bold: probe.has_tag(&tag_by_name(buffer, "md-bold")),
        italic: probe.has_tag(&tag_by_name(buffer, "md-italic")),
    }
}

fn tag_by_name(buffer: &sourceview5::Buffer, name: &str) -> gtk::TextTag {
    buffer
        .tag_table()
        .lookup(name)
        .unwrap_or_else(|| gtk::TextTag::builder().name(name).build())
}

fn tag_covers_range(
    buffer: &sourceview5::Buffer,
    name: &str,
    start: &gtk::TextIter,
    end: &gtk::TextIter,
) -> bool {
    let tag = tag_by_name(buffer, name);
    let mut probe = *start;
    while probe < *end {
        if !probe.has_tag(&tag) {
            return false;
        }
        if !probe.forward_char() {
            break;
        }
    }
    true
}

/// Reflects `active_formats_at` on the Bold/Italic toolbar buttons via the
/// existing theme/accent-aware `brand-accent` CSS class, not
/// `GtkToggleButton` state - the buttons are also wired to
/// `app.format-bold`/`app.format-italic` via `set_action_name`, and a
/// toggle button's own click-driven active state would fight this external,
/// cursor-position-driven one. Only touches a button's CSS class when its
/// state actually changed since the last call, so a burst of
/// same-formatting cursor moves does not repeatedly toggle the same class.
fn update_format_toolbar_state(widgets: &Widgets) {
    let formats = active_formats_at(&widgets.buffer);
    if formats == widgets.format_toolbar_state.get() {
        return;
    }
    widgets.format_toolbar_state.set(formats);
    for (button, active) in [
        (&widgets.format_bold_button, formats.bold),
        (&widgets.format_italic_button, formats.italic),
    ] {
        if active {
            button.add_css_class("brand-accent");
        } else {
            button.remove_css_class("brand-accent");
        }
    }
}

/// Defers `update_format_toolbar_state` to an idle callback - i.e. the next
/// point the main loop is otherwise free - rather than running it inline.
///
/// `cursor-position` is a GObject property notify, and GObject property
/// notifications are always synchronous: connecting this handler directly
/// to `connect_cursor_position_notify` would run it nested inside whatever
/// call moved the cursor, which very much includes `GtkTextBuffer::delete`/
/// `insert` while `apply_format_to_buffer` is still on the stack for a
/// formatting action. Mutating unrelated widgets (the toolbar buttons' CSS
/// classes, which can trigger their own style/layout invalidation) from
/// that deeply re-entrant a point risks exactly the kind of "widget touched
/// mid-relayout" bug this project's RefCell-reentrancy discipline exists to
/// prevent, just at the GTK-widget-tree level instead of the Rust-borrow
/// level. Deferring to idle guarantees the update always runs as its own
/// clean top-level main loop turn, after the triggering call has fully
/// returned and any layout that call queued has had a chance to settle.
fn schedule_format_toolbar_update(widgets: &Widgets) {
    if let Some(source) = widgets.format_toolbar_update_source.borrow_mut().take() {
        source.remove();
    }
    let widgets_for_update = widgets.clone();
    let source_slot = widgets.format_toolbar_update_source.clone();
    let source = glib::idle_add_local_once(move || {
        source_slot.borrow_mut().take();
        update_format_toolbar_state(&widgets_for_update);
    });
    *widgets.format_toolbar_update_source.borrow_mut() = Some(source);
}

fn show_welcome_error(widgets: &Widgets, message: &str) {
    widgets.welcome_status.set_label(message);
    widgets.welcome_status.add_css_class("error");
    widgets.stack.set_visible_child_name("welcome");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_token_round_trips_every_view() {
        for view in [
            ViewMode::AllNotes,
            ViewMode::Pinned,
            ViewMode::RecentlyOpened,
            ViewMode::Favourites,
            ViewMode::Archive,
            ViewMode::Trash,
            ViewMode::Notebook(PathBuf::from("Inbox")),
            ViewMode::Notebook(PathBuf::from("Work/Projects")),
        ] {
            let token = view_token(&view);
            assert_eq!(
                parse_view_token(&token),
                Some(view.clone()),
                "token {token:?} must round-trip"
            );
        }
    }

    #[test]
    fn parse_view_token_rejects_garbage_and_falls_back_to_none() {
        assert_eq!(parse_view_token(""), None);
        assert_eq!(parse_view_token("not-a-view"), None);
        // A notebook token always parses (existence is checked separately by
        // `resolve_restored_view`).
        assert_eq!(
            parse_view_token("notebook:Anything/Here"),
            Some(ViewMode::Notebook(PathBuf::from("Anything/Here")))
        );
    }

    #[test]
    fn vault_display_name_uses_the_folder_name() {
        assert_eq!(
            vault_display_name(Path::new("/home/user/My Notes")),
            "My Notes"
        );
        assert_eq!(vault_display_name(Path::new("/")), APP_NAME);
    }
}
