# Arch package

`PKGBUILD` is prepared for the future `v0.1.0` source tag at `SenatorialNotes/SenatorialNotes`. Because no remote or tag exists yet, it cannot download that source today.

After the repository and tag exist, update `sha256sums` with the release archive's real checksum, then build in a clean Arch environment:

```bash
makepkg --cleanbuild --syncdeps
```

Cargo dependencies are fetched during `prepare()` and the package is built with `--frozen`. The package phase itself performs no downloads.

For local development before a tag exists, build from the repository root with `cargo build --release`; do not pretend the unreleased source archive is available.
