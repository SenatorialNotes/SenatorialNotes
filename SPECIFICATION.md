# Build a Native, Local-Only Linux Notes App

You are working inside a new local project directory. Build a production-quality Linux desktop notes application with the final name **SenatorialNotes**.

Do not merely provide sample code or a development plan. Create the actual project files, implement the application, compile it, run tests, fix errors, and leave the repository in a usable state.

Do not create a GitHub repository or request GitHub authentication yet. A local Git repository is fine.

Use these identifiers consistently:

| Purpose | Identifier |
| --- | --- |
| App name | `SenatorialNotes` |
| GitHub username/owner | `SenatorialNotes` |
| Repository name | `SenatorialNotes` |
| Future repository | `github.com/SenatorialNotes/SenatorialNotes` |
| Linux executable | `senatorial-notes` |
| Rust crate/package | `senatorial-notes` |
| Application ID / Flatpak ID | `io.github.SenatorialNotes.SenatorialNotes` |
| Desktop file | `io.github.SenatorialNotes.SenatorialNotes.desktop` |
| AppStream file | `io.github.SenatorialNotes.SenatorialNotes.metainfo.xml` |
| Icon name | `io.github.SenatorialNotes.SenatorialNotes` |
| Config directory | `~/.config/senatorial-notes/` |
| Cache directory | `~/.cache/senatorial-notes/` |

## Primary Goal

Create a beautiful, fast, native notes application for Arch Linux that feels as simple and polished as Apple Notes while remaining completely local.

The application must:

- Work without an account.
- Work without an internet connection.
- Store notes locally in open, human-readable files.
- Never upload notes or metadata.
- Contain no telemetry, analytics, crash reporting, geolocation, update checks, advertisements, cloud services, or external API calls.
- Continue working when all network access is blocked.
- Use a native Linux interface rather than Electron, Tauri, Chromium, WebKit, or another embedded browser.
- Be suitable for publication as an open-source Git repository.

## Technology Requirements

Use:

- Rust stable.
- GTK4.
- libadwaita.
- gtk-rs.
- GtkSourceView 5 for the Markdown editor and syntax highlighting.
- Cargo for building and dependency management.
- SQLite FTS5 only as a disposable local search index if needed.
- Serde for configuration and metadata parsing.
- A maintained filesystem-watching crate for detecting external changes.

Do not use:

- Electron.
- Tauri.
- WebKit.
- React.
- Node.js as part of the runtime application.
- Remote fonts.
- Remote icons.
- HTTP clients such as `reqwest`, `ureq`, `hyper`, or `curl`.
- Networking libraries.
- Online spellchecking.
- Remote image loading.
- A plugin marketplace.
- A built-in updater.

Build-time internet access may be used to download declared Rust dependencies. The finished application must not perform any runtime network activity.

Use this application ID:

```text
io.github.SenatorialNotes.SenatorialNotes

```

Keep the application name and application ID centralized so code and packaging cannot drift.

## Design Direction

The interface should feel modern, calm, uncluttered, and native.

Use libadwaita components and follow GNOME human-interface conventions. Do not directly copy Apple assets, icons, or branding, but use an Apple Notes-inspired layout:

1. Notebook and smart-folder sidebar.
2. Note list.
3. Main editor.

The layout should adapt when the window becomes narrow. On smaller widths, use navigation views rather than crushing all three panes together.

Support:

- System light mode.
- System dark mode.
- Optional manual light/dark override.
- Proper HiDPI scaling.
- Keyboard-only navigation.
- Screen-reader labels.
- Clear focus states.
- Unicode and emoji.
- Left-to-right and right-to-left text where GTK supports it.

Use symbolic system icons wherever possible instead of bundling random icon packs.

## First-Run Experience

On first launch, show a simple welcome screen explaining:

- Notes remain on the user's computer.
- No account is required.
- The application has no cloud service.
- Notes are stored as Markdown files.
- Ordinary Markdown files are plaintext unless the user explicitly encrypts an individual note.
- Encrypted notes have no password recovery or backdoor.
- Full-disk or home-directory encryption is recommended for sensitive notes.

Allow the user to:

- Create a new vault.
- Open an existing vault.
- Select a folder using a native GTK folder picker.

Do not force the user into a tutorial, account flow, newsletter, privacy dialog, or cloud setup.

Remember recently opened vaults locally.

## Vault and File Format

The Markdown files are the source of truth. The application must not trap the user inside a proprietary database.

Use a structure similar to:

```text
My Notes/
├── Notes/
│   ├── Personal/
│   │   └── shopping-list--a1b2c3d4.md
│   ├── Work/
│   │   └── meeting-notes--e5f6a7b8.md
│   └── Inbox/
├── Attachments/
│   └── <note-uuid>/
├── Trash/
├── .senatorial-notes/
│   ├── vault.toml
│   ├── history/
│   └── recovery/

```

Each note must be an ordinary UTF-8 Markdown file.

Use YAML front matter for application metadata:

```markdown
---
id: "550e8400-e29b-41d4-a716-446655440000"
title: "Example note"
created_at: "2026-08-25T17:30:00Z"
updated_at: "2026-08-25T17:45:00Z"
tags:
  - example
  - personal
pinned: false
---

This is the actual note body.

```

Requirements:

- Every note gets a stable UUID.
- Note filenames should contain a sanitized title slug and a short ID.
- Renaming a note may rename the file, but must not change its UUID.
- Notebook folders should correspond to real directories.
- Attachments must use relative paths.
- Notes must remain understandable in an ordinary text editor.
- Unknown front-matter fields must be preserved whenever possible.
- Never place user note content in application logs.
- Configuration files must not contain note contents.

Store application-wide settings in:

```text
~/.config/senatorial-notes/config.toml

```

Store disposable cache and search data in:

```text
~/.cache/senatorial-notes/

```

The cache must be safe to delete. Reopening the app must rebuild it from the Markdown files.

## Core Note Management

Release 1.0 must support:

- Create a note.
- Edit a note.
- Rename a note.
- Duplicate a note.
- Move a note between notebooks.
- Pin and unpin a note.
- Delete a note into Trash.
- Restore a note from Trash.
- Permanently delete a note after confirmation.
- Create, rename, reorder, and delete notebooks.
- Support nested notebooks.
- Add and remove tags.
- Show all notes associated with a tag.
- Sort by:
  - Date modified.
  - Date created.
  - Title.
- Choose ascending or descending order.
- Filter by notebook, tag, pinned status, or trash status.
- Show a short note preview in the note list.
- Show created and modified timestamps in note information.
- Preserve the current note and scroll position between launches.

Include these smart views:

- All Notes.
- Inbox.
- Pinned.
- Recently Edited.
- Trash.

Deleting a non-empty notebook must require confirmation and must clearly explain what will happen to its notes.

## Editor

Use a native Markdown editor based on GtkSourceView.

The editor must provide:

- Markdown syntax highlighting.
- Line wrapping.
- Adjustable text size.
- Autosave.
- A visible status showing:
  - Saving.
  - Saved.
  - Save failed.
- Undo and redo.
- Find within the current note.
- Replace within the current note.
- Word count.
- Character count.
- Optional line numbers.
- Optional current-line highlighting.
- Native text selection and clipboard behavior.
- Drag-and-drop of local files and images.
- Paste support for local images from the clipboard.

Provide formatting actions for:

- Heading levels.
- Bold.
- Italic.
- Strikethrough.
- Inline code.
- Code block.
- Block quote.
- Bulleted list.
- Numbered list.
- Task list.
- Link.
- Horizontal rule.

Formatting actions must wrap selected text when appropriate and insert sensible placeholders when no text is selected.

Support Markdown task lists:

```markdown
- [ ] Not completed
- [x] Completed

```

Allow task boxes to be toggled from the rendered reading view.

## Reading and Preview Mode

Provide:

- Editing mode.
- Reading mode.
- Optional split editor and preview mode.

Render Markdown using native GTK widgets or a native text buffer. Do not use WebKit or load HTML into an embedded browser.

Support rendering of:

- Headings.
- Paragraphs.
- Emphasis.
- Strong text.
- Lists.
- Task lists.
- Block quotes.
- Code and fenced code blocks.
- Tables.
- Horizontal rules.
- Local links.
- Local images.
- Local attachments.

Never automatically fetch remote images, website previews, favicons, Open Graph metadata, or external resources.

Remote URLs may be opened in the user's normal browser only after the user explicitly clicks them.

## Search

Implement fast full-text search across:

- Titles.
- Note bodies.
- Tags.
- Notebook names.

Search requirements:

- Results should update as the user types.
- Highlight matching terms in the note list.
- Allow quoted exact phrases.
- Support filtering search by notebook and tag.
- Search should not be case-sensitive by default.
- Reindex notes after an external file change.
- Search indexing must remain completely local.

A disposable SQLite FTS5 index is acceptable. Markdown files must remain the authoritative data source.

If the search database is missing or corrupted, rebuild it automatically.

## Attachments

Support attaching local:

- Images.
- PDFs.
- Text files.
- Documents.
- Audio files.
- Other ordinary files.

When adding an attachment:

- Copy it into the current vault.
- Store it under the note's attachment directory.
- Sanitize the filename.
- Prevent path traversal.
- Avoid overwriting an existing file.
- Insert a relative Markdown link into the note.
- Never link to a temporary file that might disappear.
- Never upload or analyze the attachment remotely.

Images should display in reading mode.

Other attachments should show as clickable local-file links.

Provide a note information screen listing all attachments belonging to the note.

## Autosave and Data-Loss Protection

Autosave is mandatory.

Implement:

- Debounced autosave after approximately 500–1000 milliseconds of inactivity.
- Immediate save when changing notes.
- Save on focus loss.
- Save before normal application exit.
- Clear error reporting if a save fails.
- No silent data loss.

Every save must be atomic:

1. Write to a temporary file in the same filesystem.
2. Flush the contents.
3. Rename the temporary file over the original.
4. Preserve the previous file if the operation fails.

Do not overwrite a note when the on-disk version changed externally without warning.

When an external conflict occurs:

- Show both modification times.
- Allow keeping the editor version.
- Allow reloading the disk version.
- Allow saving the editor version as a separate note.
- Never silently discard either version.

Implement lightweight crash recovery. Unsaved recovery content must remain local and should be deleted after a successful normal save.

## Local History

Maintain optional local note history under:

```text
.senatorial-notes/history/

```

Requirements:

- History is enabled by default.
- Create a history entry only when content meaningfully changes.
- Do not create a new history file for every keystroke.
- Retain a sensible configurable number of versions.
- Provide a history browser.
- Allow previewing and restoring an earlier version.
- Restoring a version must first preserve the current version.
- Allow history to be disabled.
- Clearly show how much disk space history uses.

Do not market this as cloud backup. It is only local version history.

## Import and Export

Support importing:

- Individual `.md` files.
- Individual `.txt` files.
- A directory of Markdown files.
- A ZIP archive previously exported by SenatorialNotes.

Support exporting:

- The current note as Markdown.
- The current note as plain text.
- The current note as HTML generated locally.
- The current note through the system print dialog.
- The entire vault as a ZIP archive.

Export must never require an account or remote service.

Preserve attachments when exporting a complete vault.

## File-System Integration

Detect changes made by other programs.

The app must handle:

- Notes created externally.
- Notes edited externally.
- Notes renamed externally.
- Notes moved between notebook directories.
- Notes deleted externally.
- Newly added attachments.

Use a filesystem watcher, but also perform a verification scan at startup so missed watcher events do not corrupt the application state.

Do not assume the vault is permanently available. Handle disconnected drives and temporarily unavailable paths gracefully.

Use a vault lock to prevent two writable application instances from editing the same vault simultaneously.

When a second instance opens the same vault, offer:

- Open read-only.
- Return to the existing window.
- Cancel.

Do not blindly override a stale lock without warning.

## Privacy Requirements

These are non-negotiable.

The application must have:

- No account system.
- No login screen.
- No cloud sync.
- No telemetry.
- No analytics.
- No crash-report uploads.
- No geolocation.
- No update checks.
- No advertising.
- No tracking identifiers.
- No remote configuration.
- No plugin repository.
- No network-based spellchecker.
- No remote fonts.
- No remote image fetching.
- No website metadata fetching.
- No AI integration.
- No background networking.
- No hidden network fallback.
- No “anonymous usage statistics.”

Spellchecking may use an already installed system dictionary. It must not download dictionaries itself.

The application should work when launched inside a network-disabled namespace.

Do not request network permission in the Flatpak manifest.

Add a simple Privacy page inside About or Preferences that states:

> SenatorialNotes does not contain networking functionality. It does not transmit notes, metadata, diagnostics, location, or usage information. All application data remains on the user's device unless the user manually copies or exports it.

## Security Requirements

Do not invent custom cryptography.

Release 1.0 stores ordinary notes as Markdown. It may also store individually encrypted notes in the documented `.snote` format. Clearly document that ordinary Markdown is plaintext and recommend full-disk or home-directory encryption for broader protection.

Do not add a fake password screen that claims to secure unencrypted files.

For files and directories created by the app:

- Prefer `0700` for newly created private vault directories.
- Prefer `0600` for newly created note and metadata files.
- Respect existing permissions when opening an existing vault.
- Never recursively change permissions without explicit user approval.
- Use safe temporary files.
- Sanitize filenames.
- Prevent `../` path traversal.
- Handle symbolic links carefully.
- Do not follow a malicious attachment path outside the vault.
- Do not execute attachments.
- Do not render active scripts.
- Do not store note contents in debug logs.
- Do not include note contents in crash output.
- Avoid leaving note text in world-readable temporary directories.

Provide a `SECURITY.md` explaining:

- The local threat model.
- What the app protects against.
- What it does not protect against.
- How to report vulnerabilities.
- Why disk encryption is recommended.

Individual encrypted notes must use Argon2id plus XChaCha20-Poly1305 from established Rust cryptography crates, fresh random salt and nonce values, authenticated versioned containers, neutral filenames, no stored passwords, and no recovery/backdoor. The security model and format must remain documented and reviewable.

## Preferences

Provide a clean Preferences window with:

### Appearance

- Follow system theme.
- Force light theme.
- Force dark theme.
- Editor font.
- Editor font size.
- Line width.
- Line numbers.
- Current-line highlighting.
- Note-list preview length.

### Editing

- Autosave delay.
- Default new-note notebook.
- Default sort order.
- Spellcheck using only installed system dictionaries.
- Markdown formatting preferences.
- Create new notes with or without a title heading.

### Files

- Current vault location.
- Open vault folder in file manager.
- Local-history retention.
- Local-history disk usage.
- Rebuild search index.
- Export full vault.
- Open cache directory.
- Clear disposable cache.

### Privacy

Display the offline/privacy statement.

Do not include account, sync, analytics, update, or cloud settings.

## Keyboard Shortcuts

At minimum, support:

```text
Ctrl+N          New note
Ctrl+Shift+N    New notebook
Ctrl+S          Save immediately
Ctrl+F          Find in current note
Ctrl+Shift+F    Search all notes
Ctrl+B          Bold
Ctrl+I          Italic
Ctrl+K          Insert link
Ctrl+Z          Undo
Ctrl+Shift+Z    Redo
Ctrl+,          Preferences
Ctrl+Q          Quit
Delete          Move selected note to Trash
F2              Rename selected note or notebook
Escape          Close search/dialog or return to previous pane

```

Include a keyboard-shortcuts window accessible from the application menu.

Do not override ordinary text-editing shortcuts unexpectedly.

## Accessibility and Usability

Ensure:

- Every icon-only button has an accessible label and tooltip.
- All primary actions are reachable by keyboard.
- The interface works at 200% scaling.
- Long note titles do not break the layout.
- Empty states explain what the user can do.
- Destructive actions use clear language.
- Permanent deletion requires confirmation.
- Ordinary deletion goes to Trash without an intrusive confirmation every time.
- Error messages explain how to recover.
- The application does not freeze while indexing a large vault.
- Expensive work runs outside the main UI thread.
- Search and filesystem events do not cause noticeable typing lag.

## Performance Targets

Design for at least:

- 10,000 Markdown notes.
- Large notes of several megabytes.
- Hundreds of attachments.
- Deeply nested notebook folders.

Targets:

- Normal startup with 1,000 indexed notes should feel nearly immediate.
- Typing must remain responsive.
- Autosave must not block the interface.
- Search should update quickly after the initial index is built.
- The application should not load every attachment into memory.

Do not sacrifice correctness or data safety merely to meet a benchmark.

## Repository Contents

Create a clean repository containing:

```text
README.md
LICENSE
SECURITY.md
CONTRIBUTING.md
CHANGELOG.md
CODE_OF_CONDUCT.md
Cargo.toml
Cargo.lock
src/
tests/
data/
packaging/
  arch/
  flatpak/
.github/
  workflows/
  ISSUE_TEMPLATE/
  pull_request_template.md

```

Use an open-source license. Use **GPL-3.0-or-later** unless instructed otherwise.

The README must contain:

- A clear project description.
- Screenshots section with placeholders until screenshots exist.
- Feature list.
- Privacy statement.
- Storage-format explanation.
- Build instructions for Arch Linux.
- Runtime dependency list.
- Development instructions.
- Packaging instructions.
- Known limitations.
- Security explanation.
- Contribution instructions.
- Roadmap.

Do not claim that unfinished features already work.

## Linux Desktop Integration

Provide:

- A `.desktop` file.
- AppStream metadata.
- A scalable SVG application icon.
- Standard symbolic icons where appropriate.
- Correct application categories.
- Correct MIME associations for Markdown only if opening files is implemented safely.
- Proper single-instance behavior.
- Native file pickers.
- Native notifications only for meaningful local events.

Do not create startup services, background daemons, system-tray processes, or autostart entries.

## Arch Linux Packaging

Create a working `PKGBUILD` under:

```text
packaging/arch/

```

It should:

- Build from source.
- List all required dependencies accurately.
- Install the binary.
- Install the `.desktop` file.
- Install icons.
- Install AppStream metadata.
- Install the license.
- Avoid downloading dependencies during the package phase where Arch packaging conventions prohibit it.

Also provide clear local build instructions.

## Flatpak Packaging

Create a Flatpak manifest under:

```text
packaging/flatpak/

```

The manifest must:

- Not request network permission at runtime.
- Only request access necessary to folders explicitly selected by the user.
- Avoid broad home-directory access where a document portal can be used.
- Not run a background service.
- Not include update, telemetry, or cloud components.

Document how to build and run the Flatpak locally.

## Code Quality

Use:

- Clear Rust modules.
- Strong error handling.
- No unnecessary `unwrap()` calls in production code.
- No silent error suppression.
- Helpful but privacy-safe logs.
- Rust formatting.
- Clippy with warnings treated seriously.
- Comments for non-obvious data-safety logic.
- Small, testable storage functions.
- Explicit migrations for future metadata-format changes.

Run and fix:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test

```

Do not mark work complete while these commands fail.

## Tests

Add unit and integration tests for:

- Front-matter parsing.
- Unknown metadata preservation.
- UUID handling.
- Filename sanitization.
- Path-traversal rejection.
- Notebook creation.
- Notebook rename.
- Note creation.
- Note rename.
- Atomic save.
- Save failure recovery.
- Trash and restore.
- Permanent deletion.
- Attachment filename collisions.
- External modification detection.
- Search indexing.
- Search database rebuilding.
- History creation and restore.
- ZIP export and import.
- Configuration parsing.
- Metadata migrations.
- Vault-lock behavior.

Tests must use temporary directories and must not modify real user data.

Add at least one automated check confirming that runtime code does not depend on the intended HTTP-client crates.

## Continuous Integration

Prepare GitHub Actions workflows for future use, but do not require GitHub authentication now.

CI should run on Linux and check:

- Formatting.
- Clippy.
- Unit tests.
- Integration tests.
- Release build.
- Packaging metadata where practical.

Do not automatically publish releases.

## Local Git Setup

Initialize a local Git repository if one does not already exist.

Create a sensible `.gitignore` covering:

- Rust build output.
- Editor files.
- Temporary files.
- Test vaults.
- Generated packaging output.
- Local development settings.

Do not:

- Create a GitHub repository.
- Add a remote.
- Push anything.
- Request GitHub credentials.
- Include secrets.
- Commit real notes or test notes containing personal information.

If Git identity is not configured, do not fail the project over it. Prepare the repository and provide the commands the user may run later.

## Mandatory Real-World Corrections

The following requirements were added after testing Phase 1 on Arch Linux with Hyprland and take precedence over earlier phase boundaries:

- Title text remains an in-memory draft while typing. Body autosave must not read or commit that draft, rebuild the editor, change focus/selection, or move the cursor. A title commits on Enter, title focus loss, note change, exit, or an approximately 1.5-second debounce. Only that commit may safely rename a Markdown file to `<sanitized-title>--<short-uuid>.md`; the UUID remains stable and collisions are never overwritten.
- Ordinary deletion moves notes to Trash. Toolbar, context-menu, and note-list `Delete` actions must be available. Trash provides restore to the original notebook where possible, confirmed permanent deletion, and confirmed Empty Trash.
- Follow System, Light, and Dark modes apply immediately to every native surface, including a matching GtkSourceView style scheme, and persist across launches.
- The interface must have a restrained SenatorialNotes identity through pane proportions, spacing, typography, note cards, selection, hierarchy, title treatment, empty states, search, toolbar, and editor margins without copying Apple assets.
- Appearance Preferences provide theme, locally installed editor font, font size, practical line spacing/reading width, line numbers, note-list density, preview length, and a small curated accent set. Fonts are never downloaded.
- A native formatting toolbar/popover provides normal text, three heading levels, bold, italic, strikethrough, highlight, inline code, code block, quote, bullets, numbering, checklist, link, and divider actions while retaining Markdown as the normal storage format.
- Individual encrypted notes use neutral `.snote` filenames and the documented versioned container. Argon2id derives a key from a non-stored password; XChaCha20-Poly1305 encrypts and authenticates title, body, tags, and private metadata with fresh random salt/nonce material. No recovery mechanism, backdoor, master key, or persistent plaintext search/recovery copy is allowed.
- Encrypted-note UI provides unlock, Lock Now, automatic locking preferences, password change with new salt/nonce, and warned conversion back to plaintext Markdown. Locked notes reveal no title, preview, tags, or searchable contents.
- Automated tests must read encrypted files directly and prove secret strings are absent, correct passwords work, wrong passwords fail, tampering fails authentication, plaintext temporary/recovery/index data is absent, and title, Trash, and theme regressions remain covered.

The encrypted container and its actual security boundaries are documented in `docs/ENCRYPTED_NOTE_FORMAT.md`. Full-disk encryption remains recommended for ordinary notes, swap, caches, and broader system data.

## Implementation Order

Implement the project in these phases:

### Phase 1: Foundation

- Project structure.
- GTK/libadwaita application window.
- Vault creation and opening.
- Markdown note model.
- Atomic local storage.
- Basic notebook and note list.
- Basic editor.
- Autosave.
- Unit tests.

### Phase 2: Complete Note Management

- Trash and restore.
- Tags.
- Pinned notes.
- Sorting.
- Search.
- Filesystem watching.
- Conflict handling.
- Preferences.

### Phase 3: Editing and Attachments

- Formatting toolbar.
- Markdown reading mode.
- Local images.
- File attachments.
- Drag and drop.
- Import and export.
- Local history.

### Phase 4: Hardening

- Accessibility.
- Performance improvements.
- Recovery testing.
- Path-safety testing.
- Vault locking.
- Packaging.
- AppStream metadata.
- Privacy and security documentation.

### Phase 5: Release Preparation

- Complete README.
- Screenshots.
- Changelog.
- Final test pass.
- Arch package.
- Flatpak manifest.
- Version `0.1.0`.

After each phase:

1. Compile the application.
2. Run relevant tests.
3. Fix errors before continuing.
4. Summarize what was implemented.
5. List any known limitation honestly.

Do not replace unfinished functionality with fake buttons.

## Release 1.0 Acceptance Checklist

The project is not ready until all of the following are true:

- The app launches natively on Arch Linux.
- No account is requested.
- A user can create or open a local vault.
- A user can create, edit, rename, move, pin, tag, delete, restore, and permanently delete notes.
- Notes remain ordinary Markdown files.
- The app survives restart without losing data.
- Autosave works.
- Atomic saving is tested.
- Search works across titles and bodies.
- External file changes are detected.
- Attachments remain inside the vault.
- Reading mode does not use a browser engine.
- The app makes no runtime network requests.
- Blocking all network access does not break ordinary operation.
- No telemetry or analytics exists.
- No note text appears in logs.
- Trash and local history work.
- Import and export work.
- Dark and light modes work.
- Keyboard shortcuts work.
- The UI remains responsive during indexing.
- Tests pass.
- Clippy passes.
- Formatting passes.
- Arch packaging files exist.
- Flatpak packaging does not grant network permission.
- README, LICENSE, SECURITY, and CONTRIBUTING files exist.
- The project contains no credentials or personal notes.
- Nothing has been pushed to GitHub without explicit permission.

## Explicit Non-Goals for Version 1.0

Do not add these to version 1.0:

- User accounts.
- Cloud synchronization.
- Mobile applications.
- A web application.
- Collaboration.
- Shared notes.
- AI features.
- Online services.
- Plugin repositories.
- Browser extensions.
- Email integration.
- Calendar integration.
- Homemade or proprietary cryptographic primitives.
- Password-based fake locking.
- Automatic online updates.
- Remote themes or icon packs.

Focus on making the local desktop application stable, attractive, safe, and reliable before expanding its scope.

Begin by inspecting the current project directory, creating the project architecture, and implementing Phase 1. Do not stop after writing a plan.
