# SenatorialNotes Encrypted Vault Format

Status: **implemented in Stage D** of [`docs/ROADMAP_v0.3.md`](ROADMAP_v0.3.md).
This document describes the format as built (`src/crypto/vault.rs`,
`src/vault_encrypted.rs`). The primitives, the key hierarchy, the AAD field
set, and the HKDF labels are format-stable.

This document specifies *whole-vault* encryption. It is a container design
around primitives the project already uses — Argon2id and XChaCha20-Poly1305 —
plus HKDF-SHA256 for key-schedule domain separation. It invents no
cryptographic construction. The per-note `.snote` format
([`docs/ENCRYPTED_NOTE_FORMAT.md`](ENCRYPTED_NOTE_FORMAT.md)) is **unchanged**
by this work.

**Product vs. technical terms.** The UI calls an encrypted vault a **"Secure
Vault"** and an ordinary vault a **"Standard Vault"**; an individually
encrypted `.snote` is an **"Encrypted Note"**. This document and the source
comments use the precise terms ("whole-vault encryption", "encrypted vault",
"per-note encryption"). The on-disk format is unaffected: `kind = "encrypted"`,
`format_version = 3`, `SNENC`, `SNVLT`, `Backend::Encrypted` are unchanged.

## 1. Security model

An encrypted vault protects, **at rest, while locked**:

- every note body, title, tag set, and private metadata field;
- notebook names and the notebook tree structure;
- attachments and their filenames;
- `recovery/` content and (when it lands) `history/`;
- any future local search index;
- the note count per notebook and the mapping between on-disk files and logical
  note names (on-disk blob names are random and opaque).

Unlocking derives the vault key from the user's **single vault password**. There
is deliberately **no password recovery, master key, backdoor, stored password,
or hint** — identical to `.snote`.

It does **not** protect:

- an unlocked vault on a running machine — malware, a keylogger, screen capture,
  swap, or hibernation can read plaintext while the vault is unlocked, exactly
  as for an open `.snote`;
- **metadata still visible on disk**: the total vault size, the number of blob
  files, each blob's individual size and mtime, the `created_at` timestamp (kept
  in the clear in `vault.toml`, see §4), the vault's `vault_id` (kept clear so a
  locked vault can be identified in the recent-vaults list), and the fact that
  the folder is a locked SenatorialNotes vault. Blob-size padding is future
  work, not v0.3.

Full-disk or encrypted-home storage remains recommended: it additionally covers
swap, hibernation images, the app config (`~/.config/senatorial-notes/`), and
everything outside the vault.

## 2. Primitives and parameters

Encryption/KDF primitives are identical to `.snote` (`src/crypto/note.rs`). One
standard KDF is added — **HKDF-SHA256** (RustCrypto `hkdf` + `sha2`) — for
domain separation; it is not a new *encryption* primitive.

| Purpose | Primitive | Parameters |
| --- | --- | --- |
| Password → key-encryption key (KEK) | Argon2id v1.3 | 65 536 KiB (64 MiB) memory, 3 iterations, 1 lane, 32-byte output |
| Wrapping the vault master key, and all object encryption | XChaCha20-Poly1305 | 24-byte random nonce per operation, 16-byte tag |
| Subkey derivation from the vault master key | HKDF-SHA256 (extract + expand) | salt = `vault_id` (16 bytes), `info` = a format-stable per-domain label (§3.1), 32-byte output per domain |
| Salt / nonces / vault master key / blob names | OS CSPRNG (`getrandom`) | KDF salt 16 bytes, nonce 24 bytes, VMK 32 bytes, blob name 16 bytes |

Argon2 parameters are stored in the keyfile header so a future format version
can raise them. The reader **rejects parameters outside the conservative bounds
enforced in `crypto::note::validate_kdf_parameters`** (memory 8 MiB … 1 GiB,
iterations 1 … 10, lanes 1 … 16) before any allocation or derivation.

## 3. Key hierarchy

Envelope encryption with an HKDF domain-separation layer. No key is ever written
unwrapped; only the **wrapped** vault master key touches disk.

```
                 vault password (never stored)
                          │
             Argon2id(password, kdf_salt, argon2 params)      ── in memory, Zeroizing
                          │
                          ▼
                        KEK  (32 bytes)
                          │
    XChaCha20-Poly1305 unwrap(KEK, wrap_nonce, aad = keyfile header[0..88])
                          │
                          ▼
            vault master key  (VMK, 32 random bytes)          ── in memory, Zeroizing
                          │
        HKDF-SHA256(ikm = VMK, salt = vault_id, info = <domain label>)
                          │
      ┌───────────┬───────────┬─────────────┬────────────┬──────────┐
      ▼           ▼           ▼             ▼            ▼          (reserved)
  k_content    k_names   k_attachments  k_metadata   k_index
  note/body    the vault  attachment    reserved     reserved
  blobs,       manifest,  blobs         (future      (future
  trashed +    inner-               per-vault    encrypted
  recovery     .snote               metadata     search
  blobs        index                blob)        index)
```

`k_names` currently seals the single vault manifest (§7) and is the key for
`ObjectType::Manifest`. `k_metadata` and `k_index` subkeys are derived and held
but not yet written to any blob; they exist so a later feature adds a label, not
a re-wrap.

Why a random VMK + HKDF rather than deriving each key straight from the
password, or `SHA-256(password)`:

- **`SHA-256(password)` is explicitly rejected** — no stretching, no salt. The
  KDF is Argon2id, always.
- **Per-object keys are never derived from the password.** Only the KEK is, and
  it only ever unwraps the VMK.
- **A password change re-wraps the VMK only** — a fresh `kdf_salt`, KEK, and
  `wrap_nonce`, one XChaCha20-Poly1305 wrap, one `atomic_write` of the keyfile.
  **Zero file blobs are re-encrypted** (they are keyed by HKDF subkeys of the
  VMK, which does not change).
- **Clean domain separation.** A bug or nonce mishap in the content path cannot
  forge a manifest, and vice versa — the keys are independent HKDF outputs.

Each subkey is used **directly** with a fresh 24-byte random nonce per write.
XChaCha20's 192-bit random nonce space makes reuse negligible far beyond any
realistic vault.

`KEK`, `VMK`, and every HKDF subkey live in `Zeroizing` buffers (`VaultKeys` in
`src/crypto/vault.rs`) and are cleared on lock or drop, mirroring
`crypto::EncryptedSession`.

### 3.1 HKDF domain-separation labels — FORMAT-STABLE

These `info` byte strings are part of the format. Changing any of them is a
container **format-version bump**, never a silent edit. Source-guard tests
(`crypto::vault::tests::hkdf_labels` and
`ui_source_invariants::hkdf_labels_match_the_format_document`) assert the
constants in `src/crypto/vault.rs` equal exactly these bytes and that this
document lists them.

| Subkey | `info` label (ASCII bytes) |
| --- | --- |
| `k_content` | `senatorialnotes/vault/v1/content` |
| `k_names` | `senatorialnotes/vault/v1/names` |
| `k_attachments` | `senatorialnotes/vault/v1/attachments` |
| `k_metadata` | `senatorialnotes/vault/v1/metadata` |
| `k_index` | `senatorialnotes/vault/v1/index` |

HKDF salt is the raw 16-byte `vault_id` (non-secret; adds inter-vault
separation). The `v1` segment tracks the *key-schedule* version and moves
independently of the `SNENC` container version.

## 4. On-disk layout

An encrypted vault is `format_version = 3` (its own version, not `2` + a `kind`
flag). Its plaintext `vault.toml` carries **only** what a reader needs to
identify and open it. Every byte of note / notebook / tag / attachment /
recovery content lives under `.senatorial-notes/store/` as opaque `SNENC`
blobs, described by one encrypted manifest.

```
My Encrypted Vault/
├── .senatorial-notes/
│   ├── vault.toml            ← CLEAR, minimum only:
│   │                             format_version = 3
│   │                             vault_id = "…"
│   │                             created_at = "…"   (coarse; see §1)
│   │                             kind = "encrypted"
│   │                             [encryption] { format = 1, keyfile = "vault.keys" }
│   ├── vault.keys            ← the keyfile (§5): wrapped VMK; useless without the password
│   ├── vault.lock            ← advisory lock (docs/VAULT_LOCK.md), clear
│   └── store/
│       ├── manifest          ← k_names (ObjectType::Manifest, fixed object UUID):
│       │                        the notebook tree, the note↔blob index, trash,
│       │                        recovery index, attachment index, manifest created_at
│       ├── 9f3a…  (16 hex)   ← k_content: one note blob (SNENC)
│       ├── 4b7c…             ← k_content: another note blob, or a trashed/recovery blob
│       ├── e1d0…             ← k_attachments: an attachment blob
│       └── orphans/          ← reconciliation quarantine (§7); blobs are moved here,
│                                never deleted
└── (nothing else — no top-level Notes/, Trash/, Attachments/)
```

Blob names are random (`getrandom` → 16 bytes → lowercase hex), assigned once
when an object is created, stable for its lifetime, never derived from the
logical name. A rename or a move between notebooks changes only the manifest
entry, not the blob name, and does not re-encrypt the blob (§6).

**Refinement from the design draft.** The approved design sketched *per-directory*
manifests (a `.tree` plus one `.notes` per notebook). The implementation uses a
**single** sealed manifest blob instead: the crash story is simpler (one atomic
manifest write, one reconciliation rule), the privacy is identical (opaque blob
names, everything under `k_names`), and the draft explicitly left manifest
layout as a Stage-D implementation choice. `created_at` likewise stays in the
plaintext `vault.toml` for both vault kinds rather than moving into an encrypted
`meta` blob — it is coarse metadata (§1) and this avoids `VaultManifest` churn.

### Legacy compatibility — old binaries cannot corrupt an encrypted vault

`v0.1`/`v0.2` SenatorialNotes binaries never meaningfully parse `vault.toml`.
Such a binary opening a v3 vault runs its "create the standard directories" loop
and scans `<root>/Notes/` for `.md`/`.snote` files. Because the encrypted store
is entirely under `.senatorial-notes/store/` — a subtree old binaries never walk
for notes — an old binary sees an empty vault, and any note it creates is
written as plaintext into a fresh top-level `Notes/Inbox/`. The encrypted store
is untouched; ciphertext and plaintext never share a directory.

The forward fence for every binary from Stage A onward is the
`format_version > CURRENT_MANIFEST_VERSION` rejection: a build that understands
up to v2 refuses a v3 vault outright, and this build (understanding v3) refuses
v4+.

An in-place "decrypt vault → plaintext Markdown" escape hatch is planned for a
later stage so the at-rest form is never a one-way door; it is not in v0.3.

## 5. The keyfile — `.senatorial-notes/vault.keys`

Binary. Fixed **88-byte** header (all integers big-endian; the whole header is
AEAD associated data), followed by the wrapped VMK.

| Offset | Len | Field |
| ---: | ---: | --- |
| 0 | 8 | Magic `SNVLT\0\0\0` |
| 8 | 2 | Format version (`1`) |
| 10 | 2 | Flags (`0`) |
| 12 | 16 | `vault_id` (matches `vault.toml`) |
| 28 | 4 | Argon2 memory cost (KiB) |
| 32 | 4 | Argon2 iterations |
| 36 | 4 | Argon2 lanes |
| 40 | 16 | Argon2 KDF salt |
| 56 | 24 | Wrap nonce |
| 80 | 8 | Wrapped-VMK length (constant `48` = 32 VMK + 16 tag; a field so a future scheme can vary it) |
| 88 | 48 | Wrapped vault master key = `XChaCha20Poly1305(KEK, wrap_nonce, VMK, aad = header[0..88])` |

Unlock:

1. `inspect` the header; reject bad magic / unknown version / non-zero flags /
   `vault_id` ≠ caller-supplied / out-of-bounds Argon2 params **before**
   deriving anything.
2. `KEK = Argon2id(password, kdf_salt, params)`.
3. `VMK = XChaCha20Poly1305::decrypt(KEK, wrap_nonce, wrapped_vmk, aad = header[0..88])`
   into a `Zeroizing<[u8; 32]>`.
   - **Wrong password** → wrong KEK → AEAD tag check fails → error.
   - **Tampered header or wrapped VMK** → AEAD tag check fails → error.
4. `k_content / k_names / k_attachments / k_metadata / k_index =
   HKDF-SHA256(ikm = VMK, salt = vault_id, info = <label §3.1>)`, each into a
   `Zeroizing` buffer.

Change vault password (`rewrap_keyfile`): re-derive the KEK from the new
password with a **fresh `kdf_salt` and `wrap_nonce`**, re-wrap the **same VMK**,
`atomic_write` the keyfile, then re-read and verify the new keyfile opens with
the new password and (when the password actually changed) no longer opens with
the old one — the same verify-after-write discipline `.snote` re-keying uses.
**No file blob is rewritten.**

## 6. Object container — `SNENC` version 1

Every blob (`k_content`, `k_attachments`, and the `k_names` manifest) uses this
container. Fixed **80-byte** header, big-endian, whole header authenticated as
AAD.

| Offset | Len | Field |
| ---: | ---: | --- |
| 0 | 8 | Magic `SNENC\0\0\0` |
| 8 | 2 | Container format version (`1`) |
| 10 | 2 | Flags |
| 12 | 16 | `vault_id` |
| 28 | 16 | `object_uuid` — the note's stable UUID for a note; the fixed manifest UUID for the manifest; a random per-object UUID for an attachment |
| 44 | 2 | `object_type` — `0` note, `1` attachment, `3` manifest, `5` metadata (reserved), `6` inner-`.snote`, `7` index (reserved) |
| 46 | 2 | reserved (`0`; a non-zero value is rejected) |
| 48 | 8 | `ciphertext_len` (validated against the file length) |
| 56 | 24 | Object nonce (fresh random per write) |
| 80 | var | ciphertext ‖ Poly1305 tag |

Flags: bit 0 `INNER_SNOTE` (the plaintext is a format-version-1 `.snote`
container, §8); all other bits reserved and must be zero.

### Associated data — immutable identity only

```
aad = header_bytes[0..80]
```

The header — and therefore the AAD — binds **only immutable identity**:
`vault_id`, `object_uuid`, `object_type`, the container `format_version`, and
the `INNER_SNOTE` flag. It **does not** bind the logical or filesystem path.
Logical placement (which notebook a note is in, its logical filename) lives
**only** in the encrypted manifest (§7).

Consequences:

- **Moving or renaming an object inside the vault does not re-encrypt it.** Only
  the manifest entry changes. This matches ordinary-vault `move_note`.
- **Cross-vault substitution fails** — a blob from vault A has the wrong
  `vault_id` in its AAD.
- **Cross-object substitution fails** — the reader builds the AAD from the
  `object_uuid` the manifest *expects*; a blob encrypted under a different
  `object_uuid` fails the tag check.
- **Object-type substitution fails** — the manifest cannot be passed off as a
  note blob (or an inner-`.snote` as a plain note): `object_type` is in the AAD
  and the reader also checks it equals the type it asked for.

Decrypt: `inspect` the header (magic / version / reserved bits / `vault_id`
match / `object_type` is the requested type / `object_uuid` equals the expected
one / `ciphertext_len` matches the file), then
`XChaCha20Poly1305::decrypt(subkey, nonce, ct, aad = header[0..80])`. Any
mismatch → the object is unreadable and the error is surfaced, never silently
skipped and never returned as unauthenticated plaintext.

## 7. The vault manifest

One `k_names` blob at `store/manifest`, `ObjectType::Manifest`, sealed under a
**fixed** manifest object UUID (`5e11a700-1a9e-5700-9bd0-d4ade3904601`) — the
object type and `k_names` subkey already isolate it from every note blob.

Plaintext (before sealing) is JSON:

```jsonc
{
  "schema": 1,
  "created_at": "…",              // manifest creation time (distinct from vault.toml)
  "notebooks": ["Inbox", "Work/Projects", …],
  "notes": [
    { "object_id": "9f3a…",       // the opaque blob filename
      "object_uuid": "…",         // == the note's stable metadata UUID
      "notebook": "Work",
      "filename": "report-1a2b3c4d.md",   // logical name; "…-<shortid>.snote" when per-note encrypted
      "snote": false }
  ],
  "trash":      [ { object_id, object_uuid, note_id, original_relative_path, trashed_at, snote, title } ],
  "recovery":   [ { object_id, object_uuid, note_id } ],
  "attachments":[ { object_id, object_uuid, note_id, logical_name } ]
}
```

**Write ordering (crash safety):** to add / rename / move / delete a note,
(1) `atomic_write` (temp sibling + `fsync` + rename) the content blob, then
(2) `atomic_write` the re-sealed manifest that references it. A crash between
(1) and (2) leaves an orphan blob (no manifest entry). The **reconciliation
pass** runs once on every unlock, before the model is built: any store file not
named `manifest` and not referenced by the manifest is *moved* into
`store/orphans/` — **never deleted**. A manifest entry whose blob is missing is
left in place and surfaces as an unreadable note; no note is silently dropped.

## 8. `.snote` inside an encrypted vault

A per-note-encrypted `.snote` is kept as an **inner layer**. Its existing
format-version-1 container bytes become the *plaintext* input to a `SNENC` blob
with flag `INNER_SNOTE` set and `object_type = 6`.

```
disk blob  =  SNENC( k_content, aad = header[0..80],
                     plaintext = < the exact .snote v1 container bytes > )
```

Peeling the outer layer yields **byte-identical** valid v1 `.snote` bytes.
Therefore:

- a `.snote` inside an encrypted vault keeps its own per-note password;
- the per-note password semantics (`encrypt_note`, `change_encrypted_password`,
  `remove_encryption`, wrong-password rejection) are unchanged;
- a future "decrypt vault" writes the inner `.snote` back out unchanged.

Lock states:

| Vault | Note (`.snote`) | Behaviour |
| --- | --- | --- |
| locked | — | nothing readable; the whole-vault lock screen is shown |
| unlocked | locked | listed as a locked note with the anonymous `Locked Note · XXXXXXXX` placeholder; opening it prompts for the *note* password |
| unlocked | unlocked | normal editing; a save re-encrypts inner (note session key) then outer (`k_content`) |
| locked | (was unlocked) | vault lock drops both the vault keys and any note session; the in-memory model is cleared |

`on_note_switch` locking (from `LockingConfig`) still applies to the *inner*
`.snote` while the vault is unlocked; `after_minutes` / `on_focus_loss` /
`on_minimize` additionally lock the *whole vault*.

## 9. Unlock / lock lifecycle

**Open an encrypted vault** (`Vault::create` → `EncryptedStore::open`):

1. `vault.toml` `format_version == 3` / `kind == "encrypted"` ⇒ this flow. The
   store opens **locked**: no key material, no model.
2. Acquire the advisory vault lock (`docs/VAULT_LOCK.md`) unless "Open
   read-only".
3. The UI shows the **vault-locked screen** (a dedicated `GtkStack` page) with
   an "Unlock Vault" button.
4. On unlock, `ui::begin_vault_unlock` reads `vault.keys` and prompts with the
   existing `present_password_dialog`. Argon2id + VMK unwrap + HKDF expansion
   run on a **worker thread** via `gio::spawn_blocking`; the derived `VaultKeys`
   (which is `Send`: five `Zeroizing<[u8;32]>` + a `Uuid`) come back to the main
   thread through `glib::spawn_future_local`. No `RefCell<AppState>` borrow is
   held across the hop — the worker gets only `Vec<u8>` keyfile bytes, a
   `Uuid`, and a `Zeroizing<String>` password — so the RefCell/GTK
   stabilization is untouched.
5. The continuation re-checks the `SessionRegistry` generation (a vault switch
   during derivation makes it inert), then calls `Vault::finish_unlock(keys)`,
   which loads + verifies the manifest and runs reconciliation (§7).
6. `enter_vault_workspace` builds the same `Vec<NoteSummary>` / notebook list
   the ordinary path builds. From here the UI is identical to an ordinary
   vault.
7. **Failed unlock** (wrong password, tampered keyfile, tampered manifest):
   `finish_unlock` returns an error, the store stays locked, nothing on disk is
   touched, and the lock screen shows the reason. Repeated failures cannot
   corrupt state.

**Lock Now / auto-lock / switch away / normal exit** (`ui::lock_vault`):

1. `persist_active` flushes any pending edit. If the save fails, the vault stays
   unlocked.
2. `persist_vault_session_state`, then `clear_sensitive_documents` (drops
   `active`, `unlocked_cache`, `plain_cache`, zeroizes note buffers).
3. `Vault::lock()` → `EncryptedStore::lock()` drops `Unlocked` → `VaultKeys`
   zeroizes the VMK-derived subkeys and the cached manifest plaintext is freed.
4. `state.notes` / `state.trash` cleared; the editor buffer, title, and search
   box emptied; the `SessionRegistry` generation bumped so any armed callback
   is inert; the vault-locked screen shown.
5. Nothing to scrub on disk — there is no plaintext cache.

Auto-lock is wired into the existing `connect_locking_events`: the
focus-loss/minimize handler and the 30-second idle timer both call `lock_vault`
alongside the per-note `lock_all_encrypted`. `touch_sensitive_activity` marks
the idle clock whenever the open note is a `.snote` **or** the whole vault is an
unlocked encrypted vault (so even a plaintext `.md` note in the editor keeps the
vault awake).

## 10. Filesystem watcher in an encrypted vault

`VaultWatcher` is unchanged in API (it watches the vault root recursively).
`note_tree_snapshot` — the cheap stat-only baseline the 500 ms poll compares
against — now follows `Vault::watch_paths()`, which for an encrypted vault is
`[store/]` (the opaque ciphertext directory) instead of `Notes/` + `Trash/`.

- The watcher **never parses a blob as Markdown**; the snapshot is `(path,
  mtime, len)` tuples only.
- **Unlocked:** an external change under `store/` that does not match the
  baseline triggers the same safe rescan/reload path as an ordinary vault; a
  tampered blob then surfaces as an unreadable note rather than as plaintext.
- **Locked:** the poll drains events but does no reload (`editor_is_clean &&
  !vault_locked`); the full manifest reload happens on the next unlock.
- Self-writes are suppressed by the `watch_baseline` comparison exactly as for
  `.md` writes.

## 11. Failure modes — summary

| Input | Result |
| --- | --- |
| Wrong vault password | unlock fails; vault stays locked; no model built; nothing on disk changed |
| Tampered keyfile (any byte) | unlock fails even with the right password; the vault still *opens* (locked) so the user is told why |
| Tampered blob (any byte) | that note/manifest reads as unreadable and the error is surfaced; never unauthenticated plaintext |
| Two blobs swapped / a manifest entry pointing at the wrong blob | `object_uuid` (and/or `object_type`) AAD mismatch → unreadable; surfaced |
| A blob from another vault dropped in | `vault_id` AAD mismatch → unreadable; surfaced |
| Note moved/renamed inside the vault | **blob bytes unchanged**; only the manifest entry moves |
| Password change | keyfile re-wrapped; **no blob re-encrypted**; old password no longer opens the vault |
| `vault_id` mismatch (keyfile vs `vault.toml`) | rejected; vault will not unlock |
| `format_version` newer than known | `Error::UnsupportedVaultVersion`; refused, nothing touched |
| Crash mid-write (blob vs manifest) | reconciliation on next unlock; orphan blob quarantined to `store/orphans/`, never deleted; **zero note loss** |
| Stray `*.tmp` left in `store/` | ignored by unlock and scan; quarantined as an orphan |
| v3 vault opened by a build that only knows v2 | rejected by the `format_version > 2` fence (Stage A, tested) |

## 12. Known limitations

- **No blob-size padding** — a blob's length leaks the approximate size of the
  note/attachment it holds. Deferred past v0.3.
- **`created_at` is plaintext** in `vault.toml` for both vault kinds (§1, §4).
- **No in-place conversion** of an existing ordinary vault to an encrypted one —
  deferred to a later release. v0.3 creates encrypted vaults from scratch.
- **`k_metadata` / `k_index` are derived but unused** — reserved for a per-vault
  metadata blob and an encrypted local search index.
- An unlocked vault is exactly as exposed as an open `.snote` (§1). Memory
  zeroization is best-effort (`Zeroizing`); it cannot defeat swap or a memory
  scraper with sufficient privilege, and the format makes no such promise.
