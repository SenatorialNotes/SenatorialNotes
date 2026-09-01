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
