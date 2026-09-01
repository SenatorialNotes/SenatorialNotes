# SenatorialNotes

SenatorialNotes is a native, local-only Markdown notes application for Linux. It is built with Rust, GTK4, libadwaita, and GtkSourceView 5. There is no account, cloud service, telemetry, runtime networking, or browser engine.

> [!IMPORTANT]
> This repository is at version `0.1.0` with substantial unreleased work beyond the Phase 1 foundation. It is not yet the complete 1.0 application described in [SPECIFICATION.md](SPECIFICATION.md).

## What works in 0.1.0

- Native GTK4/libadwaita application window.
- Responsive Library, note-list, and editor navigation for wide, normal, and narrow windows.
- First-run choice to create or open a local vault with the native folder picker.
- Ordinary UTF-8 Markdown notes with YAML front matter and stable UUIDs.
- Real notebook directories, with an Inbox created for new vaults.
- GtkSourceView Markdown editor with syntax highlighting, wrapping, undo, redo, native selection, and clipboard behavior.
- Independent body autosave and title-commit paths: body saves never rebuild the editor or rename a file; title changes commit on Enter, focus loss, note change, exit, or a 1.5-second debounce.
- Safe title-based file renaming that retains the stable note UUID and never overwrites a collision.
- Atomic file replacement with flush, rename, directory sync, and existing permission preservation.
- External modification detection before overwriting an open note.
- Local recovery copy after a save failure.
- Startup verification scan plus native filesystem watching and local rescans.
- Recent and last-opened vault state stored locally.
- Trash with restore-to-original-notebook, confirmed permanent deletion, and confirmed Empty Trash.
- All Notes, Inbox, and Trash smart views with explicit empty states.
- UUID-targeted context actions for rename, pin/unpin, encryption, Trash, restore, and permanent deletion.
- Formatting actions for headings, emphasis, highlight, code, quotes, lists, checklists, links, and dividers while retaining Markdown storage.
- Preferences for system/light/dark appearance, local editor font and size, spacing, Comfortable/Wide/Full editor width, line numbers, note density, preview length, and a curated accent.
- Matching light/dark GtkSourceView schemes, so the editor follows the rest of the application.
- Individually encrypted `.snote` files using Argon2id and XChaCha20-Poly1305, neutral filenames, explicit locking, password changes, and removal back to plaintext Markdown.
- Configurable encrypted-note locking on note switch, focus loss/minimize, or a timer. Keys are session-memory only and are discarded on exit.
- Title/preview filtering that never includes locked encrypted content. A full indexed search engine remains planned.
- Single-instance activation through the application ID.
- Storage, title-regression, Trash, formatting, configuration, encryption, tamper, plaintext-leak, path-safety, callback-suppression, targeted-context-action, and 100-note stress tests, including an automated forbidden HTTP-client dependency check.

## Privacy

SenatorialNotes does not contain networking functionality. It does not transmit notes, metadata, diagnostics, location, or usage information. All application data remains on the user's device unless the user manually copies or exports it.

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

Each note is an ordinary Markdown file:

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

Unknown front-matter fields are retained during load/save round trips. The disposable application cache will live at `~/.cache/senatorial-notes/`; it is not authoritative. Application settings live at `~/.config/senatorial-notes/config.toml`.

Encrypted notes use neutral filenames such as `encrypted--12345678.snote`. Their title, body, tags, and private metadata are inside authenticated ciphertext. The clear header contains only format/KDF information and the stable UUID. See [the encrypted-note format](docs/ENCRYPTED_NOTE_FORMAT.md).

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

The files under `packaging/arch/` and `packaging/flatpak/` are release scaffolding. They are intentionally not described as production-ready until the full native build has been validated and a source release exists at the future `SenatorialNotes/SenatorialNotes` repository.

## Known limitations

Version `0.1.0` does not yet provide tag editing, user-controlled sorting, indexed full-text search, the complete external-conflict choice dialog, attachments, history browsing, rendered preview, import/export, or the complete shortcut window. Notebook creation exists in the storage API, while the current UI exposes All Notes and the default Inbox.

The interaction layer has automated regression and stress coverage, but final acceptance still requires the interactive Arch/Hyprland checklist in [`docs/STABILITY_TEST_PLAN.md`](docs/STABILITY_TEST_PLAN.md).

The Phase 1 scanner expects managed Markdown files to contain SenatorialNotes YAML front matter. Automatically adopting a raw externally created `.md` file belongs to Phase 2. Storage and rescans are currently synchronous; background indexing and large-vault responsiveness work is also pending.

If another program changes the note currently open, SenatorialNotes refuses to overwrite it and preserves a local recovery copy. The full three-choice conflict dialog belongs to Phase 2.

## Security

Ordinary `.md` notes are plaintext. Notes explicitly converted to `.snote` are encrypted at rest and require the correct password-derived key. There is deliberately no recovery mechanism. Full-disk or home-directory encryption remains recommended because it also protects ordinary notes, swap, caches, and other local data. Read [SECURITY.md](SECURITY.md) for the threat model.

## Packaging

- Arch: see [`packaging/arch/README.md`](packaging/arch/README.md).
- Flatpak: see [`packaging/flatpak/README.md`](packaging/flatpak/README.md).

No packaging process publishes a release automatically.

## Screenshots

Screenshots will be added after the Phase 1 application has been rendered and visually validated on GNOME/Arch Linux.

## Roadmap

- **Phase 1 — Foundation:** storage model, atomic save, basic native vault/note/editor UI. Implemented in source; native build validation pending.
- **Phase 2 — Note management:** Trash, restore, pinning, watcher, and Preferences are implemented; tags, user-controlled sorting, indexed search, and complete conflict choices remain.
- **Phase 3 — Editing and attachments:** formatting actions are implemented; native reading view, attachments, drag-and-drop, import/export, and local history remain.
- **Phase 4 — Hardening:** accessibility pass, performance work, recovery testing, vault locks, release packaging.
- **Phase 5 — Release preparation:** screenshots, complete documentation, final packaging and release checks.

## Contributing

Contributions are welcome once the future repository is published. Read [CONTRIBUTING.md](CONTRIBUTING.md) and the [Code of Conduct](CODE_OF_CONDUCT.md). By contributing, you agree that your work is licensed under GPL-3.0-or-later.

## License

SenatorialNotes is licensed under the GNU General Public License, version 3 or later. See [LICENSE](LICENSE).
