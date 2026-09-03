# Changelog

All notable changes to SenatorialNotes will be documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and releases use semantic versioning.

## [Unreleased]

Nothing yet.

## [0.3.0-alpha] - 2026-09-03

This release introduces the vault architecture: more than one vault, an in-app
vault switcher, an advisory vault lock, and whole-vault encryption ("Secure
Vaults") alongside the existing per-note `.snote` encryption. It follows a
real-machine acceptance pass on Arch Linux with Hyprland. It remains an alpha:
the interface and the on-disk layout may still change before 1.0, though the
vault manifest schema, the encrypted-vault container, the `.snote` container,
and the HKDF label set are treated as format-stable from here.

### Added

- **Multiple vaults and an in-app vault switcher.** A header control lists the
  current vault and recent vaults, opens a folder picker, and switches vaults
  without restarting. Missing or moved recent-vault paths are shown as
  unavailable rather than silently opened or dropped. Per-vault last state
  (selected view, note, editor scroll) is restored where practical.
- **Vault type architecture.** Every vault is a **Standard Vault**
  (`vault.toml` `format_version = 2`, plaintext Markdown and `.snote` files,
  exactly as before) or a **Secure Vault** (`format_version = 3`, whole-vault
  encryption). The kind is recorded in `vault.toml` and never changes
  implicitly.
- **Lossless `vault.toml` migration.** An existing `format_version = 1` vault is
  upgraded in place to `format_version = 2` (Standard) before any note or
  directory is touched, preserving `vault_id` and `created_at`. A vault whose
  manifest could not be written back opens read-only rather than partially
  migrated. A `format_version` newer than this build understands is refused
  without touching anything.
- **Advisory vault lock.** Opening a vault for writing creates
  `.senatorial-notes/vault.lock` (pid, hostname, boot id, app version, time — no
  secrets) held for the session and released on clean exit. A vault already in
  use offers Open Read-Only or Cancel; a stale lock is **never** removed
  automatically — the lock's contents are shown and a takeover requires the
  lock to be provably dead (a different boot, a gone process, or a positively
  reused pid). A different host is always treated as possibly live.
- **Secure Vaults (whole-vault encryption).** Create a new vault encrypted with
  one **Vault Password**. Argon2id (64 MiB, three iterations, one lane) derives
  a key-encryption key that unwraps a single random 32-byte vault master key;
  HKDF-SHA256 derives per-domain subkeys; each object is sealed with
  XChaCha20-Poly1305 and a fresh nonce. Note bodies, titles, tags, notebook
  names and tree, trash, recovery drafts, and per-vault UI state are all stored
  as opaque authenticated blobs with random names under
  `.senatorial-notes/store/`. The only plaintext kept is what is needed to
  identify and open the vault: `vault_id`, `format_version`, `kind`, and the
  advisory-lock metadata. See
  [`docs/ENCRYPTED_VAULT_FORMAT.md`](docs/ENCRYPTED_VAULT_FORMAT.md).
- **Secure Vault lock lifecycle.** A Secure Vault opens locked and shows an
  unlock screen; nothing is decrypted until the correct Vault Password is
  entered. Argon2id runs on a worker thread, never on the UI thread. "Lock
  Vault", an idle timer, and losing window focus all drop the in-memory keys
  (zeroized on drop), clear the decrypted note list, search state, and editor,
  and return to the lock screen. **Change Vault Password** re-wraps the vault
  master key only — **no note blob is re-encrypted** — and the old password
  stops working (verified after the write). Search works only while unlocked and
  is never written to a persistent index.
- **Per-note `.snote` encryption inside a Secure Vault.** Individually encrypted
  notes remain separately supported. Inside a Secure Vault a `.snote` is an
  **additional inner layer** with its own password: unlocking the vault does not
  unlock the note, and peeling the outer layer always yields byte-identical
  `.snote` container data.
- **Secure → Standard export.** An explicit action builds a **new, separate
  Standard Vault** containing unencrypted plaintext Markdown copies of every
  live note, the full notebook tree (including empty notebooks), all metadata
  (tags, favourite, pinned, archive), byte-identical inner `.snote` containers,
  and Trash. The exported vault gets its own `vault_id`; the source Secure Vault
  is not modified. The user re-enters the Vault Password (used only to derive
  the export worker's key material, never stored); the decrypt-and-build work
  runs on a worker thread with a progress display and a Cancel button. The
  export is directory-transactional: it is built in an application-owned
  temporary directory and made the destination by a single atomic rename, so a
  failure leaves no partial vault at the destination and the source untouched.
  **In v0.3 the export refuses a Secure Vault that contains attachment
  records** (`ExportUnsupportedContent`), because the Standard Vault has no
  attachment representation yet and silently dropping them is not acceptable; no
  current build can create such a vault. Recovery drafts and session/transient
  state are not exported. In-place conversion of a vault in either direction
  remains deferred to a later release.
- **R18 — plaintext-conflict detection with explicit-consent quarantine.** A
  `v0.1` / `v0.2` binary that opens a Secure Vault cannot read `vault.toml` and
  may write a plaintext note into a top-level `Notes/` folder. On opening a
  Secure Vault, SenatorialNotes detects such stray plaintext (a `Notes/` or
  `Trash/` folder holding `.md`/`.snote`, an `Attachments/` folder holding any
  file, or a stray top-level `.md`/`.snote`) and opens the vault **read-only**
  without moving anything. The user chooses Cancel, Open Read-Only (the files
  are left exactly where they are), or **Quarantine Plaintext Files…**, which
  moves them **unchanged** by same-filesystem rename into
  `.senatorial-notes/quarantine/<timestamp>/`. Nothing is ever deleted, merged,
  imported, or parsed into encrypted storage. If the move fails, every original
  file is preserved and the vault stays read-only or unopened. Empty legacy
  directories and unrelated files never trigger it.
- **Note-header quick actions.** A lock/encrypt toggle, favourite, pin, and an
  overflow menu (rename, move, archive, encryption, note information, delete) in
  the editor's title row.
- **Favourites** as an additive front-matter field independent of Pinned, with
  a Favourites smart view, and a **Recently Opened** view (recorded when a note
  is displayed, never on save — it never rewrites a note file). For a Secure
  Vault this session state is sealed inside the encrypted manifest, not the
  plaintext app config.
- A focused **Secure Vault Settings** window (auto-lock timing and triggers,
  Change Vault Password, Rename Vault, Export to Standard Vault). A Standard
  Vault gets only the general settings.
- **Rename Vault** changes only the name SenatorialNotes shows for a vault; it
  never moves the folder, renames the directory, or re-encrypts anything.
- New encrypted-vault, encrypted-lifecycle, corruption-matrix, quarantine,
  export, vault-lock, vault-switching, and vault-manifest test suites. The
  automated suite is now 318 tests across 21 binaries.

### Changed

- Product terminology is **"Standard Vault"** and **"Secure Vault"** in the
  interface. "Encrypted Note" continues to mean a per-note `.snote` container.
  On-disk names, enum values, and formats are unchanged.
- `open_vault` validates the target and decides the advisory lock **before**
  disturbing the current session; the outgoing vault is flushed and its lock
  released only after a successful save.
- The filesystem watcher is advisory-only for a Secure Vault: it cannot merge
  hand-edited ciphertext, so it prompts for a reload rather than attempting a
  content merge, and never writes a plaintext recovery file. It does not parse
  the encrypted store.
- The sidebar groups NOTES (All Notes / Recently Opened / Favourites / Pinned /
  Archive) and SECURED VAULTS (a bounded list of recent Secure Vaults), with
  Trash at the bottom.

### Security

- A Secure Vault's ciphertext binds `vault_id`, `object_uuid`, and
  `object_type` as associated data but **not** the note's path, so moving or
  renaming a note never re-encrypts it, while a blob from another vault, a
  swapped blob, or a blob passed off as the wrong object type all fail
  authentication. A wrong Vault Password, a tampered `vault.keys`, or a single
  flipped byte in any blob fails safely: the vault or that note reads as
  unavailable and unauthenticated plaintext is never returned. A crash between
  writing a blob and re-sealing the manifest is reconciled on the next unlock —
  an orphan blob is moved to `store/orphans/`, never deleted, and no note is
  lost.
- **Secure → Standard export produces unencrypted plaintext on disk** and
  therefore requires the Vault Password to be re-entered even though the vault
  is already unlocked, shows an explicit plaintext-on-disk warning, and writes
  only to a new empty folder. The source Secure Vault is byte-for-byte
  unchanged.
- What remains visible on disk for a locked Secure Vault: the number of blob
  files and each blob's size and modification time, the total vault size, the
  `created_at` and `vault_id` in `vault.toml`, and that the folder is a locked
  SenatorialNotes vault. Blob-size padding is future work.
- Memory zeroization for vault keys and decrypted buffers is best-effort
  (`Zeroizing`); it does not defeat swap, hibernation, or a privileged memory
  scraper, and the format makes no such promise. Locking is not a defence
  against a compromised running computer.
- Old `v0.1` / `v0.2` binaries provide no forward-compatibility guarantee and
  cannot safely open a Secure Vault; the structural containment (the encrypted
  store lives entirely under `.senatorial-notes/`) plus R18 detection keep an
  old binary's plaintext writes separate from ciphertext rather than
  intermingled.

### Deviations from the design documents

- The encrypted vault uses a single sealed manifest (`store/manifest`) rather
  than per-directory manifests; `created_at` stays in the plaintext `vault.toml`
  for both vault kinds; the storage abstraction is a backend enum on `Vault`,
  not a trait; the off-main-thread key derivation uses `gio::spawn_blocking`.
  The `SNENC` object header is 80 bytes; the `SNVLT` keyfile header is 88 bytes.
- The `k_metadata` and `k_index` HKDF subkeys are derived but currently unused
  (reserved for a per-vault metadata blob and an encrypted local index).

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

[Unreleased]: https://github.com/SenatorialNotes/SenatorialNotes/compare/v0.3.0-alpha...HEAD
[0.3.0-alpha]: https://github.com/SenatorialNotes/SenatorialNotes/compare/v0.2.0-alpha...v0.3.0-alpha
[0.2.0-alpha]: https://github.com/SenatorialNotes/SenatorialNotes/compare/v0.1.0-alpha...v0.2.0-alpha
[0.1.0]: https://github.com/SenatorialNotes/SenatorialNotes/releases/tag/v0.1.0-alpha
