# SenatorialNotes v0.3 — Vault Architecture

Status: **approved 2026-09-02** (five review decisions folded in — §8). Stage A
(manifest v2 + migration) is in progress; later stages are still design.

This is the plan for the release the project README already names:

> ### v0.3 — Vault architecture (future)
> - multiple Senatorial vaults
> - ordinary vaults
> - encrypted vaults (whole-vault encryption)
> - continued support for individually encrypted notes
>
> SenatorialNotes will manage information inside Senatorial vaults. It is not
> intended to become a general-purpose file manager.

The companion document [`docs/ENCRYPTED_VAULT_FORMAT.md`](ENCRYPTED_VAULT_FORMAT.md)
specifies the cryptographic design (key hierarchy, containers, on-disk layout).
This document covers scope, the `vault.toml` schema and migration, the vault
lock, the module-by-module impact, the risk register, the test matrix, and the
staging plan.

## Non-negotiable constraints (carried from the task brief)

- **Per-note `.snote` encryption is not removed.** In an ordinary vault it
  behaves exactly as it does in v0.2.0-alpha. Format version 1 `.snote`
  containers keep opening unchanged.
- **The `.snote` format is not redesigned** unless a concrete
  correctness/security defect requires it. None is currently known.
- **No new cryptographic primitives.** Only Argon2id and XChaCha20-Poly1305,
  exactly as already used in `src/crypto.rs`.
- **No features outside the scope below.** Attachments UI, document management,
  local indexing, OCR, history browser, import/export remain "Later releases"
  per the README and are explicitly out of scope for v0.3.
- **The RefCell / GTK re-entrancy stabilization is complete and frozen.** No
  file, guard, or `SignalGate` in `src/ui.rs` / `src/ui_state.rs` is modified
  without a concrete, stated defect. New vault UI follows the existing
  discipline (scoped borrows, `SignalGate::suppress()` around programmatic
  widget writes, `idle_add`/`timeout_add` deferral); it does not change it.
- **Markdown stays authoritative in an ordinary vault.** In an *encrypted*
  vault, at-rest opacity necessarily replaces plaintext-on-disk while the vault
  is locked; the escape hatch is an explicit "decrypt vault / export to
  plaintext" operation (see risk R5).

## 1. Scope

### 1.1 Multiple vaults

- A real in-app vault switcher (a menu / header control), not "open a folder and
  lose your place".
- **Open Vault…** — the existing native folder picker (`gtk::FileDialog`),
  unchanged.
- **Open Recent** — a submenu populated from `config.recent_vaults` (already
  persisted, capped at 10 by `AppConfig::remember_vault`).
- Switching vaults **without restarting** the process. `ui::open_vault` already
  does most of this (it calls `clear_sensitive_documents`, swaps `state.vault`,
  rebuilds every list, resets `flow`/`filter`); Stage B hardens and surfaces it.
- **Per-vault last state**, restored where practical: selected view
  (`ViewMode`), selected note UUID, and editor scroll position. Ordinary vaults
  store this in the app config keyed by `vault_id`; encrypted vaults store it
  inside the vault (see §1.4 and the format doc), because a note UUID plus a
  notebook name is mildly sensitive.
- **Missing / moved vault paths** in the recent list are shown as unavailable,
  never silently dropped and never opened blindly. Selecting one offers "Locate
  folder…" (re-pick) or "Remove from list".

Explicitly *not* in v0.3: multiple vaults open in multiple windows
simultaneously. One writable vault per process (see §1.3). Revisit in a later
release if there is demand.

### 1.2 Vault type architecture

- A `VaultKind` with exactly two values: `Ordinary` and `Encrypted`.
- Recorded in `vault.toml` (§2).
- An explicit `format_version` bump (1 → 2) with a **safe, lossless, one-way**
  migration for every existing vault (§2.2).
- **Existing vaults migrate to `Ordinary`.** There is no ambiguity: encrypted
  vaults did not exist at manifest version 1.
- **The kind is never changed implicitly.** Migration always yields `Ordinary`.
  Only an explicit, confirmed user action ("Encrypt this vault" / "Decrypt this
  vault") may change it, and that conversion is deferred (§7): v0.3 commits to
  *creating* new encrypted vaults, not to in-place conversion of existing ones.

### 1.3 Vault locking

An advisory lock so two writable SenatorialNotes instances cannot edit one vault
at once (`SPECIFICATION.md` → "File-System Integration": "Use a vault lock…").

- New module `src/vault_lock.rs`. On opening a vault for writing, create
  `<vault>/.senatorial-notes/vault.lock` containing
  `{ pid, hostname, boot_id, app_version, acquired_at }` and hold the file open
  for the session.
- When a vault that is already locked is requested, offer exactly:
  - **Return to the existing window** (if it is this process's own window).
  - **Open read-only** — no lock acquired; every write path returns
    `Error::VaultReadOnly`; the UI disables note creation/editing/trash/etc.
  - **Cancel.**
- **Stale-lock detection is conservative.** A lock is considered live only if
  `hostname` matches *and* `boot_id` matches *and* the `pid` is alive. Anything
  else (different host, different boot, dead pid, unparseable file) is *stale*
  but is **never deleted automatically** — the user is shown the lock's contents
  and must confirm "the other instance is not running — take over".
- The lock is released on clean exit (`connect_close_request` path, which
  already runs `persist_active` + `clear_sensitive_documents`).

### 1.4 Whole-vault encryption

Full specification in [`docs/ENCRYPTED_VAULT_FORMAT.md`](ENCRYPTED_VAULT_FORMAT.md).
Summary of the v0.3 commitment:

- Create a new encrypted vault, protected by **one vault password**. (Converting
  an *existing* ordinary vault in place is deferred to v0.4 — see §7.)
- Key hierarchy (decision, 2026-09-02): password → Argon2id → **KEK**; the KEK
  wraps **one random 32-byte vault master key (VMK)** stored in the keyfile;
  **HKDF-SHA256** derives per-domain subkeys from the VMK — separate keys for
  note/content, names/manifests, attachments, sensitive metadata, and a
  reserved future encrypted index. Each subkey encrypts its domain's files with
  XChaCha20-Poly1305 and a fresh 24-byte random nonce per write. A password
  change re-wraps the **VMK only** — no file is re-encrypted. Adds `hkdf` +
  `sha2` (RustCrypto) — in the crypto stage (Stage D), not this stage. Every
  HKDF `info` label is a format-stable constant documented in the format spec.
- **Everything sensitive is encrypted at rest**: note bodies, titles, tags,
  notebook names and tree structure, note counts per notebook, attachments,
  attachment names, `history/` and `recovery/` content, per-vault UI state, and
  any future search index. Filenames and directory names on disk are opaque
  random tokens; the logical name and placement live only in an encrypted
  per-directory manifest. The **only** plaintext kept is the minimum needed to
  identify and open the vault: `vault_id`, `format_version`, `kind`, and the
  advisory-lock metadata.
- **Unlock-on-open** (vault-password prompt), **Lock Now**, and configurable
  **automatic locking** reusing `LockingConfig` (`after_minutes`,
  `on_focus_loss`, `on_minimize`, `on_exit`; `on_note_switch` is not meaningful
  for a whole vault and is ignored).
- No plaintext in any disposable cache after lock — **there is no on-disk cache
  today** (`plain_cache` / `unlocked_cache` are in-memory `HashMap`s in
  `AppState`; `CACHE_DIR_NAME` is defined but unused). Locking drops the
  in-memory model; nothing needs scrubbing on disk.
- **Search works only while unlocked**, entirely from the in-memory
  `NoteSummary` list — unchanged from how search already works.
- **Wrong password** and **tampered ciphertext / keyfile / manifest** fail
  closed: the vault refuses to open, and nothing is decrypted or partially
  loaded.
- **Atomic-write guarantees are preserved** — encryption happens in memory,
  then `storage::atomic::atomic_write` writes the ciphertext exactly as today.
  Manifest + blob updates have a defined order and an unlock-time reconciliation
  pass (risk R7).
- **The watcher is advisory-only in an encrypted vault** — it cannot merge
  hand-edited ciphertext, so it detects "changed under us / changed while
  locked" and prompts a reload; it never attempts a content merge and never
  writes a plaintext recovery file.

### 1.5 Existing per-note encryption inside an encrypted vault

- `.snote` support stays.
- **A `.snote` inside an encrypted vault is an additional *inner* layer**, not a
  replacement. The `.snote` container bytes (the existing format-version-1
  container) are the plaintext input to the outer vault-file container. The
  file is double-locked: the outer layer opens with the vault password, the
  inner layer with the note's own password.
- After peeling the outer layer, the inner bytes are a byte-identical, valid
  standalone format-version-1 `.snote` — so "decrypt vault" produces an ordinary
  vault whose `.snote` files still open with their original passwords, and an
  existing `.snote` adopted into an encrypted vault keeps its password.
- Four lock states are possible and all are defined: (vault locked | unlocked) ×
  (note locked | unlocked). Enumerated in the format doc.

## 2. `vault.toml` schema and migration

### 2.1 Schema

Current (`format_version = 1`, written by `Vault::create` today):

```toml
format_version = 1
vault_id = "550e8400-e29b-41d4-a716-446655440000"
created_at = "2026-08-25T17:30:00Z"
```

Stage A (`format_version = 2`, ordinary vaults only — verified real output):

```toml
format_version = 2
vault_id = "550e8400-e29b-41d4-a716-446655440000"   # unchanged, preserved by migration
created_at = "2026-08-25T17:30:00Z"                  # unchanged, preserved by migration
kind = "ordinary"                                    # "ordinary" | "encrypted"
migrated_from = 1                                    # integer; present only on an upgraded manifest
```

**Encrypted vaults get their own `format_version` (3), not a `kind` flag on
v2.** `format_version` is the single compatibility gate: a structurally
incompatible layout is *always* a version bump, so a binary that understands up
to N refuses anything `> N` (§Forward-compatibility policy below). The `kind`
field is still carried — a v3 manifest is `format_version = 3, kind = "encrypted"`
— but it is semantic metadata for a *compatible* reader, never the fence.
`[encryption]` (keyfile reference, HKDF format number) is added alongside, in
Stage D. This Stage-A build refuses `format_version > 2` outright, and also
refuses a (malformed) `format_version = 2, kind = "encrypted"` manifest with
`Error::UnsupportedVaultKind`.

### Forward-compatibility policy

- New SenatorialNotes **may** migrate supported older vault formats forward
  (Stage A does v1 → v2).
- Older SenatorialNotes versions are **not guaranteed** to understand newer vault
  formats, and this is **not** something the format relies on. In particular
  `serde` ignoring unknown fields is never a safety mechanism.
- The current binary **always** rejects `format_version > CURRENT_MANIFEST_VERSION`
  before touching anything (Stage A, tested).
- **Legacy hazard (documented, unfixable — do not rewrite old tags):** the
  `v0.1.0-alpha` and `v0.2.0-alpha` binaries **never read `vault.toml` at all**
  (their `VaultManifest` type is write-only; `Vault::open` only checks that the
  file *exists*). Such a binary opening a v2 **or v3** vault will not fail — it
  runs its unconditional "create the 7 standard directories" loop, then scans
  `Notes/` for `.md`/`.snote` files (silently skipping anything else) and
  presents the vault as ordinary. Against a v2 *ordinary* vault that is
  harmless (the note format is unchanged). Against a future v3 *encrypted*
  vault it is a real hazard: the old binary would show it as empty and, if the
  user creates a note, write **plaintext `.md` into the encrypted vault's
  directory**. Because we cannot fix shipped binaries, encrypted-vault
  creation must not depend on them: the mitigation is structural — the entire
  encrypted object store lives under `.senatorial-notes/`, which old binaries
  never scan, so an old binary's writes land in a separate, empty, top-level
  `Notes/` tree that a current binary detects and quarantines rather than
  intermingling ciphertext and plaintext. See
  [`docs/ENCRYPTED_VAULT_FORMAT.md` §4 and §"Legacy compatibility"](ENCRYPTED_VAULT_FORMAT.md).

Rust representation — **Stage A** ships this in a new sibling module
`src/vault_manifest.rs` (not a `vault/` directory split; the larger
`Storage`-trait refactor of `vault.rs` is Stage D):

```rust
pub const CURRENT_MANIFEST_VERSION: u32 = 2;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultManifest {
    pub format_version: u32,
    pub vault_id: Uuid,
    pub created_at: DateTime<Utc>,
    #[serde(default)]                                  // absent in v1 → Ordinary
    pub kind: VaultKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migrated_from: Option<u32>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VaultKind {
    #[default]
    Ordinary,
    Encrypted,
}
```

No `#[serde(deny_unknown_fields)]` — forward-compatibility with fields a later
version adds (e.g. `[encryption]`), matching how `NoteMetadata` already
tolerates unknown front-matter.

### 2.2 Migration algorithm (`VaultManifest::load`, called from `Vault::create`/`open`)

```
read .senatorial-notes/vault.toml
probe { format_version: u32 } only        # so a newer schema fails as
                                          # "unsupported version", not "corrupt"
if probe.format_version > CURRENT_MANIFEST_VERSION (2):
    -> Error::UnsupportedVaultVersion { found, supported }   # refuse; touch nothing

full-parse VaultManifest (unparseable -> Error::VaultManifestCorrupt; touch nothing)

match format_version {
    0        => Error::VaultManifestCorrupt
    1        => {
        // v1 predates VaultKind entirely.
        manifest.format_version = 2
        manifest.kind           = Ordinary        // forced; any `kind` key a
                                                  // hand-edit added to a v1 file
                                                  // is ignored, never "encrypted"
        manifest.migrated_from  = Some(1)
        match atomic_write(vault.toml, manifest) {
            Ok       => Migration::Persisted { from: 1 }
            Err(io)  => Migration::InMemoryOnly { from: 1, reason }  // read-only: R1
        }
        // either way: proceed to open as Ordinary
    }
    2        => Migration::NotNeeded            // use `kind` as written
}
if manifest.kind == Encrypted -> Error::UnsupportedVaultKind   // engine not in this build
```

When the migration cannot be persisted (`Migration::InMemoryOnly`), `Vault` is
opened with **`read_only = true`**: `ensure_directories` is skipped and every
mutating method returns `Error::VaultReadOnly` before any filesystem write, so
the on-disk manifest version and the note tree can never drift into a mixed
state. Stage B's "Open read-only" reuses this same flag.

Invariants, each covered by a test (§6):

- Migration **preserves** `vault_id` and `created_at` verbatim.
- Migration **never** produces `kind = "encrypted"` (a v1 file's `kind` key, if
  any, is ignored and forced to `ordinary`).
- Migration **never** reads, writes, renames, or touches any note file, nor any
  file other than `vault.toml` itself.
- A v1 vault on read-only media opens successfully as `Ordinary` (in memory),
  reporting `Migration::InMemoryOnly`, **`is_read_only()` true**, not an error.
- A read-only session **cannot mutate**: `create_note`, `create_notebook`,
  `save_note`, `commit_title`, `move_note`, `move_to_trash`, `restore_from_trash`,
  `permanently_delete`, `empty_trash`, `encrypt_note`, `write_recovery`, … all
  return `Error::VaultReadOnly` and write nothing; no partial directory tree is
  created; `Migration::warning()` remains available.
- `format_version > 2` is refused with a clear message and **without** rewriting
  the manifest or creating any missing standard directory.
- `Vault::create` on a brand-new location writes a v2 `Ordinary` manifest; a
  second open is `Migration::NotNeeded` and does not rewrite the file (no churn).
- `VaultKind` serializes as exactly `"ordinary"` / `"encrypted"` (verified against
  real `toml::to_string_pretty` output, not assumed).

**Not a compatibility guarantee:** an old `v0.1`/`v0.2` binary opening a v2
vault does not *fail*, but that is because those binaries ignore `vault.toml`
entirely — it is not something the format promises or depends on. See the
Forward-compatibility policy in §2.1.

## 3. Existing modules that change

| Module | Change | Risk |
| --- | --- | --- |
| `src/vault.rs` | **Stage A:** `struct Vault` gains `manifest: VaultManifest`, `migration: Migration`, and `read_only: bool`; `create`/`open` call `VaultManifest::load` (migrate/validate) *before* creating standard directories, skip `ensure_directories` when read-only, and set `read_only` from `Migration::InMemoryOnly`; a private `ensure_writable()` guard is the first line of every mutating method (16 sites); new `kind()` / `vault_id()` / `manifest()` / `migration()` / `is_read_only()` accessors; the private `VaultManifest` struct is removed (moved to `vault_manifest.rs`). **Stage D:** route every `fs::read` / `fs::write` / `atomic_write` / `fs::rename` / `read_dir` through a new `Storage` abstraction (§4, `PlainStorage` = today's behavior, `EncryptedStorage` = crypto + manifests); `Vault`'s public method shapes stay identical. | Stage A: Low (internal; guarded by the full existing suite). Stage D: High — every storage test must pass unchanged for `Ordinary`. |
| `src/crypto.rs` → `src/crypto/` | **Stage D only.** Split into `crypto/mod.rs` + `crypto/note.rs` (today's `.snote` code, **byte-identical**) + `crypto/vault.rs` (new: KEK derivation via Argon2id, VMK wrap/unwrap, HKDF-SHA256 subkey derivation, `VaultCipher`, keyfile header I/O). Shared header-read/param-validation helpers factored carefully. **Untouched by Stage A.** | Med — `.snote` behavior must not shift; golden vector test (§6). |
| `src/config.rs` | `recent_vaults: Vec<PathBuf>` kept for compat; add `recent_vault_meta: Vec<RecentVault>` (`{ path, vault_id, kind, last_opened, ui_state }`) with `#[serde(default)]`. `LockingConfig` gains a doc note that it also governs whole-vault locking; no field change. | Med — config read by old/new binary; additive only. |
| `src/error.rs` | **Stage A:** `UnsupportedVaultVersion { found, supported }`, `VaultManifestCorrupt(String)`, `UnsupportedVaultKind`, `VaultReadOnly`. **Later stages:** `VaultKindMismatch`, `VaultAlreadyOpen { by }`, `VaultLockStale`, `WrongVaultPassword` (or reuse `DecryptionFailed`). | Low. |
| `src/constants.rs` | **Stage A:** none required (`CURRENT_MANIFEST_VERSION` lives in `vault_manifest.rs`; the manifest filename can stay a local const). **Later:** `VAULT_KEYFILE = "vault.keys"`, `VAULT_LOCK_FILE = "vault.lock"`, magic strings (`SNVLT`, `SNENC`). | Low. |
| `src/ui.rs` | `AppState` gains `vault_session: Option<VaultSession>` (holds `VaultCipher` + manifest cache + auto-lock state) and a `VaultLock` handle. New: vault-switcher menu + Open Recent, vault-password prompt (reuse `present_password_dialog`), "already open" dialog, "vault locked" screen, missing-recent-path handling, per-vault state save/restore in `open_vault` and the close handler. **KDF runs off the GTK main thread** (worker thread → `glib::MainContext` channel → main thread applies result); the worker returns plain data, so no RefCell is held across the await. No change to `SignalGate`, selection coordinator, or any existing borrow-scoping. | High — must not regress the stabilization; every new callback audited to the existing standard. |
| `src/watcher.rs` | No API change. Add encrypted-vault semantics doc + a note that in an encrypted vault the poll handler treats any non-self change as "reload manifests" (unlocked) or "reload on next unlock" (locked), never a merge. | Low. |
| `src/paths.rs` | Helpers for opaque encrypted-vault on-disk tokens (`opaque_file_name()`, validation). Ordinary-vault helpers unchanged. | Low. |
| `src/lib.rs` | Register new modules; re-exports. | Low. |
| `Cargo.toml` | **Stage D:** adds `hkdf` and `sha2` (both RustCrypto, same ecosystem as `argon2` / `chacha20poly1305`) for HKDF-SHA256 subkey derivation. **No dependency change in Stage A.** | Low. |
| `README.md` | Roadmap: mark v0.3 in progress; "What works" once shipping; storage-format section gains an encrypted-vault subsection; known-limitations updated. | Low. |
| `SECURITY.md` | New sections: whole-vault threat model, what an encrypted vault does / does not protect (filenames opaque, but size/count/mtime/`vault_id` still exposed), the key hierarchy summary, `.snote`-inside-encrypted-vault semantics, lock-file semantics. | Low. |
| `CHANGELOG.md` | `[Unreleased]` → the v0.3 entries as stages land. | Low. |
| `docs/STABILITY_TEST_PLAN.md` | Interactive gate additions: vault switching, Open Recent with a missing path, lock contention (two instances), encrypted-vault unlock/Lock Now/auto-lock, `.snote` inside an encrypted vault. | Low. |
| `data/…metainfo.xml` | New `<release version="0.3.0-alpha">`. | Low. |
| `packaging/flatpak/…yml` | Review filesystem access for Open Recent by raw path under the sandbox (risk R8). Still **no `--share=network`**. | Med. |

## 4. New modules to create

| Module | Stage | Responsibility |
| --- | --- | --- |
| `src/vault_manifest.rs` | **A** | `VaultManifest` v1/v2, `VaultKind`, `Migration`, `CURRENT_MANIFEST_VERSION`, `load` + migrate (§2.2), `write`, `new_ordinary`. Pure, no GTK, no crypto. Sibling module (not a `vault/` directory). |
| `src/vault_storage.rs` | D | The `Storage` trait and its two impls. `PlainStorage` delegates straight to `std::fs` + `storage::atomic` (today's exact behavior). `EncryptedStorage` wraps a `VaultCipher`, maintains the encrypted per-directory manifests, does opaque-name mapping, and runs the crash-reconciliation pass on unlock. |
| `src/vault_directory_manifest.rs` | D | The encrypted manifest types: `{ opaque_token → (logical_name, object_uuid, kind) }` per directory, plus the notebook tree. Serialized (JSON), then encrypted with the `names` subkey. **All of it lives under `.senatorial-notes/` so an old binary never scans it** (§legacy hazard, §2.1). |
| `src/crypto/vault.rs` | D | `derive_kek` (Argon2id), `VaultMasterKey` wrap/unwrap under the KEK, `derive_subkeys` (HKDF-SHA256 → `content`/`names`/`attachments`/`metadata`/`index`), `VaultCipher { encrypt_object, decrypt_object }`, keyfile header encode/`inspect`. Uses `argon2` + `chacha20poly1305` (in tree) + `hkdf` + `sha2` (new). Encrypted vaults are `format_version = 3`. |
| `src/vault_lock.rs` | C | `VaultLock::acquire(root)` / `LockStatus` (`Free` / `HeldByThisProcess` / `HeldLive(info)` / `Stale(info)`); releases on `Drop`. Liveness = hostname + `boot_id` (`/proc/sys/kernel/random/boot_id`) + pid alive (`kill(pid, 0)` via `libc`, already a dep). Never deletes a `HeldLive` lock; a `Stale` lock is only reclaimed after explicit caller confirmation. |
| `src/vault_session.rs` | E | `VaultSession` — the in-memory unlocked-vault handle: owns the `VaultCipher` (zeroizing), the loaded manifest cache, the auto-lock deadline, and the `LockingConfig` snapshot. Analogous to `crypto::EncryptedSession` but vault-scoped. `Drop` zeroizes. |
| `tests/vault_manifest.rs` | **A** | §6. |
| `tests/vault_lock.rs` | C | §6. |
| `tests/encrypted_vault.rs` | D/E | §6 — mirrors `tests/encryption_regressions.rs`. |
| `tests/vault_switching.rs` | B | §6. |
| `tests/fixtures/` | **A** onward | Golden vectors: a checked-in v1 `vault.toml` (Stage A); a v1 `.snote` container and a v1 encrypted-vault sample (Stage D). |

`ui.rs` stays a single file unless the vault-menu code makes it unwieldy; if
split, into `src/ui/vault_menu.rs` only, leaving the stabilized core untouched.

## 5. Migration & backward-compatibility risk register

| # | Risk | Mitigation | Test |
| --- | --- | --- | --- |
| R1 | v1→v2 manifest upgrade writes to the vault on first open; fails on read-only media. | The upgrade is attempted; on failure the vault opens `Ordinary` **read-only** (`Migration::InMemoryOnly`, `is_read_only()` true, `ensure_directories` skipped) — never an error. Every mutating method is guarded by `ensure_writable()`, so a read-only session cannot produce a mixed manifest-version/note-tree state. | ✅ `vault_manifest::{read_only_vault_still_opens_via_in_memory_migration, read_only_migration_blocks_note_creation, read_only_migration_blocks_mutation_of_an_existing_note, read_only_migration_does_not_partially_create_the_directory_tree, writable_v1_vault_migrates_and_stays_writable}` |
| R2 | `config.toml` schema drift (old binary ↔ new binary). | Keep `recent_vaults: Vec<PathBuf>`; all new fields `#[serde(default)]`; never remove or rename. | `config::old_config_still_loads`, `config::roundtrip` |
| R3 | `.snote` inside an encrypted vault: 4 lock states, ordering of vault-unlock vs note-unlock. | Explicit state machine in the format doc; `object_type = inner-snote` in the container header (bound as AAD) marks these blobs; all 4 states tested. | `encrypted_vault::snote_inside_all_four_states` |
| R3b | AEAD binding: a note moved/renamed inside a vault must **not** re-encrypt (decision, 2026-09-02). Associated data binds **immutable identity only** — `vault_id`, `object_uuid`, `object_type`, container `format_version` — never the logical/filesystem path. Logical placement lives in the encrypted manifests. | Container AAD carries the four immutable fields; a move updates only the manifest entry. Cross-vault, cross-object, and object-type substitution still fail authentication. | `encrypted_vault::move_does_not_reencrypt`, `encrypted_vault::cross_object_and_cross_vault_substitution_fail` |
| R4 | Existing format-version-1 `.snote` files must keep opening forever. | `crypto/note.rs` frozen; golden `.snote` vector fixture; all 7 `encryption_regressions` tests kept. | `encryption_regressions.rs` (unchanged) + `encrypted_vault::golden_v1_snote_opens` |
| R5 | Opaque filenames break the "plaintext Markdown on disk" expectation for encrypted vaults. | Prominent docs; robust "Decrypt vault / Export to plaintext Markdown" producing an ordinary vault; never auto-encrypt an existing vault; encryption is always an explicit opt-in at creation. | `encrypted_vault::decrypt_vault_yields_ordinary_plaintext` |
| R6 | Watcher false-positives from many ciphertext rewrites in an encrypted vault. | The existing stat-baseline (`watch_baseline`) already suppresses self-writes; extend it to cover manifests; advisory-only handler. | `encrypted_vault::self_writes_do_not_trigger_reload` |
| R7 | Crash between writing a file blob and updating its directory manifest → orphan blob or dangling entry. | Defined order: **write blob, fsync, then atomically replace the manifest.** Unlock-time reconciliation: orphan blobs (no manifest entry) are quarantined to `.senatorial-notes/orphans/`, not deleted; dangling entries (no blob) are reported, entry dropped, zero note loss. | `encrypted_vault::crash_between_blob_and_manifest_loses_nothing` |
| R8 | Flatpak: Open Recent by raw path may be denied by the sandbox (portal grants are per-session unless persisted). | Investigate `xdg-desktop-portal` document persistence / `--persist`; fall back to re-prompting with `gtk::FileDialog` when a raw-path open is denied; document the limitation. Never add `--filesystem=home`. | Manual (Flatpak build); documented in packaging README |
| R9 | Stale lock after `kill -9` / power loss / NFS. | `boot_id` + hostname + pid-liveness; a lock from another host or another boot is *stale*; stale locks are shown to the user and reclaimed only on explicit confirmation; never auto-deleted. | `vault_lock::stale_when_pid_dead`, `vault_lock::foreign_host_never_auto_reclaimed` |
| R10 | Argon2id (64 MiB, 3 passes) on every encrypted-vault open → UI stall + memory spike. | Run the KDF on a worker thread; return the derived material to the main thread via a `glib::MainContext` channel; show a spinner. The worker returns plain bytes — no `RefCell` borrow crosses the thread hop, so the stabilization is unaffected. | `ui_source_invariants::vault_unlock_kdf_runs_off_main_thread` (source guard) |
| R11 | "Already open" check: two different paths, same copied `vault_id`. | Lock is keyed by the **canonicalized path**; `vault_id` is only a secondary/friendly signal. | `vault_lock::two_paths_same_vault_id_lock_independently` |
| R12 | Incomplete zeroization of KEK / VMK / HKDF subkeys / decrypted manifests / note buffers. | `Zeroizing` everywhere, mirroring `EncryptedSession`; `VaultSession::drop` and `VaultCipher::drop` zeroize; HKDF output written into `Zeroizing` buffers; accept and document the standard Rust/GTK transient-copy caveat (already in `SECURITY.md`). | `encrypted_vault::locking_leaves_no_plaintext_on_disk` + review |
| R13 | Metadata still leaks in an encrypted vault: file count, individual sizes, mtimes, `vault_id`, total size. | Document explicitly in `SECURITY.md` (mirrors the `.snote` disclosure). Optionally pad blob sizes to a bucket — **out of scope for v0.3**, noted as future. | Doc review |
| R14 | Per-vault UI state (last note UUID + notebook) written to the *plaintext* app config for an encrypted vault would leak. | Encrypted vaults store UI state **inside** the vault (encrypted metadata-domain blob); the app config holds only `{ path, vault_id, kind, last_opened }` for them. | `vault_switching::encrypted_vault_ui_state_not_in_plaintext_config` |
| R15 | Refactoring `vault.rs` onto a `Storage` trait (Stage D) regresses an ordinary-vault edge case (symlink rejection, `renameat2` no-replace, dir fsync, permission preservation). | `PlainStorage` is a thin pass-through to the exact functions used today; the entire existing `model_and_storage` + `notebooks` suite runs against `Ordinary` unchanged and must stay green. | full existing suite (135 tests) |
| R16 | HKDF `info` labels drift between builds → a vault written by one build cannot be read by another. | Every label is a `const &[u8]` in `crypto/vault.rs`, listed in the format doc's label table, and treated as **format-stable** (changing one is a format-version bump). A source-guard test asserts the constants match the documented bytes. | `encrypted_vault::hkdf_labels_match_format_doc` |
| R17 | Stage A adds fields to `struct Vault`; some construction site or `Clone`/`Debug` assumption breaks. | Only `Vault::create` constructs `Vault` (`open` delegates to it); `VaultManifest`/`Migration` derive `Clone, Debug, PartialEq, Eq`. Full existing suite must stay green. | ✅ full existing suite (135) + 18 new + `cargo build --all-features` |
| **R18** (audited Stage A.1) | **Legacy hazard, unfixable:** `v0.1`/`v0.2` binaries never read `vault.toml` (their `VaultManifest` is write-only; `open` only checks the file exists). They **accept** any v2/v3 vault, run the "create 7 standard dirs" loop, scan `Notes/` for `.md`/`.snote` (skipping everything else silently), and present it as ordinary — so an old binary could write **plaintext `.md` into a future encrypted vault**. Old tags must not be rewritten. | The `format_version > CURRENT` fence protects every binary from Stage A on. For old binaries there is no format-level fix; the mitigation is **structural**: the entire encrypted object store lives under `.senatorial-notes/` (never scanned by old binaries), so old-binary writes land in a *separate* empty top-level `Notes/` tree. A current binary opening a v3 vault detects a non-empty top-level `Notes/`/`Trash/` and quarantines it (surfaces "an incompatible version wrote plaintext notes here"), never intermingling. Documented as a known hazard in README/SECURITY at Stage F. | `encrypted_vault::old_binary_plaintext_notes_are_quarantined_not_merged` (Stage D/E) + doc review |
| **R19** (audited Stage A.1) | A read-only (`InMemoryOnly`) session could still mutate the note tree → v1 manifest on disk + edited notes = drift. | `Vault.read_only` + `ensure_writable()` at the top of all 16 mutating methods; `ensure_directories` skipped for a read-only vault. Prefer an explicit flag (Stage B's "Open read-only" reuses it) over relying on filesystem permissions. | ✅ `vault_manifest::read_only_migration_*` (see R1) |

## 6. Test matrix

All 135 existing tests stay green. New/changed:

**`tests/vault_manifest.rs`** (Stage A + A.1) — **✅ 18 tests, all passing**
- v1 fixture → migrates to v2 `Ordinary`, `vault_id` + `created_at` preserved verbatim (exact timestamp checked), `migrated_from = Some(1)`, on-disk file rewritten to v2.
- v1 fixture that hand-added `kind = "encrypted"` → still migrates to `Ordinary` (v1 `kind` ignored).
- migration never touches a note file: a `.md` in the v1 vault is byte-identical and **mtime-unchanged** after open.
- fresh `Vault::create` → v2 `Ordinary`, no `migrated_from` (and it is not serialized).
- second open of a v2 vault → `Migration::NotNeeded`, `vault.toml` **bytes** unchanged (no churn).
- `format_version = 3` → `Error::UnsupportedVaultVersion { found: 3, supported: 2 }`; `vault.toml` unchanged; `Trash/`/`Attachments/` not created.
- garbage / missing-`format_version` `vault.toml` → `Error::VaultManifestCorrupt`; nothing modified.
- `kind = "encrypted"` at v2 → `Error::UnsupportedVaultKind`; tree not created.
- **read-only vault** → v1 opens `Ordinary`, `Migration::InMemoryOnly`, `is_read_only()`, no error; on-disk file still v1; `warning()` present. (Root-safe: accepts `Persisted` when euid 0.)
- **read-only session cannot mutate:** `create_note` / `create_notebook` / `move_to_trash` → `Error::VaultReadOnly`, **zero** files or directories created; the existing note is byte-unchanged.
- **read-only session does not partially upgrade the tree:** `ensure_directories` skipped → `Trash/`/`Attachments/` absent.
- **writable v1 vault still migrates normally** and a subsequent `create_note` writes a real file.
- schema: `VaultManifest` de/serializes; `VaultKind` serializes as **exactly** `kind = "ordinary"` / `kind = "encrypted"` (asserted against real `toml::to_string_pretty` lines); missing `kind` → `Ordinary`; missing `migrated_from` → `None` and omitted; unknown keys ignored; `new_ordinary()` round-trips through `write`/parse.

**`tests/vault_lock.rs`**
- acquire on a free vault → `vault.lock` written with the documented shape.
- second acquire (same process) → `VaultAlreadyOpen`.
- lock released on `Drop`; file removed.
- pid-dead lock → `Stale`; foreign-host or foreign-`boot_id` lock → `Stale`.
- a `Stale` lock is **not** deleted by `acquire`; reclaim needs explicit confirm.
- a live lock is **never** deleted.
- `Open read-only` acquires no lock; `EncryptedStorage`/`PlainStorage` writes return `VaultReadOnly`.
- two directories with the same `vault_id` lock independently (R11).

**`tests/encrypted_vault.rs`** (mirrors `tests/encryption_regressions.rs`)
- create encrypted vault → raw on-disk bytes of every file contain **none** of: a known note title, body substring, tag, or notebook name (byte-grep, like the `.snote` tests).
- correct vault password unlocks; wrong password → `DecryptionFailed`, no partial model, vault stays locked.
- flip one byte of: a file blob / the keyfile / a directory manifest → auth fails, vault refuses to open, nothing decrypted.
- swap two note blobs on disk (or a note blob for a manifest blob) → `object_uuid` / `object_type` AAD mismatch detected on read.
- take a blob from vault A, drop it into vault B at the "same" logical slot → `vault_id` AAD mismatch.
- move a note between notebooks → the blob bytes are **unchanged** (no re-encryption); only the manifest entry moves.
- parametrized: the full create / edit / rename / move / pin / archive / tag / trash / restore / permanent-delete note lifecycle produces the **same observable results** in an `Encrypted` vault as an `Ordinary` one.
- HKDF `info` label constants equal the bytes documented in the format doc's label table (R16).
- Lock Now → in-memory model gone; no plaintext bytes in any `.senatorial-notes` file; no recovery file ever created.
- attachments dir contents encrypted; `history/` + `recovery/` encrypted-or-absent.
- crash simulation between blob write and manifest write → next unlock reconciles, **zero** notes lost, orphan quarantined not deleted (R7).
- `.snote` inside an encrypted vault:
  - all 4 (vault, note) lock states behave as specified (R3);
  - peeling the outer layer yields a byte-identical valid v1 `.snote`;
  - the note's original password still unlocks the inner layer;
  - a golden pre-v0.3 `.snote` fixture still opens (R4).
- `decrypt vault` → produces an `Ordinary` vault with plaintext Markdown and working `.snote` files (R5).
- self-writes do not trigger a watcher reload (R6).
- Argon2 unlock timing within a generous bound (pattern from `encryption_regressions`).
- encrypted-vault scan at realistic scale (mirror `notebooks_tags_and_sorting_stay_responsive_at_realistic_vault_scale`).

**`tests/vault_switching.rs`** (or extend `ui_state_regressions.rs`)
- switching vault clears `unlocked_cache` + `plain_cache`, resets `flow` + `filter`, rebuilds lists.
- switching away from an unlocked encrypted vault locks + zeroizes it.
- missing recent-vault path → surfaced as unavailable, not opened, not dropped.
- per-vault last view + note UUID restored for an `Ordinary` vault.
- encrypted-vault UI state is **not** present in the plaintext app config (R14).

**`tests/ui_source_invariants.rs`** additions
- vault unlock KDF runs off the main thread (source guard for the worker-thread pattern) (R10).
- no new `.unwrap()` / `.expect(` / `panic!` / `unreachable!` in vault UI code (existing guard already covers `ui.rs`; extend to any new UI module).
- the lock file is never `remove_file`d without a liveness check or explicit-confirmation call on the same path (source guard) (R9).
- no `SignalGate` / selection-coordinator / borrow-scoping lines in `ui.rs` changed by v0.3 (diff guard — the stabilization is frozen).

**`tests/config.rs`** additions
- pre-v0.3 `config.toml` still deserializes.
- `recent_vault_meta` additive round-trip.

**`tests/fixtures/`**: checked-in golden vectors so format regressions are caught permanently.

## 7. Staging plan

Each stage is independently buildable, testable, and commit-checkpointed
(project doctrine), passes the full gate
(`fmt` / `clippy -D warnings` / `test --all-features` / `build --release` +
metadata checks), and ends with an honest limitations note. No stage modifies
the RefCell/GTK stabilization.

The stage letters below follow the **user's Stage-A definition** (2026-09-02):
Stage A is the manifest + migration work, not the multi-vault UI.

- **Stage 0 — this document + `docs/ENCRYPTED_VAULT_FORMAT.md`.** *(done)*
- **Stage A + A.1 — `vault.toml` v2 + migration + compat hardening (no crypto, no UI). ✅ done, gates green, uncommitted.**
  `src/vault_manifest.rs` (`VaultManifest`, `VaultKind`, `Migration`,
  `CURRENT_MANIFEST_VERSION`, `load` / `write` / `new_ordinary` / `manifest_path`);
  `Vault` gains `manifest` + `migration` + `read_only` and
  `kind()` / `vault_id()` / `manifest()` / `migration()` / `is_read_only()`;
  `create`/`open` migrate v1 → v2 `Ordinary` before touching the tree;
  `format_version > 2` and `kind = "encrypted"` fail safely; a non-persistable
  migration opens the vault **read-only** (`ensure_writable()` on all 16 mutating
  methods, `ensure_directories` skipped). Forward-compat policy + the
  old-binary legacy hazard documented (§2.1, R18). `tests/vault_manifest.rs`
  (18) + `tests/fixtures/vault_v1.toml`.
- **Stage B — Multi-vault UX (no crypto).** Vault switcher, Open Recent from
  `config.recent_vaults`, switch-without-restart hardening, missing-path
  handling, per-vault UI-state persistence for ordinary vaults, surface the
  Stage-A migration warning in `save_status`.
- **Stage C — Vault lock.** `src/vault_lock.rs`, the "already open" dialog,
  stale detection, read-only mode.
- **Stage D — whole-vault encryption engine.** *(implemented; see note below)*
  `crypto/vault.rs` (Argon2id → KEK → wrapped VMK → HKDF-SHA256 subkeys),
  keyfile (`vault.keys`), `SNENC` object container, `src/vault_encrypted.rs`
  (`EncryptedStore` + a single sealed manifest + reconciliation), a `Backend`
  enum on `Vault` routing every I/O method, and the full UI: create-encrypted
  flow, opens-locked + unlock screen, off-main-thread KDF (`gio::spawn_blocking`),
  Lock Now / auto-lock, `.snote`-inside semantics, encrypted-store watcher,
  search only while unlocked. Adds `hkdf` + `sha2`. `tests/encrypted_vault.rs`
  + `crypto::vault` + `vault_encrypted` unit tests.
- **Stage E — real-machine acceptance.** Arch/Hyprland acceptance pass for the
  encrypted-vault flows (extended `STABILITY_TEST_PLAN.md`).
- **Stage F — Docs, packaging, release prep.** README / SECURITY / CHANGELOG,
  `metainfo.xml` release, `0.3.0-alpha` version bump. Not tagged, not pushed —
  the user's real-machine call, as with v0.2.

**Deferred to a later release (not v0.3):** **in-place conversion of an existing
ordinary vault to encrypted (and back)** — v0.3 only *creates* encrypted vaults
and opens/switches both kinds; the format is designed so conversion can be added
safely in v0.4. Also deferred: multiple vaults open in multiple windows;
blob-size padding against size-metadata leakage; a SQLite FTS index (would have
to live encrypted inside the vault).

### Stage D implementation notes (deviations from the design above)

The engine was built as a single stage (crypto core **and** the end-to-end UI
together). Deliberate refinements to the design draft, all documented in
`docs/ENCRYPTED_VAULT_FORMAT.md`:

- **Single sealed manifest** (`store/manifest`) rather than per-directory
  `.tree` + `.notes` manifests — simpler crash story, identical privacy. The
  draft left manifest layout as a Stage-D implementation choice.
- **`created_at` stays in plaintext `vault.toml`** for both vault kinds (coarse
  metadata already disclosed in `SECURITY.md`); no encrypted `meta` blob yet
  (`k_metadata` / `k_index` subkeys derived but unused).
- **Storage abstraction is a `Backend { Plain, Encrypted(EncryptedStore) }`
  enum on `Vault`**, not a `Storage` trait — every public `Vault` method keeps
  its exact signature and dispatches on the backend.
- **Off-main-thread KDF uses `gio::spawn_blocking` + `glib::spawn_future_local`**
  (glib 0.22 removed `MainContext::channel`). `VaultKeys` is `Send`.
- **`SNENC` header is 80 bytes** (final field packing); the keyfile header is
  88 bytes as specified.
- **The old-binary plaintext-`Notes/` quarantine (R18) is not yet implemented** —
  the structural containment (encrypted store entirely under
  `.senatorial-notes/store/`) holds, but a current binary does not yet detect
  and surface a stray top-level `Notes/`. Tracked for a later stage.
- **In-place decrypt/export to an ordinary vault is not implemented** in this
  stage (was listed as a Stage-D deliverable); deferred with the conversion
  work.

## 8. Resolved decisions (2026-09-02)

All five review questions are answered; this section records them because the
HKDF labels, the AAD field set, and the `vault.toml` schema are **format-stable**
once implemented.

1. **Encrypted-vault naming — opaque.** Opaque encrypted filenames and opaque
   directory/object identifiers. Do not leak notebook names, note titles, tags,
   attachment names, vault structure, or note counts. Keep only the minimum
   plaintext to identify/open the vault: `vault_id`, `format_version`, `kind`,
   lock metadata.
2. **Key hierarchy — random VMK + HKDF-SHA256.** `password → Argon2id → KEK →
   unwrap one random 32-byte vault master key → HKDF-SHA256 subkeys` per domain
   (content, names/manifests, attachments, sensitive metadata, future index).
   Uses RustCrypto `hkdf` + `sha2`. Never `SHA-256(password)`, never
   per-object keys straight from the password, never whole-vault re-encryption
   on password change — a password change re-wraps the VMK only. Every HKDF
   `info` / domain-separation label is documented in the format spec and is
   format-stable.
3. **AEAD associated data — immutable identity only.** Bind `vault_id`,
   `object_uuid`, `object_type`, and container `format_version`. **Do not** bind
   mutable logical/filesystem paths. Moving or renaming an object inside a vault
   must not re-encrypt its payload; logical placement lives in the encrypted
   manifests. Cross-vault, cross-object, and object-type substitution must fail
   authentication.
4. **Encrypt-existing-vault conversion — deferred to v0.4.** v0.3 supports:
   creating ordinary vaults, creating encrypted vaults, opening/switching both,
   normal operation of both, and a safe decrypt/export path. No in-place mass
   conversion of an existing ordinary vault in v0.3; the format is designed to
   allow it later.
5. **Multi-vault UI — in `ui.rs`.** Keep the initial implementation in `ui.rs`;
   extract a module only if it grows large enough to justify it. Do not refactor
   unrelated stable GTK code.
