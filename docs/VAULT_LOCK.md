# SenatorialNotes Advisory Vault Lock — v0.3 Stage C

Status: **design + implementation for v0.3 Stage C.**

An advisory, filesystem-level lock so **only one writable SenatorialNotes
session owns a given vault at a time**. It is independent of GTK/GIO
single-instance (which only covers one D-Bus session on one machine) and works
across users and across machines on a shared/synced vault.

It is **advisory**: it cannot physically prevent another program from writing to
the folder. It exists to stop two *SenatorialNotes* sessions from autosaving
into the same vault and to make an abandoned lock safely identifiable.

## 1. The lock file

`<vault>/.senatorial-notes/vault.lock` — TOML (consistent with `vault.toml` and
the trash records). Contains **no note content and no sensitive metadata** —
only what is needed to identify the owning session and tell a live owner from a
dead one.

```toml
format_version  = 1
vault_id        = "5c1ef9a5-7b6a-4a03-8057-85fdcbdfcfe3"  # must match vault.toml
hostname        = "fractal-arch"
boot_id         = "a1b2c3d4-5e6f-7081-9223-445566778899"  # /proc/sys/kernel/random/boot_id
pid             = 48902
pid_start_ticks = 372910                                   # /proc/<pid>/stat field 22, 0 if unknown
process_name    = "senatorial-notes"                       # expected /proc/<pid>/comm
app_version     = "0.3.0-alpha"
acquired_at     = "2026-09-02T11:21:03Z"
```

`format_version` gates the file the same way `vault.toml`'s does: a
`format_version` this build does not understand is treated as a **live** lock
(never overwritten) — a newer SenatorialNotes owns it.

## 2. Classification — `LockStatus`

**Core safety rule:** the absence of proof that a lock owner is alive is *not*
proof that it is dead. A writable takeover is offered **only** for a lock whose
owner is *positively* proven not to be running.

```
Free                    no vault.lock present
HeldByThisProcess       lock.pid == our pid && lock.boot_id == our boot_id
                          && lock.hostname == our hostname
Blocked { owner, reason }    cannot prove the owner is dead → NO takeover;
                             offer only Open Read-Only / Cancel
                             (and Show Existing Window when meaningful)
ProvenDead { owner, reason } the owner is positively known not to be running →
                             a reviewed writable takeover may be offered
```

### `DeadReason` — takeover *is* offered after confirmation

| reason | proof |
| --- | --- |
| `DifferentBoot` | same host, a *different* `boot_id` — the machine restarted since the lock was taken, so no process from that session can still exist |
| `ProcessGone` | same host + same boot, `kill(pid, 0)` → `ESRCH` |
| `PidReused` | same host + same boot, the PID is alive but its identity proves a different process: `/proc/<pid>/comm` ≠ the recorded `process_name`, **or** a known `pid_start_ticks` differs from the current one |

### `BlockedReason` — takeover is **never** offered

| reason | why it is not proof of death |
| --- | --- |
| `Live` | same host + boot, the PID is alive and its identity is consistent — it *is* running |
| `DifferentHost` | a different hostname. **This may be a live SenatorialNotes session on another computer editing this vault over NFS / SMB / a NAS.** Liveness cannot be checked from here, so it is never treated as dead. |
| `LivenessUnknown` | `kill(pid, 0)` → `EPERM` (the process runs as another user), `/proc` unreadable, an unexpected errno, or this session cannot read its own host/boot identity |
| `NewerFormat` | `lock.format_version` newer than this build — a newer SenatorialNotes owns it |
| `Malformed(detail)` | the lock file could not be parsed → ownership unknown |
| `Unreadable` | the lock file is present but could not be read |
| `VaultIdMismatch` | `lock.vault_id` ≠ this vault's, **and** the owning process could not be proven dead (a proven-dead leftover from another vault is `ProvenDead(...)` instead and *can* be reclaimed) |

## 3. Liveness verdict

A PID number is meaningless on another machine or across a reboot, so the check
is layered:

```
1. lock.hostname != our hostname   (both known)  -> Blocked(DifferentHost)   # could be a live NFS peer
2. our hostname or lock.hostname unknown         -> Blocked(LivenessUnknown)
3. lock.boot_id != our boot_id     (both known)  -> ProvenDead(DifferentBoot) # machine restarted
4. our boot_id or lock.boot_id unknown           -> Blocked(LivenessUnknown)
5. kill(lock.pid, 0):
     ESRCH                          -> ProvenDead(ProcessGone)
     EPERM / other errno / n/a      -> Blocked(LivenessUnknown)
     Ok  -> identity_proves_reuse(pid)?           # comm mismatch OR start-time mismatch
              yes -> ProvenDead(PidReused)
              no  -> Blocked(Live)

Then: if lock.vault_id != vault.vault_id
        proven-dead verdict -> ProvenDead(...)     # reclaimable leftover
        blocked verdict     -> Blocked(VaultIdMismatch)
```

`NewerFormat`, `Malformed`, `Unreadable` are decided before the liveness check
(the file cannot be trusted), and are always `Blocked`.

**PID alone never proves anything.** A `Live` verdict requires the `boot_id`
match *and* a consistent process identity; a `PidReused` verdict requires a
*positive* identity mismatch, not merely a failure to confirm.

## 4. API (`src/vault_lock.rs`)

```rust
pub struct VaultLock { /* private: Option<lock path> + whether we own it */ }

pub enum LockAcquisition {
    /// We are now the writable owner (freshly acquired or already ours).
    Acquired(VaultLock),
    /// Could not acquire; `status` is `Blocked` or `ProvenDead`.
    Contended(LockStatus),
}

impl VaultLock {
    /// Classify the lock without touching it.
    pub fn inspect(vault: &Vault) -> LockStatus;

    /// Try to acquire the writable lock. `Free` / `HeldByThisProcess` succeed;
    /// `Blocked` and `ProvenDead` both return `Contended` (a proven-dead lock
    /// is only reclaimed by `take_over`, after review).
    pub fn acquire(vault: &Vault) -> Result<LockAcquisition>;

    /// Reclaim a **proven-dead** lock after the user reviewed
    /// `confirmed_reason`. Returns `Contended` if the on-disk lock is no longer
    /// `ProvenDead` for exactly that `DeadReason` (e.g. a live session appeared,
    /// or the reason changed). Writes our lock, then re-reads and verifies it
    /// is ours.
    pub fn take_over(vault: &Vault, confirmed_reason: DeadReason) -> Result<LockAcquisition>;

    /// A non-owning handle for a read-only session. Owns nothing; `Drop` is a
    /// no-op; it can never remove or replace the writable owner's lock.
    pub fn read_only() -> VaultLock;

    pub fn is_owner(&self) -> bool;
    pub fn release(self);   // explicit; Drop does the same
}

impl Drop for VaultLock {
    // Removes the file only if `is_owner` AND the file still contains *our*
    // identity (never deletes a lock that was taken over from under us, nor
    // anyone else's). Best-effort; errors ignored.
}
```

Acquire/take-over write atomically: temp sibling `.vault.lock.<uuid>.tmp`
(mode 0600) → `fsync` → `rename_no_replace` (acquire, `Free` case) or atomic
`rename` (take-over, replacing the proven-dead file) → **re-read and verify the
lock is ours**; if not, we lost a race → release and report `Contended`.
`take_over` **never** overwrites a `Blocked` lock — a malformed / newer-format /
different-host / unverifiable lock is left exactly as found.

## 5. UI behaviour

All dialogs are a modal `gtk::AlertDialog` (same pattern as the delete-notebook
confirm). In every case, until the user chooses, the current vault/session is
untouched; **Open Read-Only** always uses `VaultLock::read_only()` (a non-owning
handle) so the blocked owner's lock file is never modified.

### `ProvenDead` — takeover offered

Detail explains the `DeadReason` in plain language, then:

Buttons: **Cancel** · **Open Read-Only** · **Take Over**. Take Over calls
`VaultLock::take_over(vault, reason)`; if it comes back `Contended` (the lock
changed while the dialog was open) that is surfaced, never forced.

### `Blocked` — **no** takeover

- `BlockedReason::Live` (a live session on this machine): **Cancel** ·
  **Open Read-Only** · **Show Existing Window** (best-effort
  `Gio::Application::activate`; then the switch is abandoned).
- every other `BlockedReason` (`DifferentHost`, `LivenessUnknown`,
  `NewerFormat`, `Malformed`, `Unreadable`, `VaultIdMismatch`): **Cancel** ·
  **Open Read-Only** only. No third button. The detail explains why the lock
  cannot be verified; for `Malformed` it adds that the user may remove the lock
  file by hand *only if certain no other SenatorialNotes session owns the
  vault*.

## 6. Lifecycle ordering (in `open_vault` / `commit_vault_switch`)

1. `Vault::open(path)` — validate. On failure the current session is untouched.
2. `VaultLock::acquire(&new_vault)`:
   - `Acquired(lock)` → straight to step 3 (commit).
   - `Contended(status)` → if we were opening read-only anyway, `commit` with
     `VaultLock::read_only()`; otherwise the contention dialog (§5). Its outcome:
     - *Open read-only* → `commit` with `VaultLock::read_only()`, `read_only = true`.
     - *Take over* (`ProvenDead` only) → `VaultLock::take_over`; on `Acquired` →
       `commit`, on `Contended` → "the lock changed while the dialog was open",
       abort, current session untouched.
     - *Show existing window* (`Blocked(Live)` only) / *Cancel* → abort, current
       session untouched.
3. **commit_vault_switch** (only reached with a decided lock/mode):
   1. `prepare_to_leave_active` — flush the outgoing vault's active note. **If
      this fails**, `drop` the just-acquired new lock, keep the old vault + old
      lock, show a message. *(release the old lock only after a successful flush)*
   2. `persist_vault_session_state` for the outgoing vault.
   3. `state.vault_lock.take()` → `Drop` **releases the old writable lock** — now
      that the outgoing vault is flushed and its session state saved.
   4. `clear_sensitive_documents`, `cancel_all_timers`, `cancel_pending_selection`,
      `cancel_editor_deferrals`, `widgets.sessions.bump()`.
   5. swap in `vault`, the new `VaultLock`, `read_only`, rebuild lists, restore
      view/note/scroll (Stage B).

**Failure to acquire the new lock never touches the old session** — steps 3.x
are not reached.

### Normal exit (`connect_close_request`)

After `persist_active` succeeds and `clear_sensitive_documents` runs, `state`
is dropped when the window closes → `AppState.vault_lock`'s `Drop` **removes our
lock file cleanly**.

### Crash / kill

The process dies without running `Drop` → the lock file remains. A later session
classifies it as `ProvenDead` — `ProcessGone` if the PID is now free on the same
boot, `PidReused` if the PID was recycled, `DifferentBoot` after a reboot — and
offers a reviewed takeover. If that later session runs as a *different user* and
cannot signal the (still-live-looking) PID, it sees `Blocked(LivenessUnknown)`
instead and offers only read-only — it will not steal a lock it cannot verify.

## 7. What Stage C does not do

No encrypted-vault engine, no `vault.keys`, no HKDF, no `SNENC`, no `.snote`
change, no `vault.toml` schema change, no change to Stage A/B behaviour beyond
adding `AppState.vault_lock` and the acquire/release calls, and no re-entrancy
change: `VaultLock` is pure filesystem I/O with no GTK and no `AppState` borrow
inside it.
