# SenatorialNotes stability acceptance test

Run this pass on an Arch Linux desktop with GTK4, libadwaita, and GtkSourceView 5 installed. Start from a disposable vault so the test cannot affect personal notes.

## Automated gate

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo build --release
```

## Interactive gate

1. Open a disposable vault at approximately 900 × 650.
2. Click **New Note** 100 times. Confirm that the title field remains focused and the application remains responsive.
3. Alternate rapidly between the first and second notes with the pointer, then with keyboard navigation.
4. Edit a title and body while watching the cursor, selection, focus, and scroll position through several autosaves.
5. Switch repeatedly among **All Notes**, **Unfiled**, and **Trash**.
6. Right-click several different notes without first selecting them. Close and reopen each menu.
7. From context menus, rename, pin, unpin, and move both selected and non-selected notes to Trash.
8. In Trash, restore selected and non-selected notes, then permanently delete a disposable note after confirming the dialog.
9. Resize through 1200 × 750, 900 × 650, and below 760 px. Verify that Library becomes an overlay, Notes and Editor become a single navigation flow at the narrow breakpoint, and no toolbar action is clipped.
10. Repeat steps 2–8 with `RUST_BACKTRACE=1` and confirm that the terminal contains no panic or `RefCell already borrowed` message.

The build is accepted only after this interactive pass succeeds on the target Arch/Hyprland system.

## Secure Vault interactive gate (v0.3 Stage E)

Run against `RUST_BACKTRACE=full target/release/senatorial-notes` on the target Arch/Hyprland system. Use disposable folders under `~/Documents`.

### A. Secure Vault basics

1. **New Secure Vault** from the sidebar. Set a password. Confirm the workspace opens unlocked, the header shows the padlock and **Lock Vault**.
2. Create several notes and notebooks. Lock the vault (header button), then quit and relaunch. Confirm it opens on the **lock screen** with the shell visible and no note content leaked.
3. Unlock with the wrong password — inline error, still locked. Unlock with the right password — the workspace restores.
4. **Change Vault Password…** (Secure Vault Settings). Relock, confirm the old password fails and the new one works.
5. Auto-lock: set "Lock after inactivity" to 1 minute, leave the app idle, confirm it locks. Toggle focus-loss / minimize locking and confirm.
6. Encrypt one note (per-note `.snote`), lock and unlock the vault, confirm the note lists as **Locked Note · …** and opens with its own password.

### B. R18 plaintext quarantine

7. Quit the app. In a Secure Vault's folder, create `Notes/Inbox/` and drop a plaintext `test.md` into it (simulating an old binary). Relaunch and open that vault.
8. Confirm the dialog **"Plaintext files found in …"** appears with **Cancel / Open Read-Only / Quarantine Plaintext Files…**.
9. Choose **Open Read-Only** — confirm the vault opens read-only, editing is disabled, and `Notes/Inbox/test.md` is **still there, untouched**.
10. Reopen the vault, this time choose **Quarantine Plaintext Files…**. Confirm: the status line reports the move, `Notes/` is gone from the vault root, `.senatorial-notes/quarantine/<timestamp>/Notes/Inbox/test.md` exists byte-identical, and the vault is now open **writable**.

### C. Secure → Standard export

11. In an unlocked Secure Vault with several notes (plus one `.snote` and one trashed note), open **Secure Vault Settings → Export to Standard Vault…** (or the app-menu item).
12. Step through: explainer → **Confirm Vault Password** → choose an **empty** folder → confirm the "unencrypted copy" warning. Confirm a progress dialog appears and does **not** freeze the window (the Cancel button stays responsive).
13. On completion, choose **Open Exported Vault**. Confirm it opens as a **Standard Vault**, every note is present as plaintext Markdown, notebooks match, the trashed note is in **Trash** and restores cleanly, and the `.snote` still opens with its original password.
14. Confirm the original Secure Vault is unchanged (same notes, still encrypted, still locks/unlocks).
15. Try exporting again into a **non-empty** folder and into the Secure Vault's own folder — both must be refused with a clear message, writing nothing.

Zero panics / `already borrowed` / GTK criticals across the whole pass.
