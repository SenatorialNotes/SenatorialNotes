# SenatorialNotes

SenatorialNotes is a native, local-only Markdown notes application for Linux. It is built with Rust, GTK4, libadwaita, and GtkSourceView 5. There is no account, cloud service, telemetry, analytics, runtime networking, or browser engine.

> [!IMPORTANT]
> This repository is at version `0.3.0-alpha`. It is an alpha: the interface and the on-disk layout may still change before 1.0, though the vault manifest schema, the encrypted-vault container, and the encrypted-note container are treated as format-stable from here. It is not yet the complete 1.0 application described in [SPECIFICATION.md](SPECIFICATION.md).

## What SenatorialNotes is

- Native Linux desktop application (Rust, GTK4, libadwaita, GtkSourceView 5).
- Local-only. Notes are ordinary files in a folder you choose.
- No account, no cloud, no sync, no telemetry, no analytics, no built-in runtime networking.
- Markdown is the authoritative format for every note.
- More than one vault, each a **Standard Vault** (plaintext Markdown) or a **Secure Vault** (whole-vault encryption).
- Optional strong per-note encryption for individual notes, independently of the vault.

## What works in 0.3.0-alpha

### Vaults

- More than one vault, with an in-app switcher in the header: current vault, recent vaults, a folder picker, and switching without restarting. Missing or moved recent-vault paths are shown as unavailable rather than opened blindly or dropped.
- Every vault is a **Standard Vault** (`vault.toml` `format_version = 2`; plaintext Markdown and `.snote` files, exactly as before) or a **Secure Vault** (`format_version = 3`; whole-vault encryption). The kind is recorded in `vault.toml` and never changes on its own.
- A `format_version = 1` vault from v0.1/v0.2 migrates in place to `format_version = 2` (Standard) before any note is touched, preserving `vault_id` and `created_at`; a vault whose manifest cannot be rewritten opens read-only.
- An advisory `vault.lock` (pid, hostname, boot id, app version, time — no secrets) so two writable instances cannot edit one vault at once. A vault already in use offers Open Read-Only or Cancel; a stale lock is never removed automatically, and a takeover requires the lock to be provably dead.
- **Secure Vaults.** Create a vault encrypted with one **Vault Password**: Argon2id → a key-encryption key that unwraps a random vault master key → HKDF-SHA256 per-domain subkeys → XChaCha20-Poly1305 per object. Note bodies, titles, tags, notebook names and tree, trash, and per-vault UI state are opaque authenticated blobs with random names under `.senatorial-notes/store/`. A Secure Vault opens locked; Argon2id runs off the UI thread; "Lock Vault", an idle timer, and focus loss drop the in-memory keys and clear the decrypted state. Changing the Vault Password re-wraps the vault master key only and re-encrypts no note.
- A per-note `.snote` **inside** a Secure Vault is an additional inner layer with its own password; unlocking the vault does not unlock the note.
- **Secure → Standard export.** An explicit action builds a new, separate Standard Vault with plaintext copies of every live note, the notebook tree, all metadata, byte-identical `.snote` containers, and Trash. The user re-enters the Vault Password (used only to derive the export worker's keys); the work runs off the UI thread with progress and Cancel; the export is atomic (built in a temporary directory, then renamed into place) and never modifies the source. In v0.3 it refuses a Secure Vault that holds attachment records, and does not export recovery drafts or session state. In-place conversion of a vault in either direction is deferred to a later release.
- **Plaintext-conflict detection (R18).** Opening a Secure Vault whose folder also holds plaintext an older or incompatible binary wrote opens the vault read-only and offers Cancel, Open Read-Only, or **Quarantine Plaintext Files…**, which moves the files unchanged into `.senatorial-notes/quarantine/<timestamp>/`. Nothing is ever deleted, merged, or imported.

### Notes and storage

- Local vault creation and opening with the native folder picker on first run.
- Ordinary UTF-8 Markdown notes with YAML front matter and stable UUIDs.
- Independent body autosave and title-commit paths: body saves never rebuild the editor or rename a file; title changes commit on Enter, focus loss, note change, exit, or a 1.5-second debounce.
- Safe title-based file renaming that keeps the stable note UUID and never overwrites a collision.
- Atomic file replacement with flush, rename, directory sync, and existing-permission preservation.
- External-modification detection before overwriting an open note, with a local recovery copy after a save failure.
- Startup verification scan plus native filesystem watching and local rescans.
- Recent and last-opened vault state stored locally.
- Single-instance activation through the application ID.
- UUID-targeted context actions for rename, pin/unpin, encryption, Trash, restore, and permanent deletion.

### Organisation

- Real notebooks: create, rename, and safely delete them, including nested child notebooks. Deletion refuses whenever the subtree still holds a note or any file SenatorialNotes does not manage, and never does a recursive directory removal.
- A built-in default notebook shown as **Unfiled** in the sidebar, the Note Information panel, and the Move to Notebook dialog. Its on-disk directory stays named `Inbox` so existing vaults keep working; you never need to touch it manually.
- Move notes between notebooks, including encrypted notes (a move is a plain filesystem rename, never a re-encryption).
- Tags: add and remove them in the editor, and filter the note list from a sidebar tag section, with case-insensitive duplicate prevention.
- Archive, with an Archive smart view.
- Smart views: All Notes, Unfiled, Pinned, Recently Edited, Archive, and Trash.
- Trash with restore to the original notebook, confirmed permanent deletion, and confirmed Empty Trash.
- User-controlled sort order (Last Edited, Date Created, Title A–Z, Title Z–A) alongside the default pinned-first order. Choosing a sort order never rewrites note files.
- Composed filtering: notebook, tag, and text search combine.
- A Note Information panel with title, notebook, tags, created and modified times, pinned and archived state, encryption status, and word and character counts, with the vault-relative path and UUID under an Advanced expander.

### Editor

- Responsive Library, note-list, and editor navigation for wide, normal, and narrow windows, built on libadwaita breakpoints, with explicit empty states.
- GtkSourceView-based Markdown editor with wrapping, undo, redo, native selection, and clipboard behaviour.
- Editor V2 live visual styling: headings render at a real visual hierarchy, bold renders as bold and italic as italic (including bold+italic together and one level of nesting), and strikethrough, highlight, inline code, fenced code blocks, quotes, lists, checklists, links, and dividers get appropriate visual treatment.
- Markdown remains the canonical text on disk. Syntax markers such as `#`, `*`, `**`, `` ` ``, and `~~` stay **visible but visually subdued** rather than hidden. This is a deliberate choice, not a missing feature (see [The editor](#the-editor) below).
- A formatting toolbar and keyboard actions for headings, emphasis, highlight, code, quotes, lists, checklists, links, and dividers that toggle formatting rather than stacking markers.
- Keyboard: `Ctrl+Shift+N` new notebook, `Ctrl+1` / `Ctrl+2` focus the note list or editor, `Ctrl+]` / `Ctrl+[` next / previous note, `Ctrl+Shift+P` pin, `Ctrl+Shift+A` archive, `Alt+Return` Note Information.
- Preferences for system/light/dark appearance, editor font and size, spacing, Comfortable/Wide/Full editor width, line numbers, note density, preview length, and a curated accent, with matching light/dark editor colour schemes.

### Encryption

- Two independent layers: **per-note** `.snote` encryption (below) and **whole-vault** encryption (see [Vaults](#vaults)). A `.snote` works in either vault kind; inside a Secure Vault it is a second, inner layer with its own password.
- Individually encrypted `.snote` files using Argon2id and XChaCha20-Poly1305, with neutral filenames.
- Encrypt Note, Unlock, Lock Now, change-password, and remove-encryption flows. Derived keys are session-memory only and are discarded on exit.
- Configurable automatic locking on note switch, focus loss or minimise, or a timer.
- The title, body, tags, and private metadata (including pinned and archived state) of a locked encrypted note stay inside authenticated ciphertext.
- While a note is unlocked, its decrypted title, preview, and tags are held only in memory for the session and shown in the sidebar like any other note; locking discards that in-memory copy and reverts the row to an anonymous placeholder.
- Locked notes are labelled with an anonymous `Locked Note · XXXXXXXX` identifier derived only from the note's own UUID, so multiple locked notes are distinguishable without leaking their titles.
- Search runs entirely against in-memory summaries with no network access and no persistent plaintext index; locked notes never populate searchable content.

### Testing

- Storage, title-regression, Trash, notebook, tag, sorting, formatting, configuration, per-note encryption, tamper, plaintext-leak, path-safety, callback-suppression, targeted-context-action, and large-vault stress tests, plus an automated forbidden HTTP-client dependency check.
- Vault-manifest migration, multi-vault switching, advisory-lock classification, whole-vault encryption, encrypted-vault lifecycle and corruption matrices, plaintext-conflict quarantine, and Secure → Standard export tests. The automated suite is 318 tests across 21 binaries.

## The editor

SenatorialNotes styles Markdown visually while keeping the Markdown itself as the text on disk. It is **not** a full WYSIWYG editor: the syntax markers stay on screen, only dimmed.

An earlier plan for this release explored hiding the markers entirely (Obsidian- or Typora-style). That approach was prototyped first and rejected: it triggered a reproducible fatal crash in GTK4's own text engine when the cursor was moved programmatically through a buffer containing hidden text, with no workable application-level fix for that navigation model. Keeping the markers visible but subdued is the deliberate result.

## Privacy

SenatorialNotes does not contain networking functionality. It does not transmit notes, metadata, diagnostics, location, or usage information. All application data stays on the user's device unless the user manually copies or exports it.

The application has no HTTP client dependency and requests no network access in its Flatpak manifest. Build tools may access package sources while resolving Rust dependencies; the finished application does not.

## Storage format

Markdown files are the source of truth. A new vault starts with this layout:

```text
My Notes/
├── Notes/
│   └── Inbox/
├── Attachments/
├── Trash/
└── .senatorial-notes/
    ├── vault.toml
    ├── history/
    └── recovery/
```

This is a **Standard Vault**. Its `.senatorial-notes/vault.toml` records `format_version = 2` and `kind = "ordinary"`. The `Notes/Inbox/` directory is the one shown in the app as **Unfiled**; new notebooks you create are sibling directories under `Notes/`. Each note is an ordinary Markdown file:

```markdown
---
id: 550e8400-e29b-41d4-a716-446655440000
title: Example note
created_at: 2026-08-25T17:30:00Z
updated_at: 2026-08-25T17:45:00Z
tags: []
pinned: false
---
This is the note body.
```

Unknown front-matter fields are retained during load/save round trips. The disposable application cache lives at `~/.cache/senatorial-notes/`; it is not authoritative. Application settings live at `~/.config/senatorial-notes/config.toml`.

Encrypted notes use neutral filenames such as `encrypted--12345678.snote`. Their title, body, tags, and private metadata are inside authenticated ciphertext. The clear header contains only format/KDF information and the stable UUID. Moving a `.snote` file between notebooks is a plain rename because the path is not part of the authenticated header. See [the encrypted-note format](docs/ENCRYPTED_NOTE_FORMAT.md).

A **Secure Vault** (`vault.toml` `format_version = 3`, `kind = "encrypted"`) has no plaintext `Notes/`, `Trash/`, or `Attachments/` tree. Everything sensitive is stored under `.senatorial-notes/store/` as opaque `SNENC` authenticated-encryption blobs with random names, described by one sealed manifest; the wrapped key material is in `.senatorial-notes/vault.keys`. The only plaintext is `vault_id`, `format_version`, `kind`, `created_at`, and the advisory-lock file. See [the encrypted-vault format](docs/ENCRYPTED_VAULT_FORMAT.md).

## Build on Arch Linux

Install the native toolchain and development libraries:

```bash
sudo pacman -S --needed base-devel rust gtk4 libadwaita gtksourceview5
```

Then build and run:

```bash
cargo build
cargo run
```

For the storage-only test suite on a machine without the GTK development libraries:

```bash
cargo test --no-default-features
```

Runtime libraries are GTK4, libadwaita, and GtkSourceView 5. Rust and Cargo are build-time dependencies.

## Development checks

Before submitting a change, run:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo build --release
```

Tests only operate in temporary directories. Do not add personal notes or real vaults as fixtures.

## Desktop installation

During development, the binary can be installed locally with Cargo. Desktop metadata is under `data/`. Release packaging will install:

- `senatorial-notes`
- `io.github.SenatorialNotes.SenatorialNotes.desktop`
- `io.github.SenatorialNotes.SenatorialNotes.metainfo.xml`
- the `io.github.SenatorialNotes.SenatorialNotes` scalable icon

The files under `packaging/arch/` and `packaging/flatpak/` are release scaffolding. They are intentionally not described as production-ready until a source release archive exists at the future `SenatorialNotes/SenatorialNotes` repository.

## Known limitations

Version `0.3.0-alpha` does not yet provide indexed full-text search, the complete three-choice external-conflict dialog, attachments, history browsing, a separately rendered preview, note import, or the complete keyboard-shortcut window. Vault conversion is one-way and partial: a Secure Vault can be exported to a new plaintext Standard Vault, but there is no in-place conversion in either direction, and the export refuses a Secure Vault that contains attachment records. Blob-size padding for Secure Vaults is future work.

The scanner expects managed Markdown files to contain SenatorialNotes YAML front matter. Automatically adopting a raw externally created `.md` file is future work. Storage and rescans are currently synchronous; background indexing and large-vault responsiveness work is also pending.

If another program changes the note currently open, SenatorialNotes refuses to overwrite it and preserves a local recovery copy. The full three-choice conflict dialog is future work.

This release passed the interactive Arch/Hyprland acceptance checklists in [`docs/STABILITY_TEST_PLAN.md`](docs/STABILITY_TEST_PLAN.md) — the base stability pass and the new Secure Vault gate (Secure Vault basics, R18 quarantine, and Secure → Standard export) — in addition to the automated regression and stress suite.

## Security

In a Standard Vault, ordinary `.md` notes are plaintext; notes explicitly converted to `.snote` are encrypted at rest. In a Secure Vault, everything is encrypted at rest while the vault is locked. Either way the key is derived from a password with no recovery mechanism, and exporting a Secure Vault to a Standard Vault writes unencrypted plaintext to disk and re-asks for the Vault Password first. Full-disk or home-directory encryption remains recommended because it also protects swap, caches, the plaintext `vault.toml`, and other local data. Read [SECURITY.md](SECURITY.md) for the threat model.

## Packaging

- Arch: see [`packaging/arch/README.md`](packaging/arch/README.md).
- Flatpak: see [`packaging/flatpak/README.md`](packaging/flatpak/README.md).

No packaging process publishes a release automatically.

## Screenshots

Screenshots will be added after the application has been visually validated for publication.

## Roadmap

### v0.1 — Foundation and hardening (delivered)

- stable local Markdown notes
- responsive native GTK interface
- autosave and recovery
- per-note encryption
- local title/body/tag search
- crash resistance and real-machine testing

### v0.2 — Organisation and editor (delivered)

- proper notebooks and nested notebooks
- safe notebook creation, rename, and delete
- note movement between notebooks
- tags and tag filtering
- Archive, Pinned, and Recently Edited smart views
- user-controlled sorting and composed filtering
- keyboard-navigation improvements
- Note Information panel
- Editor V2 live visual Markdown styling

### v0.3 — Vault architecture (this release)

- multiple Senatorial vaults with an in-app switcher
- Standard Vaults (`format_version = 2`) and Secure Vaults (`format_version = 3`, whole-vault encryption)
- lossless `format_version = 1` → `2` migration and an advisory vault lock
- continued support for individually encrypted `.snote` notes, including inside a Secure Vault
- Secure → Standard export to a new plaintext vault
- plaintext-conflict detection with explicit-consent quarantine

In-place conversion of a vault between Standard and Secure, in either direction, is deferred to a later release. SenatorialNotes manages information inside Senatorial vaults; it is not intended to become a general-purpose file manager.

### Later releases — Local document management (future)

Longer-term direction includes:

- attachments
- PDFs and documents
- document metadata and versioning
- checksums and integrity verification
- duplicate detection
- previews
- local indexing
- local-only OCR
- encrypted document/index storage where appropriate

No SenatorialNotes cloud service, account system, telemetry, or built-in synchronisation service is planned.

## Contributing

Contributions are welcome. Read [CONTRIBUTING.md](CONTRIBUTING.md) and the [Code of Conduct](CODE_OF_CONDUCT.md). By contributing, you agree that your work is licensed under GPL-3.0-or-later.

## License

SenatorialNotes is licensed under the GNU General Public License, version 3 or later. See [LICENSE](LICENSE).
