# SenatorialNotes

SenatorialNotes is a native, local-only Markdown notes application for Linux. It is built with Rust, GTK4, libadwaita, and GtkSourceView 5. There is no account, cloud service, telemetry, analytics, runtime networking, or browser engine.

> [!IMPORTANT]
> This repository is at version `0.2.0-alpha`. It is an alpha: the interface, the on-disk layout, and the encrypted-note container may still change before 1.0. It is not yet the complete 1.0 application described in [SPECIFICATION.md](SPECIFICATION.md).

## What SenatorialNotes is

- Native Linux desktop application (Rust, GTK4, libadwaita, GtkSourceView 5).
- Local-only. Notes are ordinary files in a folder you choose.
- No account, no cloud, no sync, no telemetry, no analytics, no built-in runtime networking.
- Markdown is the authoritative format for every note.
- Optional strong per-note encryption for individual notes.

## What works in 0.2.0-alpha

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

- Individually encrypted `.snote` files using Argon2id and XChaCha20-Poly1305, with neutral filenames.
- Encrypt Note, Unlock, Lock Now, change-password, and remove-encryption flows. Derived keys are session-memory only and are discarded on exit.
- Configurable automatic locking on note switch, focus loss or minimise, or a timer.
- The title, body, tags, and private metadata (including pinned and archived state) of a locked encrypted note stay inside authenticated ciphertext.
- While a note is unlocked, its decrypted title, preview, and tags are held only in memory for the session and shown in the sidebar like any other note; locking discards that in-memory copy and reverts the row to an anonymous placeholder.
- Locked notes are labelled with an anonymous `Locked Note · XXXXXXXX` identifier derived only from the note's own UUID, so multiple locked notes are distinguishable without leaking their titles.
- Search runs entirely against in-memory summaries with no network access and no persistent plaintext index; locked notes never populate searchable content.

### Testing

- Storage, title-regression, Trash, notebook, tag, sorting, formatting, configuration, encryption, tamper, plaintext-leak, path-safety, callback-suppression, targeted-context-action, and large-vault stress tests, plus an automated forbidden HTTP-client dependency check.

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

The `Notes/Inbox/` directory is the one shown in the app as **Unfiled**; new notebooks you create are sibling directories under `Notes/`. Each note is an ordinary Markdown file:

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

Version `0.2.0-alpha` does not yet provide indexed full-text search, the complete three-choice external-conflict dialog, attachments, history browsing, a separately rendered preview, import/export, or the complete keyboard-shortcut window.

The scanner expects managed Markdown files to contain SenatorialNotes YAML front matter. Automatically adopting a raw externally created `.md` file is future work. Storage and rescans are currently synchronous; background indexing and large-vault responsiveness work is also pending.

If another program changes the note currently open, SenatorialNotes refuses to overwrite it and preserves a local recovery copy. The full three-choice conflict dialog is future work.

This release passed the interactive Arch/Hyprland acceptance checklist in [`docs/STABILITY_TEST_PLAN.md`](docs/STABILITY_TEST_PLAN.md) in addition to the automated regression and stress suite.

## Security

Ordinary `.md` notes are plaintext. Notes explicitly converted to `.snote` are encrypted at rest and require the correct password-derived key. There is deliberately no recovery mechanism. Full-disk or home-directory encryption remains recommended because it also protects ordinary notes, swap, caches, and other local data. Read [SECURITY.md](SECURITY.md) for the threat model.

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

### v0.2 — Organisation and editor (this release)

- proper notebooks and nested notebooks
- safe notebook creation, rename, and delete
- note movement between notebooks
- tags and tag filtering
- Archive, Pinned, and Recently Edited smart views
- user-controlled sorting and composed filtering
- keyboard-navigation improvements
- Note Information panel
- Editor V2 live visual Markdown styling

### v0.3 — Vault architecture (future)

- multiple Senatorial vaults
- ordinary vaults
- encrypted vaults (whole-vault encryption)
- continued support for individually encrypted notes

SenatorialNotes will manage information inside Senatorial vaults. It is not intended to become a general-purpose file manager.

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
