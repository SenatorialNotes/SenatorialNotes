# Changelog

All notable changes to SenatorialNotes will be documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and releases use semantic versioning.

## [Unreleased]

Nothing yet.

## [0.2.0-alpha] - 2026-09-01

This release focuses on note organisation and the editor. It follows a real-machine
acceptance pass on Arch Linux with Hyprland. It remains an alpha: the storage format,
the encrypted-note container, and parts of the interface may still change before 1.0.

The `v0.1.0-alpha` tag published only the Phase 1 storage foundation. Work that had
been staged but unreleased at that point — Trash, per-note encryption, the formatting
toolbar, preferences, and the redesigned interface — reaches a public release here for
the first time, alongside the new organisation and editor work.

### Added

- **Trash** with restore to the original notebook, confirmed permanent deletion, and a
  confirmed Empty Trash.
- **Per-note encryption.** Individually encrypted `.snote` containers using Argon2id and
  XChaCha20-Poly1305 with neutral filenames, plus Encrypt Note, Unlock, Lock Now,
  change-password, and remove-encryption flows. Derived keys live only in session
  memory and are discarded on exit. Encrypted-note locking is configurable (on note
  switch, on focus loss or minimise, or on a timer).
- **A Markdown formatting toolbar** and keyboard actions for headings, emphasis,
  highlight, inline code, quotes, lists, checklists, links, and dividers, while the file
  on disk stays Markdown.
- **Preferences** for system/light/dark appearance, editor font and size, spacing,
  Comfortable/Wide/Full editor width, line numbers, note density, and preview length.
- Matching light and dark GtkSourceView style schemes, so the editor follows the rest
  of the application.
- **Proper notebooks.** Create, rename, and delete real user-facing notebooks, with
  nested child notebooks. Deletion is deliberately conservative: it refuses whenever the
  target subtree still contains a note, or any file or symlink SenatorialNotes does not
  manage, and never performs a recursive directory removal.
- **Note movement between notebooks**, including for encrypted `.snote` notes. Because
  the encrypted container's authenticated header never includes the file path, a move is
  an ordinary collision-safe filesystem rename (`renameat2` / `RENAME_NOREPLACE`) and
  never re-encrypts or rewrites the note.
- **"Unfiled"** as the user-facing name for the built-in default notebook. Its on-disk
  directory stays named `Inbox` so existing vaults keep working unchanged; this is a
  display rename, not a data migration.
- **Tags and tag filtering.** Add and remove tags in the editor, and filter the note
  list from a sidebar tag section. Duplicate tags are prevented case-insensitively while
  the first-seen casing is preserved.
- **Archive**, with a dedicated Archive smart view.
- **Pinned** and **Recently Edited** as first-class smart views, alongside All Notes,
  Unfiled, and Trash.
- **User-controlled sort order** — Last Edited, Date Created, Title A–Z, Title Z–A —
  alongside the default order (pinned first, then most recently edited, then title).
  Choosing a sort order never rewrites note files. Date Created now sorts by each note's
  real creation timestamp.
- **Composed filtering**: notebook, tag, and text search combine.
- **Note Information** panel showing title, notebook, tags, created and modified times,
  pinned and archived state, encryption status, and word and character counts, with the
  vault-relative path and UUID under an Advanced expander. Reachable from a header
  button, the application and context menus, and `Alt+Return`. For a locked encrypted
  note it shows only what a locked summary genuinely knows and never guesses at
  protected fields.
- **Keyboard and navigation improvements**: `Ctrl+Shift+N` new notebook, `Ctrl+1` /
  `Ctrl+2` to focus the note list or editor, `Ctrl+]` / `Ctrl+[` for next and previous
  note, `Ctrl+Shift+P` to pin, `Ctrl+Shift+A` to archive, `Alt+Return` for Note
  Information.
- **Editor V2 — live visual Markdown styling.** While the file on disk stays plain
  Markdown, the editor now renders a visual hierarchy: headings at real relative sizes,
  bold as bold and italic as italic (including bold+italic together and one level of
  nesting), and appropriate treatment for strikethrough, highlight, inline code, fenced
  code blocks, quotes, lists, checklists, links, and dividers. Marker punctuation
  (`**`, `#`, `~~`, `==`, `` ` ``, list and quote prefixes) stays visible but is
  visually subdued rather than hidden. Malformed or unsupported Markdown is left
  untouched. Styling is recomputed by a debounced whole-buffer pass after edits, and
  the Bold and Italic toolbar buttons reflect the formatting active at the cursor or
  selection.
- Anonymous, UUID-derived labels for locked encrypted notes: each locked row reads
  `Locked Note · XXXXXXXX`, where the suffix is derived only from the note's own UUID
  (the identifier already used as its on-disk filename) so multiple locked notes are
  distinguishable without revealing anything the ciphertext protects.
- A GUI-independent selection coordinator with nested event suppression and stable
  UUID row targets.
- Adaptive Library and Notes/Editor navigation built on libadwaita breakpoints, with
  explicit empty states for notes, Unfiled, and Trash.
- Expanded regression and stress coverage: 100 sequential note creations, rapid
  selection coalescing, create/rename/pin/trash/restore flows, smart-view switching,
  targeted context actions, formatting toggle-on/off, overlapping bold and italic,
  heading normalisation, and search matching body text while locked content cannot.

### Changed

- Redesigned the three-pane interface with calmer spacing, note cards, clearer
  hierarchy, editor margins, and a restrained accent system.
- Body autosave updates the current model and file without rebuilding the note list or
  editor. Titles stay in-memory drafts while typing and commit separately after 1.5
  seconds or an explicit commit event.
- GTK callbacks finish every application-state borrow before changing widgets or
  starting another transition; right-click menus carry their note UUID directly and no
  longer force a selection change before opening.
- New Note is created in the currently selected notebook, falling back to Unfiled from
  any smart view.
- While an encrypted note is unlocked, its real title, preview, and tags appear in the
  sidebar like any other open note. This is read from the same in-memory,
  never-persisted note list every other summary already uses — not a new disk-backed
  cache. Locking the note (manually, on a timer, or at restart) immediately discards
  that decrypted copy and reverts the row to the anonymous locked placeholder.
- A locked encrypted note's protected fields (pinned, archived) are never guessed while
  it is locked. They read as unset until it is unlocked, so a locked note stays out of
  the Pinned, Archive, and Recently Edited views (which are built from protected
  metadata) while remaining fully visible in All Notes and in its real notebook, since
  notebook membership is the file's location and not part of the ciphertext. See
  `SECURITY.md`.
- GtkSourceView's own built-in Markdown syntax highlighting is disabled on the editor
  buffer so Editor V2's span-driven styling is the single, deliberate source of visual
  Markdown formatting.

### Fixed

- Fixed a GTK formatting crash: a fatal `Gtk-WARNING` about snapshotting a widget
  without a current allocation, reachable by rapidly triggering formatting controls.
  The toolbar's active-state update was running reentrantly from inside a text-buffer
  mutation; it is now deferred to a clean top-level main-loop turn and skips redundant
  work.
- `Ctrl+B` no longer also applies italic. Fenced code-block detection was added so
  disabling the built-in highlighter does not regress code-block styling.
- The Markdown formatting toolbar now toggles formatting instead of stacking markers.
  Bold, italic, strikethrough, highlight, and inline code remove their existing pair
  when the selection or surrounding span already carries it; bold and italic are told
  apart by `*` run parity; Style → Heading normalises an existing heading prefix and
  toggles back to a paragraph rather than prepending another `# `.
- Fixed sorting regressions found in acceptance testing: Title A–Z / Z–A ordering, and
  Date Created falling back to the modification time because `NoteSummary` had no
  creation timestamp to sort by.
- An unlocked encrypted note's sidebar row now shows its real title, preview, and tags
  instead of remaining on the locked placeholder while it is open.
- Switching to a view, including Unfiled, no longer launches a password prompt on its
  own. Automatic fallback selection can land on a locked note and show its placeholder,
  but a password dialog appears only when the user directly acts on that specific note.
- The password dialog no longer aborts the process after a valid password is entered.
  Audited across Encrypt Note, Unlock, Change Password, and Remove Encryption, and
  verified with real GTK interaction under `G_DEBUG=fatal-warnings`.
- Change Password on an encrypted note now re-keys the container and verifies that the
  old password fails and the new one succeeds before reporting success, and hands the
  UI a freshly verified session so a later autosave cannot re-encrypt under the old key.
- Password prompts are now windows SenatorialNotes controls rather than self-closing
  dialogs, so a rejected password states a specific reason. The 12-character minimum
  passphrase length is stated up front.
- Remove Encryption now shows an explicit plaintext-on-disk warning and a deliberate
  confirmation before asking for the current password.
- Resizing the window very small no longer triggers a fatal `Adwaita-WARNING` about a
  stack exceeding the window size. The window now has a real minimum size and the
  compact breakpoint carries the whole collapsed layout.
- Rapid note switching no longer stalls or drops the final intent: clean plaintext
  notes are served from a stamp-validated in-memory cache, a burst of row clicks is
  coalesced to a single load of the final target, and the filesystem watcher no longer
  rescans the vault in response to SenatorialNotes' own atomic writes.
- Programmatic note loading and formatting no longer fight GtkSourceView's undo system,
  removing "Cannot begin/end irreversible action" warnings.
- The note list uses one shared context-menu popover owned by the list rather than one
  parented to every row, removing `Finalizing GtkListBoxRow … still has children`
  warnings.
- The search placeholder matches the active view (All Notes, Unfiled, Trash).

### Security

- Search matches plaintext title, body, and tags entirely against in-memory summaries.
  There is no network access and no persistent plaintext search index. Locked encrypted
  notes never populate a searchable body or tag, so their contents cannot match.
- Locked encrypted notes keep their title, body, tags, and private metadata (including
  pinned, archived, and tags) inside authenticated ciphertext. Decrypted summaries exist
  only in memory for the current session while a note is unlocked and are dropped on
  locking; nothing decrypted is written to a file, index, or cache.
- Moving a `.snote` file between notebooks does not re-encrypt it and does not weaken
  its authentication, because the file path was never authenticated as associated data.
- Locked notes use a non-secret UUID-derived suffix for their row label rather than
  anything derived from a protected field.
- Encrypted notes use neutral filenames and never write plaintext crash-recovery files
  or persistent plaintext search data.

### Performance and hardening

- Notebooks, tags, and sorting were stress-tested at a realistic vault scale (25
  notebooks up to two levels deep, 300 notes with mixed tag, pinned, and archived
  state). Listing, scanning, and every sort order complete well within budget, and
  sorting is confirmed never to write to a note file.
- The Markdown live-preview parser is benchmarked against a realistic ~240 KB document
  to stay comfortably fast, since it runs after every debounced keystroke pause.

### Design note

An earlier plan explored hiding Markdown markers entirely, Obsidian- or Typora-style,
using GTK's invisible-text tag, gated behind a prototype. That prototype hit a
reproducible fatal defect in GTK4's own text engine when the cursor is moved
programmatically through a buffer containing any invisible-tagged span — reproducible
with plain ASCII, and with no viable application-level workaround for that navigation
model. Editor V2 therefore never uses invisible text or custom cursor navigation, and
Markdown markers remain visible but subdued.

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

[Unreleased]: https://github.com/SenatorialNotes/SenatorialNotes/compare/v0.2.0-alpha...HEAD
[0.2.0-alpha]: https://github.com/SenatorialNotes/SenatorialNotes/compare/v0.1.0-alpha...v0.2.0-alpha
[0.1.0]: https://github.com/SenatorialNotes/SenatorialNotes/releases/tag/v0.1.0-alpha
