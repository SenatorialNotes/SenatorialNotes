# Changelog

All notable changes to SenatorialNotes will be documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and releases use semantic versioning.

## [Unreleased]

### v0.2 (in progress — not yet accepted; real-machine acceptance pending)

#### Added

- Real, user-facing notebooks: create, rename, and safely delete (refuses on
  any note or any file/symlink it does not manage anywhere in the subtree;
  never a recursive `remove_dir_all`), with nested child notebooks. `Inbox`
  is a reserved notebook and cannot be renamed or deleted.
- Moving a note between notebooks, including encrypted `.snote` notes - the
  encrypted container's authenticated header never includes the file path,
  so a move is a plain, collision-safe (`renameat2`/`RENAME_NOREPLACE`)
  filesystem rename, never a re-encryption.
- Archive, with an Archive smart view; Pinned and Recently Edited as
  first-class smart views alongside All Notes/Inbox/Trash.
- Tag add/remove in the editor, and a sidebar tag-filter section, with
  case-insensitive duplicate prevention that preserves first-seen casing.
- User-controlled sort order (Last Edited, Date Created, Title A-Z, Title
  Z-A) alongside the existing default (pinned-first, then recency, then
  title); explicit choices never rewrite note files.
- Combined notebook + tag + search filtering.
- A Note Information panel (title, notebook, tags, created/modified,
  pinned, archived, encryption status, word/character count, with the
  vault-relative path and UUID tucked into an Advanced expander), reachable
  from a header button, the app/context menus, and `Alt+Return`. For a
  locked encrypted note it shows only what a locked summary actually knows
  (never a guess at its protected title/tags/body).
- Keyboard: `Ctrl+Shift+N` new notebook, `Ctrl+1`/`Ctrl+2` focus the note
  list/editor, `Ctrl+]`/`Ctrl+[` next/previous note, `Ctrl+Shift+P` pin,
  `Ctrl+Shift+A` archive, `Alt+Return` note information.

#### Changed

- New Note creates in the currently selected notebook, falling back to
  `Inbox` from any smart view.
- A locked encrypted note's protected fields (pinned, archived) are never
  guessed or leaked while locked: they read as `false` until the note is
  unlocked, which keeps it out of Pinned/Archive/Recently Edited (all built
  from protected fields) while it stays fully visible in All Notes and its
  real notebook (notebook membership is the file's location, not inside
  ciphertext). See `SECURITY.md`.

### Fixed

- The password dialog no longer aborts the process after a valid password is entered. Confirm/Cancel/Escape now take the pending completion out of its `RefCell` and drop the borrow *before* calling `window.close()`, which synchronously re-emits `close-request` into the same cell. Audited across Encrypt Note, Unlock, Change Password, and Remove Encryption; verified with real GTK interaction on Arch/Hyprland under `G_DEBUG=fatal-warnings`.
- The Markdown formatting toolbar now toggles instead of stacking markers: Bold/Italic/Strikethrough/Highlight/Inline code remove their pair when the selection (or the span the cursor is in) already carries it, bold and italic are told apart by `*` run parity so toggling one never rewrites the other, and Style → Heading normalises any existing heading prefix and toggles back to a paragraph on a second press rather than prepending another `# `.
- Search now matches plaintext note body text and tags, not just the title, for All Notes and Inbox. It runs entirely against the in-memory summaries with no network access and no persistent plaintext index; locked encrypted notes never populate a searchable body or tag, so their contents cannot match.
- Resizing the window small no longer triggers a fatal `Adwaita-WARNING` about a `GtkStack` exceeding the `AdwApplicationWindow` size, in either dimension. The welcome, locked, empty, trash-detail, empty-list and formatting-toolbar pages scroll rather than imposing their natural size as a hard minimum; the three `GtkStack`s are no longer size-homogeneous and the top-level one no longer cross-fades (a running transition measured both children, leaking the welcome page's width into the workspace); the `<=760px` breakpoint now also collapses the library sidebar (Adwaita breakpoints are mutually exclusive, so the narrow one must carry the whole compact layout); the header title and save-status labels ellipsize. The window then carries a real minimum size (410x320) matching the collapsed header/toolbar, so a resize can never reach an invalid allocation. Verified by sweeping widths and heights, including the exact breakpoint boundaries, under `G_DEBUG=fatal-warnings`.
- Rapid note switching is dispatched from a short normal-priority timeout rather than a low-priority idle callback (which a flood of pointer events and frame-clock ticks could starve for a visible pause). Exactly one dispatch is ever outstanding, it consumes only the newest request, a burst that ends on the already-open note does no reload, redundant list-row selection is skipped, and the filesystem-watcher poll stays idle while a selection dispatch is in flight.
- Change Password on an encrypted note now re-keys the container, verifies that the old password fails and the new one succeeds before reporting success, and hands the UI a freshly verified session/stamp so a later autosave can no longer re-encrypt the note under the old key.
- Password prompts are windows SenatorialNotes controls instead of self-closing `adw::MessageDialog`s, so a rejected password (empty, too short, mismatched, or wrong current password) shows a specific reason instead of silently doing nothing. The minimum passphrase length (12 characters) is stated up front.
- Remove Encryption now shows an explicit plaintext-on-disk warning and a deliberate confirmation before asking for the current password.
- The note list uses one shared context-menu popover owned by the list instead of one permanently parented to every row, eliminating `Finalizing GtkListBoxRow … still has children` warnings; row gestures hold their row weakly.
- Programmatic note loading and formatting no longer fight GtkSourceView's undo system: full-buffer replacement is done with undo disabled, and formatting edits only the changed span inside a single user action, removing the "Cannot begin/end irreversible action while in user action" warnings.
- Rapid note switching no longer stalls: clean plaintext notes are served from a stamp-validated in-memory cache instead of being re-read and re-parsed, a burst of row clicks is coalesced to a single load of the final target, and the filesystem watcher compares a cheap stat-only baseline so SenatorialNotes' own atomic writes no longer trigger a vault-wide rescan.
- The search placeholder always matches the active view (All Notes → "Search notes", Inbox → "Search Inbox", Trash → "Search Trash").

### Added

- A GUI-independent selection coordinator with nested event suppression and stable UUID row targets.
- Stress/regression coverage for 100 sequential note creations, rapid selection, create/rename/pin/trash/restore flows, smart-view switches, and targeted context actions.
- Regression coverage for formatting toggle-on/toggle-off, overlapping bold/italic, heading normalisation, and for local search matching body text while locked encrypted content cannot.
- Adaptive Library and Notes/Editor navigation using libadwaita breakpoints.
- A functional Inbox smart view and explicit empty states for notes, Inbox, and Trash.
- Trash, restore-to-original-notebook, permanent deletion, and Empty Trash with confirmation.
- Appearance and encrypted-note locking preferences.
- Markdown formatting toolbar and keyboard actions.
- Versioned `.snote` containers using Argon2id and XChaCha20-Poly1305.
- Encrypt, unlock, Lock Now, change-password, and remove-encryption flows.
- Regression and security tests for title commits, Trash, theme persistence, ciphertext secrecy, wrong passwords, tampering, recovery files, and locked search data.

### Changed

- GTK callbacks now finish every application-state borrow before changing widgets or invoking another transition.
- Right-click menus carry their note UUID directly and no longer force a selection change before opening.
- New Note inserts one row and selects it once after signal suppression; delete, restore, rename, pin, previews, and saved status use incremental updates.
- The formatting toolbar now adapts as Style, Bold, Italic, and More instead of clipping a full action row.
- Editor width is presented as Comfortable, Wide, or Full width, with Wide as the less-centered default.
- Redesigned the three-pane interface with calmer spacing, note cards, clearer hierarchy, editor margins, and a restrained accent system.
- Body autosave now updates the current model/file without rebuilding the note list or editor.
- Titles remain in-memory drafts while typing and commit separately after 1.5 seconds or an explicit commit event.
- GtkSourceView now receives a matching light/dark style scheme.

### Security

- Encrypted notes use neutral filenames and keep title, body, tags, and private metadata inside authenticated ciphertext.
- Encrypted notes never write plaintext recovery files or persistent plaintext search data.

### Planned

- Complete native build and visual validation on Arch Linux.
- Remaining note-management and editing work described in `SPECIFICATION.md`.

## [0.1.0] - 2026-08-25

### Added

- Initial Rust project using GTK4, libadwaita, and GtkSourceView 5.
- Local vault creation/opening and an Inbox notebook.
- UUID-backed Markdown notes with YAML front matter.
- Preservation of unknown metadata fields.
- Atomic save, external-change stamps, and local recovery copies.
- Native first-run screen, note list, title field, editor, and debounced autosave.
- Native filesystem watching with authoritative Markdown rescans.
- Local configuration for recent vaults.
- Storage/path-safety tests and forbidden HTTP-client dependency check.
- Desktop, AppStream, icon, Arch, Flatpak, security, and contribution metadata.

[Unreleased]: https://github.com/SenatorialNotes/SenatorialNotes/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/SenatorialNotes/SenatorialNotes/releases/tag/v0.1.0
