use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use adw::{Application, ApplicationWindow};
use gtk::gdk;
use gtk::gio;
use gtk::glib;
use senatorial_notes::config::{Accent, AppConfig, NoteListDensity, Theme};
use senatorial_notes::constants::{APP_ID, APP_NAME, MIN_PASSWORD_LENGTH, PRIVACY_STATEMENT};
use senatorial_notes::formatting::{FormatAction, apply_markdown_format};
use senatorial_notes::search::summary_matches;
use senatorial_notes::ui_state::{
    RowTarget, SelectionCoordinator, SelectionIntent, UiFlow, ViewMode,
};
use senatorial_notes::watcher::VaultWatcher;
use senatorial_notes::{EncryptedSession, FileStamp, Note, NoteSummary, TrashEntry, Vault};
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
    last_sensitive_activity: Option<Instant>,
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
    vault_label: gtk::Label,
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
    trash_button: gtk::Button,
    document_stack: gtk::Stack,
    title: gtk::Entry,
    buffer: sourceview5::Buffer,
    editor: sourceview5::View,
    formatting_bar: gtk::ScrolledWindow,
    save_status: gtk::Label,
    locked_copy: gtk::Label,
    trash_detail_title: gtk::Label,
    empty_trash_button: gtk::Button,
    appearance_provider: gtk::CssProvider,
}

struct Controls {
    create_vault: gtk::Button,
    open_vault: gtk::Button,
    new_note: gtk::Button,
    all_notes: gtk::Button,
    inbox: gtk::Button,
    trash: gtk::Button,
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

    connect_folder_button(&controls.create_vault, true, state.clone(), widgets.clone());
    connect_folder_button(&controls.open_vault, false, state.clone(), widgets.clone());

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
            switch_view(ViewMode::Notes, &state, &widgets);
        });
    }

    {
        let state = state.clone();
        let widgets = widgets.clone();
        let pending = pending.clone();
        controls.inbox.connect_clicked(move |_| {
            cancel_all_timers(&pending);
            switch_view(ViewMode::Inbox, &state, &widgets);
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
                if state
                    .active
                    .as_ref()
                    .is_some_and(ActiveDocument::is_encrypted)
                {
                    state.last_sensitive_activity = Some(Instant::now());
                }
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
            if persist_active(&state, &widgets, true) {
                clear_sensitive_documents(&state);
                // Detach the shared context menu before its parent is disposed.
                widgets.row_menu.unparent();
                glib::Propagation::Proceed
            } else {
                glib::Propagation::Stop
            }
        });
    }

    connect_locking_events(&state, &widgets);
    install_watcher_poll(&state, &widgets);

    let last_vault = { state.borrow().config.last_vault.clone() };
    if let Some(path) = last_vault.filter(|path| path.is_dir()) {
        open_vault(&path, false, &state, &widgets);
    }

    widgets.window.present();
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
    vault_label.set_max_width_chars(24);
    header.set_title_widget(Some(&vault_label));
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
    let library_heading = gtk::Label::new(Some("LIBRARY"));
    library_heading.set_xalign(0.0);
    library_heading.add_css_class("sidebar-section-title");
    sidebar.append(&library_heading);
    let all_notes = sidebar_button("All Notes", "view-list-symbolic");
    all_notes.add_css_class("sidebar-selected");
    all_notes.set_tooltip_text(Some("Show every note in this vault"));
    sidebar.append(&all_notes);
    let inbox = sidebar_button("Inbox", "mail-inbox-symbolic");
    inbox.set_tooltip_text(Some("Show notes in the Inbox notebook"));
    sidebar.append(&inbox);
    let trash = sidebar_button("Trash", "user-trash-symbolic");
    trash.set_tooltip_text(Some("Show deleted notes"));
    sidebar.append(&trash);
    let privacy_badge = gtk::Label::new(Some("Offline by design"));
    privacy_badge.set_valign(gtk::Align::End);
    privacy_badge.set_vexpand(true);
    privacy_badge.set_xalign(0.0);
    privacy_badge.add_css_class("caption");
    privacy_badge.add_css_class("dim-label");
    sidebar.append(&privacy_badge);
    library_split.set_sidebar(Some(&sidebar));

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
    notes_header.append(&notes_heading);
    notes_header.append(&empty_trash_button);
    notes_box.append(&notes_header);
    let search = gtk::SearchEntry::builder()
        .placeholder_text("Search notes")
        .margin_start(12)
        .margin_end(12)
        .margin_bottom(10)
        .build();
    search.update_property(&[gtk::accessible::Property::Label("Search notes")]);
    notes_box.append(&search);
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
    title_row.append(&title);
    title_row.append(&save_status);
    editor_box.append(&title_row);

    let formatting_bar = build_formatting_bar();
    editor_box.append(&formatting_bar);
    let buffer = sourceview5::Buffer::new(None::<&gtk::TextTagTable>);
    if let Some(language) = sourceview5::LanguageManager::default().language("markdown") {
        buffer.set_language(Some(&language));
        buffer.set_highlight_syntax(true);
    }
    buffer.set_highlight_matching_brackets(true);
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
            trash_button: trash.clone(),
            document_stack,
            title,
            buffer,
            editor,
            formatting_bar,
            save_status,
            locked_copy,
            trash_detail_title,
            empty_trash_button,
            appearance_provider,
        },
        Controls {
            create_vault,
            open_vault,
            new_note,
            all_notes,
            inbox,
            trash,
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
    menu.append(Some("Open Vault…"), Some("app.open-vault"));
    menu.append(Some("Preferences"), Some("app.preferences"));
    let security = gio::Menu::new();
    security.append(Some("Encrypt Note…"), Some("app.encrypt-note"));
    security.append(Some("Lock Now"), Some("app.lock-now"));
    security.append(Some("Change Password…"), Some("app.change-password"));
    security.append(Some("Remove Encryption…"), Some("app.remove-encryption"));
    menu.append_section(Some("Encrypted Note"), &security);
    let note = gio::Menu::new();
    note.append(Some("Move to Trash"), Some("app.move-to-trash"));
    menu.append_section(Some("Note"), &note);
    menu.append(Some("About SenatorialNotes"), Some("app.about"));
    menu.append(Some("Quit"), Some("app.quit"));
    menu
}

fn build_formatting_bar() -> gtk::ScrolledWindow {
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

    for (label, tooltip, action) in [
        ("B", "Bold (Ctrl+B)", "app.format-bold"),
        ("I", "Italic (Ctrl+I)", "app.format-italic"),
    ] {
        let button = gtk::Button::with_label(label);
        button.add_css_class("flat");
        button.add_css_class("format-button");
        button.set_tooltip_text(Some(tooltip));
        button.set_action_name(Some(action));
        button.update_property(&[gtk::accessible::Property::Label(tooltip)]);
        bar.append(&button);
    }

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
    bar_scroll
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
) {
    button.connect_clicked(move |_| {
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
        let parent = widgets.window.clone();
        dialog.select_folder(
            Some(&parent),
            None::<&gio::Cancellable>,
            move |result| match result {
                Ok(folder) => match folder.path() {
                    Some(path) => open_vault(&path, create, &state, &widgets),
                    None => {
                        show_welcome_error(&widgets, "The selected folder is not a local path.")
                    }
                },
                Err(error) if !error.matches(gio::IOErrorEnum::Cancelled) => {
                    show_welcome_error(&widgets, &format!("Folder selection failed: {error}"));
                }
                Err(_) => {}
            },
        );
    });
}

fn open_vault(path: &Path, create: bool, state: &Rc<RefCell<AppState>>, widgets: &Widgets) {
    let result = if create {
        Vault::create(path)
    } else {
        Vault::open(path)
    };
    match result {
        Ok(vault) => {
            clear_sensitive_documents(state);
            let watcher = VaultWatcher::new(vault.root());
            let watcher_error = watcher.as_ref().err().map(ToString::to_string);
            let watcher = watcher.ok();
            let config_save_error = {
                let mut state = state.borrow_mut();
                state.config.remember_vault(vault.root());
                state.watcher = watcher;
                state.vault = Some(vault);
                state.notes.clear();
                state.trash.clear();
                state.body_dirty = false;
                state.title_dirty = false;
                state.flow.switch_view(ViewMode::Notes);
                state.config.save().err().map(|error| error.to_string())
            };
            if let Some(error) = watcher_error {
                widgets
                    .save_status
                    .set_label(&format!("Vault opened without live updates: {error}"));
            }
            if let Some(error) = config_save_error {
                widgets
                    .save_status
                    .set_label(&format!("Vault opened; settings were not saved: {error}"));
            }
            widgets.vault_label.set_label(
                path.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or(APP_NAME),
            );
            widgets.stack.set_visible_child_name("workspace");
            if widgets.library_split.is_collapsed() {
                widgets.library_split.set_show_sidebar(false);
            }
            widgets.content_split.set_show_content(false);
            apply_view_chrome(ViewMode::Notes, widgets);
            if !refresh_current_view(state, widgets) {
                return;
            }
            let is_empty = { state.borrow().notes.is_empty() };
            if is_empty {
                create_new_note(
                    state,
                    widgets,
                    &Rc::new(RefCell::new(PendingSaves::default())),
                );
            } else {
                select_first_row(state, widgets);
            }
            refresh_watch_baseline(state);
        }
        Err(error) => show_welcome_error(widgets, &format!("Could not open the vault: {error}")),
    }
}

fn switch_view(mode: ViewMode, state: &Rc<RefCell<AppState>>, widgets: &Widgets) {
    let already_selected = { state.borrow().flow.view() == mode };
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
        state.flow.switch_view(mode);
    }
    widgets.search.set_text("");
    apply_view_chrome(mode, widgets);
    widgets.document_stack.set_visible_child_name("empty");
    if widgets.library_split.is_collapsed() {
        widgets.library_split.set_show_sidebar(false);
    }
    widgets.content_split.set_show_content(false);
    if refresh_current_view(state, widgets) {
        select_first_row(state, widgets);
    }
}

fn create_new_note(
    state: &Rc<RefCell<AppState>>,
    widgets: &Widgets,
    pending: &Rc<RefCell<PendingSaves>>,
) {
    let current_view = { state.borrow().flow.view() };
    if !matches!(current_view, ViewMode::Notes | ViewMode::Inbox) {
        cancel_all_timers(pending);
        switch_view(ViewMode::Notes, state, widgets);
        let switched_to_notes = { state.borrow().flow.view() == ViewMode::Notes };
        if !switched_to_notes {
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
    match vault.create_note("Untitled", Path::new("Inbox")) {
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

fn apply_view_chrome(mode: ViewMode, widgets: &Widgets) {
    widgets.notes_heading.set_label(mode.heading());
    widgets
        .search
        .set_placeholder_text(Some(mode.search_placeholder()));
    widgets
        .empty_trash_button
        .set_visible(mode == ViewMode::Trash);
    update_library_selection(mode, widgets);
}

fn update_library_selection(mode: ViewMode, widgets: &Widgets) {
    for button in [
        &widgets.all_notes_button,
        &widgets.inbox_button,
        &widgets.trash_button,
    ] {
        button.remove_css_class("sidebar-selected");
    }
    match mode {
        ViewMode::Notes => widgets.all_notes_button.add_css_class("sidebar-selected"),
        ViewMode::Inbox => widgets.inbox_button.add_css_class("sidebar-selected"),
        ViewMode::Trash => widgets.trash_button.add_css_class("sidebar-selected"),
    }
}

fn select_first_row(state: &Rc<RefCell<AppState>>, widgets: &Widgets) {
    let preferred = {
        let state = state.borrow();
        match state.flow.view() {
            ViewMode::Notes | ViewMode::Inbox => state.flow.selected_note().map(RowTarget::Note),
            ViewMode::Trash => state.flow.selected_trash().map(RowTarget::Trash),
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
        RowTarget::Note(id) => load_note_by_id(id, state, widgets),
        RowTarget::Trash(id) => show_trash_by_id(id, state, widgets),
    }
}

fn load_note_by_id(id: Uuid, state: &Rc<RefCell<AppState>>, widgets: &Widgets) {
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
    let source =
        glib::timeout_add_local_once(Duration::from_millis(SELECTION_DISPATCH_MS), move || {
            widgets_for_dispatch.select_source.replace(None);
            let Some(target) = widgets_for_dispatch.pending_select.take() else {
                return;
            };
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
    let encrypted = document.is_encrypted();
    {
        let mut state = state.borrow_mut();
        if let Some(mut previous) = state.active.take() {
            previous.clear_sensitive();
        }
        state.title_draft = title.clone();
        state.body_dirty = false;
        state.title_dirty = false;
        state.last_sensitive_activity = encrypted.then(Instant::now);
        state.active = Some(document);
    }
    let _editor_guard = widgets.editor_events.suppress();
    widgets.title.set_sensitive(true);
    widgets.editor.set_sensitive(true);
    widgets.formatting_bar.set_sensitive(true);
    widgets.title.set_text(&title);
    set_buffer_text_silently(&widgets.buffer, &body);
    widgets.document_stack.set_visible_child_name("editor");
    widgets.save_status.set_label(if encrypted {
        "Unlocked · encrypted at rest"
    } else {
        "Saved"
    });
}

fn show_locked_placeholder(widgets: &Widgets) {
    widgets
        .locked_copy
        .set_label("This note is encrypted on disk. Enter its password to decrypt it in memory.");
    widgets.title.set_sensitive(false);
    widgets.editor.set_sensitive(false);
    widgets.formatting_bar.set_sensitive(false);
    widgets.document_stack.set_visible_child_name("locked");
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
        if state.active.is_none() {
            false
        } else {
            state.body_dirty = true;
            if state
                .active
                .as_ref()
                .is_some_and(ActiveDocument::is_encrypted)
            {
                state.last_sensitive_activity = Some(Instant::now());
            }
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
    let source = glib::timeout_add_local_once(Duration::from_millis(delay), move || {
        pending_for_save.borrow_mut().body.take();
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
    let delay = state
        .borrow()
        .config
        .title_commit_delay_ms
        .clamp(1_000, 5_000);
    let state_for_save = state.clone();
    let widgets_for_save = widgets.clone();
    let pending_for_save = pending.clone();
    let source = glib::timeout_add_local_once(Duration::from_millis(delay), move || {
        pending_for_save.borrow_mut().title.take();
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
                active.is_encrypted(),
                note.metadata.pinned,
                note.metadata.updated_at,
                note.metadata.tags.clone(),
            )
        })
    };
    let Some((id, title, body, path, encrypted, pinned, updated_at, tags)) = active_snapshot else {
        return;
    };
    let preview_limit = { state.borrow().config.appearance.note_preview_length };
    let preview = if encrypted {
        "Encrypted — unlock to view".into()
    } else {
        truncate_preview(&body, preview_limit)
    };
    if let Some(summary) = state
        .borrow_mut()
        .notes
        .iter_mut()
        .find(|note| note.id == id)
    {
        summary.relative_path = path;
        summary.pinned = pinned;
        summary.updated_at = updated_at;
        // Keep locked encrypted summaries private even during an unlocked
        // session; this also prevents plaintext from entering persistent or
        // list-level search data.
        if !encrypted {
            summary.title = title.clone();
            summary.preview = preview.clone();
            summary.body = body.clone();
            summary.tags = tags.clone();
        }
    }
    let row_widgets = { widgets.row_widgets.borrow().get(&id).cloned() };
    if let Some(row_widgets) = row_widgets {
        row_widgets.pin.set_visible(pinned);
        if !encrypted {
            row_widgets.title.set_label(&title);
            row_widgets.preview.set_label(&preview);
        }
    }
}

fn refresh_current_view(state: &Rc<RefCell<AppState>>, widgets: &Widgets) -> bool {
    let (vault, mode) = {
        let state = state.borrow();
        (state.vault.clone(), state.flow.view())
    };
    let Some(vault) = vault else {
        return false;
    };
    let result = match mode {
        ViewMode::Notes | ViewMode::Inbox => vault.scan_notes().map(|notes| (Some(notes), None)),
        ViewMode::Trash => vault.scan_trash().map(|trash| (None, Some(trash))),
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
            state.flow.view(),
            state.flow.selected_note(),
            state.flow.selected_trash(),
            state.config.appearance.note_list_density,
            match state.flow.view() {
                ViewMode::Inbox => filtered_inbox(&state, &query),
                ViewMode::Notes | ViewMode::Trash => filtered_notes(&state, &query),
            },
            filtered_trash(&state, &query),
        )
    };
    let targets: Vec<RowTarget> = match mode {
        ViewMode::Notes | ViewMode::Inbox => notes
            .iter()
            .map(|summary| RowTarget::Note(summary.id))
            .collect(),
        ViewMode::Trash => trash
            .iter()
            .map(|entry| RowTarget::Trash(entry.id))
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
        ViewMode::Notes | ViewMode::Inbox => {
            for summary in notes {
                let (row, row_widgets) = note_row(
                    &summary.title,
                    &summary.preview,
                    summary.encrypted,
                    summary.pinned,
                    density,
                    note_context_menu(summary.id, summary.pinned, summary.encrypted),
                    &widgets.row_menu,
                );
                widgets
                    .row_widgets
                    .borrow_mut()
                    .insert(summary.id, row_widgets);
                widgets.note_list.append(&row);
            }
        }
        ViewMode::Trash => {
            for entry in trash {
                let subtitle = if entry.encrypted {
                    "Encrypted note".into()
                } else {
                    format!("From {}", entry.original_relative_path.display())
                };
                let (row, row_widgets) = note_row(
                    &entry.title,
                    &subtitle,
                    entry.encrypted,
                    false,
                    density,
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
    }
    let selected = match mode {
        ViewMode::Notes | ViewMode::Inbox => {
            selected_note.and_then(|id| widgets.selection.index_of(RowTarget::Note(id)))
        }
        ViewMode::Trash => {
            selected_trash.and_then(|id| widgets.selection.index_of(RowTarget::Trash(id)))
        }
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
                ViewMode::Notes => ("No notes yet", "Create a new note to start writing."),
                ViewMode::Inbox => ("Inbox is empty", "New notes appear in the Inbox."),
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
        &summary.title,
        &summary.preview,
        summary.encrypted,
        summary.pinned,
        density,
        note_context_menu(summary.id, summary.pinned, summary.encrypted),
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
        let visible = match state.flow.view() {
            ViewMode::Inbox => filtered_inbox(&state, &query),
            ViewMode::Notes | ViewMode::Trash => filtered_notes(&state, &query),
        };
        visible
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
        RowTarget::Note(id) => load_note_by_id(id, state, widgets),
        RowTarget::Trash(id) => show_trash_by_id(id, state, widgets),
    }
}

fn note_row(
    title_text: &str,
    preview_text: &str,
    encrypted: bool,
    pinned: bool,
    density: NoteListDensity,
    menu: gio::Menu,
    row_menu: &gtk::PopoverMenu,
) -> (gtk::ListBoxRow, RowWidgets) {
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
    let pin = gtk::Image::from_icon_name("emblem-favorite-symbolic");
    pin.set_pixel_size(14);
    pin.set_visible(pinned);
    pin.set_tooltip_text(Some("Pinned note"));
    title_row.append(&pin);
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
        },
    )
}

fn note_context_menu(id: Uuid, pinned: bool, encrypted: bool) -> gio::Menu {
    let menu = gio::Menu::new();
    append_targeted_menu_item(&menu, "Rename", "app.context-rename", id);
    append_targeted_menu_item(
        &menu,
        if pinned { "Unpin" } else { "Pin" },
        "app.context-toggle-pin",
        id,
    );
    if !encrypted {
        append_targeted_menu_item(&menu, "Encrypt Note…", "app.context-encrypt", id);
    }
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
    use gtk::glib::variant::ToVariant;

    let item = gio::MenuItem::new(Some(label), None);
    item.set_action_and_target_value(Some(action), Some(&id.to_string().to_variant()));
    menu.append_item(&item);
}

fn filtered_notes(state: &AppState, query: &str) -> Vec<NoteSummary> {
    state
        .notes
        .iter()
        .filter(|summary| summary_matches(summary, query))
        .cloned()
        .collect()
}

fn filtered_inbox(state: &AppState, query: &str) -> Vec<NoteSummary> {
    filtered_notes(state, query)
        .into_iter()
        .filter(|summary| {
            summary
                .relative_path
                .components()
                .next()
                .is_some_and(|component| component.as_os_str() == "Inbox")
        })
        .collect()
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
    let view = { state.borrow().flow.view() };
    if !matches!(view, ViewMode::Notes | ViewMode::Inbox) {
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

fn toggle_pin_by_id(id: Uuid, state: &Rc<RefCell<AppState>>, widgets: &Widgets) {
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
        widgets
            .save_status
            .set_label("Unlock this encrypted note before changing its pinned state.");
        return;
    }

    let save_succeeded = if active_is_target {
        {
            let mut state = state.borrow_mut();
            if let Some(active) = state.active.as_mut() {
                let note = active.note_mut();
                note.metadata.pinned = !note.metadata.pinned;
                state.body_dirty = true;
            }
        }
        persist_active(state, widgets, false)
    } else {
        match vault.load_note(&summary.relative_path) {
            Ok((mut note, stamp)) => {
                note.metadata.pinned = !note.metadata.pinned;
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
                        widgets
                            .save_status
                            .set_label(&format!("Could not change pinned state: {error}"));
                        false
                    }
                }
            }
            Err(error) => {
                widgets
                    .save_status
                    .set_label(&format!("Could not open note for pinning: {error}"));
                false
            }
        }
    };

    if !save_succeeded {
        return;
    }
    {
        let mut state = state.borrow_mut();
        state.notes.sort_by(|left, right| {
            right
                .pinned
                .cmp(&left.pinned)
                .then_with(|| right.updated_at.cmp(&left.updated_at))
                .then_with(|| left.title.to_lowercase().cmp(&right.title.to_lowercase()))
        });
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
        replace_note_row(&updated, state, widgets);
        widgets
            .save_status
            .set_label(if updated.pinned { "Pinned" } else { "Unpinned" });
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
}

fn install_list_delete_key(state: &Rc<RefCell<AppState>>, widgets: &Widgets) {
    let controller = gtk::EventControllerKey::new();
    let state = state.clone();
    let widgets_for_key = widgets.clone();
    controller.connect_key_pressed(move |_, key, _, _| {
        let view = { state.borrow().flow.view() };
        if key == gdk::Key::Delete && matches!(view, ViewMode::Notes | ViewMode::Inbox) {
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
        match changed {
            Some(Ok(true)) if editor_is_clean => {
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

/// Cheap stat-only snapshot of the notes and trash trees: (path, mtime, length)
/// for every `.md`/`.snote`/trash file, sorted. No file contents are read.
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
    walk(&vault.notes_dir(), &mut out);
    walk(&vault.trash_dir(), &mut out);
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
            state.flow.view(),
            widgets.search.text().to_string(),
        )
    };
    let Some(vault) = vault else {
        return;
    };
    match mode {
        ViewMode::Notes | ViewMode::Inbox => {
            let Ok(mut scanned) = vault.scan_notes() else {
                widgets
                    .save_status
                    .set_label("Notes changed on disk, but the updated list could not be read.");
                return;
            };
            let pin_changes = {
                let mut state = state.borrow_mut();
                let previous = std::mem::take(&mut state.notes);
                let mut merged = Vec::with_capacity(scanned.len());
                let mut pin_changes = Vec::new();
                for old in previous {
                    if let Some(index) = scanned.iter().position(|new| new.id == old.id) {
                        let new = scanned.remove(index);
                        if old.pinned != new.pinned {
                            pin_changes.push(new.id);
                        }
                        if preserve_active_id == Some(old.id) {
                            merged.push(old);
                        } else {
                            merged.push(new);
                        }
                    }
                }
                merged.extend(scanned);
                state.notes = merged;
                pin_changes
            };
            let (targets, summaries) = {
                let state = state.borrow();
                let summaries = match mode {
                    ViewMode::Inbox => filtered_inbox(&state, &query),
                    ViewMode::Notes | ViewMode::Trash => filtered_notes(&state, &query),
                };
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
                if pin_changes.contains(&summary.id) {
                    replace_note_row(&summary, state, widgets);
                    continue;
                }
                let row_widgets = { widgets.row_widgets.borrow().get(&summary.id).cloned() };
                if let Some(row_widgets) = row_widgets {
                    row_widgets.title.set_label(&summary.title);
                    row_widgets.preview.set_label(&summary.preview);
                    row_widgets.pin.set_visible(summary.pinned);
                }
            }
        }
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

fn connect_locking_events(state: &Rc<RefCell<AppState>>, widgets: &Widgets) {
    {
        let state = state.clone();
        let widgets = widgets.clone();
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
                }
            });
    }
    {
        let state = state.clone();
        let widgets = widgets.clone();
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
            }
            glib::ControlFlow::Continue
        });
    }
}

fn lock_all_encrypted(state: &Rc<RefCell<AppState>>, widgets: &Widgets) {
    if !persist_active(state, widgets, true) {
        return;
    }
    {
        let mut state = state.borrow_mut();
        for (_, mut document) in state.unlocked_cache.drain() {
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
    present_change_password_dialog(&widgets.window, move |passwords| {
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

fn present_change_password_dialog<F>(parent: &ApplicationWindow, callback: F)
where
    F: FnOnce(Option<(Zeroizing<String>, Zeroizing<String>)>) + 'static,
{
    let window = adw::Window::builder()
        .transient_for(parent)
        .modal(true)
        .title("Change Password")
        .default_width(430)
        .resizable(false)
        .build();
    let header = adw::HeaderBar::new();
    header.set_show_start_title_buttons(false);
    header.set_show_end_title_buttons(false);
    let cancel = gtk::Button::with_label("Cancel");
    let confirm = gtk::Button::with_label("Change Password");
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

fn finish_on_close<F>(window: &adw::Window, callback: &Rc<RefCell<Option<F>>>)
where
    F: FnOnce(Option<Zeroizing<String>>) + 'static,
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

    content.append(&preference_heading("Encrypted note locking"));
    add_lock_switch(
        &content,
        "When switching away from the note",
        current.encrypted_note_locking.on_note_switch,
        state,
        widgets,
        |config, value| config.encrypted_note_locking.on_note_switch = value,
    );
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
            if !prepare_to_leave_active(&state, &widgets, &pending) {
                return;
            }
            let dialog = gtk::FileDialog::builder()
                .title("Open a SenatorialNotes Vault")
                .modal(true)
                .build();
            let state = state.clone();
            let widgets = widgets.clone();
            let parent = widgets.window.clone();
            dialog.select_folder(Some(&parent), None::<&gio::Cancellable>, move |result| {
                match result {
                    Ok(folder) => match folder.path() {
                        Some(path) => open_vault(&path, false, &state, &widgets),
                        None => {
                            show_welcome_error(&widgets, "The selected folder is not a local path.")
                        }
                    },
                    Err(error) if !error.matches(gio::IOErrorEnum::Cancelled) => {
                        show_welcome_error(&widgets, &format!("Folder selection failed: {error}"));
                    }
                    Err(_) => {}
                }
            });
        });
    }
    application.add_action(&open);
    application.set_accels_for_action("app.open-vault", &["<Primary>o"]);

    let preferences = gio::SimpleAction::new("preferences", None);
    {
        let application = application.clone();
        let state = state.clone();
        let widgets = widgets.clone();
        preferences.connect_activate(move |_, _| show_preferences(&application, &state, &widgets));
    }
    application.add_action(&preferences);
    application.set_accels_for_action("app.preferences", &["<Primary>comma"]);

    let focus_search = gio::SimpleAction::new("focus-search", None);
    {
        let search = widgets.search.clone();
        focus_search.connect_activate(move |_, _| {
            search.grab_focus();
        });
    }
    application.add_action(&focus_search);
    application.set_accels_for_action("app.focus-search", &["<Primary><Shift>f"]);

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

    let lock = gio::SimpleAction::new("lock-now", None);
    {
        let state = state.clone();
        let widgets = widgets.clone();
        let pending = pending.clone();
        lock.connect_activate(move |_, _| {
            cancel_all_timers(&pending);
            lock_all_encrypted(&state, &widgets);
        });
    }
    application.add_action(&lock);

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
                toggle_pin_by_id(id, &state, &widgets);
            }
        });
    }
    application.add_action(&context_toggle_pin);

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

fn show_welcome_error(widgets: &Widgets, message: &str) {
    widgets.welcome_status.set_label(message);
    widgets.welcome_status.add_css_class("error");
    widgets.stack.set_visible_child_name("welcome");
}
