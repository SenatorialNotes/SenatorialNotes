# Arch package

`PKGBUILD` is prepared for the future `v0.2.0-alpha` source tag at `SenatorialNotes/SenatorialNotes`. Because no remote or tag exists yet, it cannot download that source today. (`pkgver` is `0.2.0_alpha` because Arch package versions cannot contain a hyphen; `$_tag` and `$_srcver` carry the real tag and archive directory name.)

After the repository and tag exist, update `sha256sums` with the release archive's real checksum, then build in a clean Arch environment:

```bash
makepkg --cleanbuild --syncdeps
```

Cargo dependencies are fetched during `prepare()` and the package is built with `--frozen`. The package phase itself performs no downloads.

For local development before a tag exists, build from the repository root with `cargo build --release`; do not pretend the unreleased source archive is available.
