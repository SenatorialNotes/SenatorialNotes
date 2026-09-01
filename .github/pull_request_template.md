## Summary

Describe the user-visible change and why it belongs in SenatorialNotes.

## Verification

- [ ] `cargo fmt --check`
- [ ] `cargo clippy --all-targets --all-features -- -D warnings`
- [ ] `cargo test --all-features`
- [ ] `cargo build --release --all-features`
- [ ] Tests use temporary directories and contain no personal note data.
- [ ] Documentation describes only implemented behavior.
- [ ] No runtime networking, telemetry, remote assets, or note-content logging was added.

## Storage and privacy impact

Explain any format, permission, recovery, dependency, or privacy implications. Write “None” only after checking.
