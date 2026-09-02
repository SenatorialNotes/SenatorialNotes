# Security Policy

## Local threat model

SenatorialNotes is designed to keep notes off networks and out of proprietary storage. Ordinary Markdown and versioned encrypted-note containers are authoritative, runtime networking is absent, paths are validated, newly created private data defaults to restrictive permissions, saves use a temporary sibling plus atomic rename, and note contents are not intentionally logged.

The application is intended to reduce risks from:

- accidental upload, telemetry, analytics, or remote resource loading;
- data loss during an interrupted save;
- simple `../` attachment or notebook path traversal;
- silent overwrite after an external edit;
- an untrusted symbolic link leading a managed note path outside the vault.

## What it does not protect against

Ordinary `.md` vault files are plaintext. Individually encrypted `.snote` files protect their title, body, tags, and private metadata at rest, but SenatorialNotes does not protect an unlocked note from:

- another process running as the same user;
- an administrator or attacker with root access;
- an unlocked or compromised desktop session;
- malware, keyloggers, screen capture, swap, hibernation images, or forensic access to an unencrypted disk;
- deliberate copies, exports, backups, or synchronization performed outside the application;
- physical access when the operating system and storage are not encrypted.

Use full-disk or encrypted home-directory storage for sensitive notes. Back up important vaults separately; local history is not a backup service.

## File handling

New private vault directories are created with mode `0700` and new note/metadata files with mode `0600` on Unix. Existing permissions are respected. SenatorialNotes does not recursively rewrite permissions. Managed vault paths reject traversal and symbolic-link components; attachments are never executed by the storage layer.

## Reporting a vulnerability

The future project home is `https://github.com/SenatorialNotes/SenatorialNotes`. Until a private reporting channel is published there, do not place exploit details or private note data in a public issue. Once GitHub is configured, this file will be updated with the repository's private security-advisory process.

Include the affected version, operating system, minimal reproduction steps, and impact. Never include a real vault or personal note content.

## Encrypted notes

SenatorialNotes does not invent cryptographic primitives. Version 1 encrypted-note containers use Argon2id (64 MiB, three iterations, one lane, 32-byte output) and XChaCha20-Poly1305 with a fresh random 16-byte salt and 24-byte nonce. Header bytes are authenticated as associated data. The implementation uses established RustCrypto crates and a documented, reviewable [container format](docs/ENCRYPTED_NOTE_FORMAT.md).

Passwords, reversible passwords, plaintext keys, recovery questions, master recovery keys, and backdoors are never stored. Derived keys exist only in process memory while a note is unlocked and are zeroized when their session object is dropped. Sensitive serialization buffers are also zeroized. Rust/GTK strings and operating-system memory can still leave transient copies; locking is not a defense against a compromised running computer.

There is no password recovery. Security depends on password strength, correct cryptographic implementation, and the integrity of the computer while the note is unlocked. Full-disk encryption is still recommended because it additionally protects ordinary notes, swap, hibernation, caches, filenames outside encrypted containers, and other application data.

## Locked encrypted notes and organisational views

Pinned state, Archive state, and tags for an encrypted note live inside its authenticated ciphertext, exactly like its title and body. SenatorialNotes never stores a plaintext copy of these fields to make organisational views work while a note is locked, and it never guesses.

Practically, this means a locked encrypted note's pinned/archived state reads as "not set" until it is unlocked: it stays visible in All Notes and its real notebook (the file's location on disk is not protected metadata), but it never appears in the Pinned, Archive, or Recently Edited smart views - even if it truly is pinned, archived, or recently edited - because SenatorialNotes cannot truthfully make that claim without the password. Unlocking the note restores its correct place in every view immediately; locking it again reverts to the same non-committal state, with no residual value left in memory.

While a note is unlocked, its real title, preview, and tags are shown in the sidebar the same as any other open note - this is read from the same in-memory, never-persisted `state.notes` list every other summary already lives in, not a new disk-backed cache. Locking the note (manually, on a timer, or at restart) immediately discards that in-memory copy and reverts the row to the anonymous locked placeholder below; nothing decrypted is ever written to a file, index, or cache.

Since a vault can hold more than one locked note, each locked note's sidebar row carries a short label (e.g. "Locked Note · A1B2C3D4") instead of a bare "Locked Note". The suffix is deterministic and derived only from the note's own UUID - the same identifier already exposed as its on-disk filename - never from its title, body, tags, or any other protected field, so it distinguishes locked notes from one another without revealing anything the ciphertext protects.

## Moving an encrypted note between notebooks

A notebook is a directory on disk, and a note's notebook membership is simply the file's location - it is not part of the encrypted payload. Moving a `.snote` file between notebooks is therefore an ordinary filesystem rename: it does not re-encrypt the container, does not ask for the password, and does not change the ciphertext or its authentication. This is safe because the file path was never authenticated as associated data (only the fixed 88-byte header is). See [the encrypted-note format](docs/ENCRYPTED_NOTE_FORMAT.md).

## The default notebook never requires a password

The built-in default notebook (shown as "Unfiled" in the interface; its on-disk directory is named `Inbox`) is an ordinary, unencrypted notebook - it is only special in that notes fall back to it when no other notebook is selected. Browsing to it, or to any notebook or smart view, never requires a password by itself. A password dialog only appears when the user directly acts on one specific locked note - clicking its row, pressing next/previous with it as the target, or pressing its "Unlock Note" button. Automatic fallback selection (switching to a view and landing on whatever note sorts first, or on an adjacent note after one is removed) may select a locked note and show its locked placeholder, but never launches the password dialog on its own.

## Secure Vaults (whole-vault encryption)

A vault can be created as a **Secure Vault** — an *encrypted vault*
(`format_version = 3`); an ordinary vault is a **Standard Vault**. Its notes, titles, tags, notebook names and tree, attachments and their filenames, trashed notes, and recovery drafts are all stored under `.senatorial-notes/store/` as opaque authenticated-encryption blobs with random names. The full container design, key hierarchy, HKDF labels, and failure behaviour are in [`docs/ENCRYPTED_VAULT_FORMAT.md`](docs/ENCRYPTED_VAULT_FORMAT.md).

**Key hierarchy.** The vault password is stretched with Argon2id (64 MiB, three iterations, one lane) into a key-encryption key (KEK). The KEK only ever unwraps one random 32-byte vault master key (VMK) stored wrapped in `vault.keys`. Domain subkeys (note content, manifest/names, attachments, and two reserved slots) are derived from the VMK with HKDF-SHA256 using fixed, format-stable labels. `SHA-256(password)` is never used; per-object keys are never derived from the password. Changing the vault password re-wraps the VMK only - **no note is re-encrypted** - and the old password stops working (verified after the write).

**What it protects, at rest, while locked:** every note body/title/tag, notebook names and structure, attachment contents and names, trash, and recovery content. Ciphertext is authenticated (XChaCha20-Poly1305); a wrong password, a tampered `vault.keys`, or a single flipped byte in any blob fails safely - the vault or that note reads as unavailable, and unauthenticated plaintext is never returned. A blob moved from another vault, a swapped blob, or a blob passed off as the wrong object type all fail authentication, because the vault UUID, object UUID, and object type are bound as associated data. A note's notebook is *not* bound, so moving or renaming a note never re-encrypts it.

**What is still visible on disk** even for a locked encrypted vault: the number of blob files and each blob's size and modification time, the total vault size, the `created_at` timestamp and `vault_id` in the plaintext `vault.toml`, and the fact that the folder is a locked SenatorialNotes vault. Blob-size padding is future work.

**Lock lifecycle.** An encrypted vault opens locked and shows an unlock screen; nothing is decrypted until the correct password is entered. Argon2id runs on a worker thread, never on the UI thread. A failed unlock changes nothing on disk and leaves the vault locked; repeated failures cannot corrupt it. "Lock Now", the idle timer, and losing window focus all drop the in-memory keys (zeroized on drop), clear the decrypted note list, search state, and editor buffer, and return to the lock screen. Search works only while the vault is unlocked and is never written to a persistent index. As with encrypted notes, locking is not a defence against a compromised running computer, and memory zeroization is best-effort against swap or a privileged memory scraper.

A per-note `.snote` inside an encrypted vault is an additional inner layer with its own password; unlocking the vault does not unlock the note, and the outer layer always peels back to byte-identical `.snote` data.
