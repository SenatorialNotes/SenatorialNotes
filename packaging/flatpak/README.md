# Flatpak package

The local manifest deliberately has no `--share=network`, background service, or broad home-directory permission. The GTK folder picker uses the document portal to grant access to folders the user explicitly selects.

The manifest is release scaffolding. `Cargo.lock` is committed, but a reproducible offline Cargo source list must still be generated and added before calling the Flatpak release-ready. Once that exists, install the GNOME SDK and Rust extension matching runtime 50, then build from this directory:

```bash
flatpak install --user flathub org.gnome.Platform//50 org.gnome.Sdk//50 org.freedesktop.Sdk.Extension.rust-stable//25.08
flatpak-builder --user --install --force-clean build-dir io.github.SenatorialNotes.SenatorialNotes.yml
flatpak run io.github.SenatorialNotes.SenatorialNotes
```

Do not add network permission. Build-time dependency sources should be vendored or declared as Flatpak sources; the installed application does not need network access.
