# Contributing to SenatorialNotes

Thank you for helping build a reliable local notes application.

## Before starting

Discuss large UI, storage-format, security, or dependency changes before implementing them. Preserve these invariants:

- Markdown files remain authoritative and human-readable.
- Runtime networking, telemetry, remote assets, AI services, and browser engines remain absent.
- Note bodies and titles never enter logs.
- Existing unknown front-matter fields survive a normal save.
- Storage changes include tests for success and failure paths.

## Development setup

On Arch Linux:

```bash
sudo pacman -S --needed base-devel rust gtk4 libadwaita gtksourceview5
cargo build
```

Run the full checks:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo build --release
```

Keep commits focused. Do not commit generated vaults, personal notes, credentials, build output, or editor-local configuration.

## Testing storage code

Use `tempfile` for every test that writes data. Test both the intended behavior and the recovery behavior. Never use `~/.config/senatorial-notes/`, `~/.cache/senatorial-notes/`, or a real vault in a test.

## Dependencies

Explain why a new dependency is necessary. Runtime networking crates and frameworks that embed a browser are out of scope. Dependencies must have a compatible license and an active maintenance story.

## Documentation

Describe only behavior that exists. Put planned work under the roadmap or known limitations rather than presenting it as an available feature.

All contributions are licensed under GPL-3.0-or-later and must follow [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).
