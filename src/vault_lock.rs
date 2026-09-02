//! Advisory, filesystem-level vault lock — v0.3 Stage C / C.1.
//!
//! Only one *writable* SenatorialNotes session may own a given vault at a time.
//! Independent of GTK/GIO single-instance (which only covers one D-Bus session
//! on one machine); works across users and machines on a shared or synced
//! vault. It is **advisory**: it does not physically prevent other programs
//! from writing, it stops two SenatorialNotes sessions from autosaving into the
//! same vault and makes an abandoned lock safely identifiable.
//!
//! **Core safety rule (Stage C.1):** the absence of proof that a lock owner is
//! alive is *not* proof that it is dead. A writable takeover is only ever
//! offered for a [`LockStatus::ProvenDead`] lock - one where SenatorialNotes
//! can *positively* establish that the previous owner is no longer running.
//! Everything else is [`LockStatus::Blocked`]: the user may open read-only or
//! cancel, but never take over.
//!
//! The full protocol is documented in `docs/VAULT_LOCK.md`. This module is
//! pure: filesystem I/O only, no GTK, and it never borrows the application
//! model.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::constants::BINARY_NAME;
use crate::error::io_error;
use crate::storage::atomic::rename_no_replace;
use crate::vault::Vault;
use crate::{Error, Result};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

const LOCK_FILE: &str = "vault.lock";
const CURRENT_LOCK_VERSION: u32 = 1;

/// Contents of `<vault>/.senatorial-notes/vault.lock`. No note content, no
/// sensitive metadata — only what identifies the owning session and lets a
/// later session tell a live owner from a dead one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockOwner {
    pub format_version: u32,
    pub vault_id: Uuid,
    pub hostname: String,
    pub boot_id: String,
    pub pid: u32,
    #[serde(default)]
    pub pid_start_ticks: u64,
    pub process_name: String,
    pub app_version: String,
    pub acquired_at: DateTime<Utc>,
}

impl LockOwner {
    fn for_this_process(vault_id: Uuid) -> Self {
        let (hostname, boot_id, pid) = this_process_identity();
        Self {
            format_version: CURRENT_LOCK_VERSION,
            vault_id,
            hostname,
            boot_id,
            pid,
            pid_start_ticks: process_start_ticks(pid).unwrap_or(0),
            process_name: BINARY_NAME.to_string(),
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            acquired_at: Utc::now(),
        }
    }

    /// A short, non-secret description for the contention dialog.
    pub fn describe(&self) -> String {
        format!(
            "{} · PID {} · since {} · SenatorialNotes {}",
            if self.hostname.is_empty() {
                "an unknown host"
            } else {
                self.hostname.as_str()
            },
            self.pid,
            self.acquired_at.format("%Y-%m-%d %H:%M"),
            self.app_version,
        )
    }
}

/// Why a lock's owner is **positively proven** not to be running. Only these
/// permit a reviewed writable takeover.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeadReason {
    /// Same host, a different `boot_id` — the machine has restarted since the
    /// lock was taken, so no process from that session can still exist.
    DifferentBoot,
    /// Same host and boot, `kill(pid, 0)` reported no such process (`ESRCH`).
    ProcessGone,
    /// Same host and boot, the PID exists but its identity (`/proc/<pid>/comm`,
    /// or a known process start time) proves it is a different process — the
    /// original session is gone and the PID was recycled.
    PidReused,
}

impl DeadReason {
    /// A one-line explanation for the takeover dialog.
    pub fn explain(&self, owner: &LockOwner) -> String {
        match self {
            DeadReason::DifferentBoot => {
                "The session that locked this vault ended when this computer last restarted."
                    .to_string()
            }
            DeadReason::ProcessGone => format!(
                "The session that locked this vault (PID {}) is no longer running.",
                owner.pid
            ),
            DeadReason::PidReused => format!(
                "The lock points at PID {}, which now belongs to an unrelated process - the \
                 session that took the lock has ended.",
                owner.pid
            ),
        }
    }
}

/// Why a lock **cannot be taken over** — SenatorialNotes cannot prove the
/// previous owner is dead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlockedReason {
    /// Same host and boot, the owning process is alive and its identity is
    /// consistent — it is running.
    Live,
    /// A different hostname. This may be a **live** SenatorialNotes session on
    /// another computer editing this vault over NFS / SMB / a NAS. Liveness
    /// cannot be checked from here, so it is never treated as dead.
    DifferentHost,
    /// Liveness could not be established: `kill(pid, 0)` returned `EPERM` (the
    /// process runs as another user), `/proc` could not be read, or this
    /// session could not read its own host/boot identity.
    LivenessUnknown,
    /// The lock file's `format_version` is newer than this build understands —
    /// a newer SenatorialNotes owns it.
    NewerFormat,
    /// The lock file could not be parsed; ownership is unknown.
    Malformed(String),
    /// The lock file is present but could not be read.
    Unreadable,
    /// The lock's `vault_id` does not match this vault, and the owning process
    /// could not be proven dead — another session may be involved.
    VaultIdMismatch,
}

impl BlockedReason {
    /// A one-line explanation for the read-only / cancel dialog.
    pub fn explain(&self, owner: Option<&LockOwner>, lock_path: &Path) -> String {
        match self {
            BlockedReason::Live => format!(
                "\u{201c}This vault is open in another session on this computer ({}).\u{201d}",
                owner.map(LockOwner::describe).unwrap_or_default()
            ),
            BlockedReason::DifferentHost => format!(
                "This vault is locked by {}. That may be a SenatorialNotes session on another \
                 computer editing this vault over a network share - SenatorialNotes cannot check \
                 whether it is still running, so it will not take the lock.",
                owner
                    .map(|owner| owner.hostname.clone())
                    .filter(|host| !host.is_empty())
                    .unwrap_or_else(|| "another computer".to_string()),
            ),
            BlockedReason::LivenessUnknown => {
                "This vault has a lock, but SenatorialNotes cannot verify whether the session that \
                 holds it is still running."
                    .to_string()
            }
            BlockedReason::NewerFormat => {
                "This vault's lock was written by a newer version of SenatorialNotes.".to_string()
            }
            BlockedReason::Malformed(detail) => format!(
                "This vault's lock file is unreadable ({detail}), so SenatorialNotes cannot tell \
                 whether another session owns it. You can open the vault read-only. Only if you \
                 are certain no other SenatorialNotes session is using this vault should you \
                 remove {} yourself and try again.",
                lock_path.display()
            ),
            BlockedReason::Unreadable => format!(
                "This vault's lock file ({}) could not be read, so ownership cannot be \
                 established.",
                lock_path.display()
            ),
            BlockedReason::VaultIdMismatch => {
                "This folder contains a lock that refers to a different vault, and the process it \
                 names could not be proven to have exited. Another SenatorialNotes session may be \
                 involved."
                    .to_string()
            }
        }
    }
}

/// Classification of a vault's lock file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LockStatus {
    /// No `vault.lock` present.
    Free,
    /// The lock is held by *this* process.
    HeldByThisProcess,
    /// A lock that **cannot** be taken over — the previous owner cannot be
    /// proven dead. Offer only "Open Read-Only" and "Cancel".
    Blocked {
        owner: Option<LockOwner>,
        reason: BlockedReason,
    },
    /// A lock whose owner is **positively proven** not to be running. A reviewed
    /// writable takeover may be offered.
    ProvenDead {
        owner: LockOwner,
        reason: DeadReason,
    },
}

/// The result of an acquire attempt.
#[derive(Debug)]
pub enum LockAcquisition {
    /// This process is now the writable owner.
    Acquired(VaultLock),
    /// Could not acquire; `status` is `Blocked` or `ProvenDead` and the caller
    /// decides what to offer the user.
    Contended(LockStatus),
}

/// A held (or non-owning read-only) vault lock. Dropping an owned lock releases
/// it.
#[derive(Debug)]
pub struct VaultLock {
    /// `None` for a read-only handle, which owns nothing.
    path: Option<PathBuf>,
    owner: bool,
}

impl VaultLock {
    /// A non-owning handle for a read-only session. It records nothing, its
    /// `Drop` is a no-op, and it can never remove or replace the writable
    /// owner's lock.
    pub fn read_only() -> Self {
        Self {
            path: None,
            owner: false,
        }
    }

    pub fn is_owner(&self) -> bool {
        self.owner
    }

    /// Explicitly release the lock now (equivalent to dropping it).
    pub fn release(self) {}

    fn lock_path(vault: &Vault) -> PathBuf {
        vault.state_dir().join(LOCK_FILE)
    }

    /// Classify the vault's lock without touching it.
    pub fn inspect(vault: &Vault) -> LockStatus {
        classify(vault, &Self::lock_path(vault))
    }

    /// Try to become the writable owner. `Free` and `HeldByThisProcess`
    /// succeed; `Blocked` and `ProvenDead` both return `Contended` — a
    /// proven-dead lock is only reclaimed by [`take_over`](VaultLock::take_over)
    /// after review.
    pub fn acquire(vault: &Vault) -> Result<LockAcquisition> {
        let lock_path = Self::lock_path(vault);
        match classify(vault, &lock_path) {
            LockStatus::HeldByThisProcess => Ok(LockAcquisition::Acquired(Self {
                path: Some(lock_path),
                owner: true,
            })),
            LockStatus::Free => Self::write_and_verify(vault, &lock_path, false),
            contended @ (LockStatus::Blocked { .. } | LockStatus::ProvenDead { .. }) => {
                Ok(LockAcquisition::Contended(contended))
            }
        }
    }

    /// Reclaim a **proven-dead** lock after the user has reviewed
    /// `confirmed_reason`. Refuses (`Contended`) if the on-disk lock is no
    /// longer proven dead for that reason (e.g. it changed, or a live session
    /// appeared while the user was deciding).
    pub fn take_over(vault: &Vault, confirmed_reason: DeadReason) -> Result<LockAcquisition> {
        let lock_path = Self::lock_path(vault);
        match classify(vault, &lock_path) {
            LockStatus::Free => Self::write_and_verify(vault, &lock_path, false),
            LockStatus::ProvenDead { reason, .. } if reason == confirmed_reason => {
                Self::write_and_verify(vault, &lock_path, true)
            }
            other => Ok(LockAcquisition::Contended(other)),
        }
    }

    /// Writes our lock (`replace` = overwrite an existing proven-dead file),
    /// then re-reads and confirms it is ours before returning `Acquired`.
    fn write_and_verify(vault: &Vault, lock_path: &Path, replace: bool) -> Result<LockAcquisition> {
        let dir = vault.state_dir();
        let owner = LockOwner::for_this_process(vault.vault_id());
        let text = toml::to_string_pretty(&owner)
            .map_err(|error| Error::Configuration(error.to_string()))?;

        let temp = dir.join(format!(".{LOCK_FILE}.{}.tmp", Uuid::new_v4().simple()));
        let write_temp = || -> Result<()> {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            let mut file = options
                .open(&temp)
                .map_err(|source| io_error(&temp, source))?;
            file.write_all(text.as_bytes())
                .map_err(|source| io_error(&temp, source))?;
            file.sync_all().map_err(|source| io_error(&temp, source))?;
            Ok(())
        };
        if let Err(error) = write_temp() {
            let _ = fs::remove_file(&temp);
            return Err(error);
        }

        let placed = if replace {
            fs::rename(&temp, lock_path).map_err(|source| io_error(lock_path, source))
        } else {
            rename_no_replace(&temp, lock_path)
        };
        if let Err(error) = placed {
            let _ = fs::remove_file(&temp);
            return match &error {
                Error::AlreadyExists(_) => {
                    Ok(LockAcquisition::Contended(classify(vault, lock_path)))
                }
                _ => Err(error),
            };
        }
        sync_directory(&dir);

        match classify(vault, lock_path) {
            LockStatus::HeldByThisProcess => Ok(LockAcquisition::Acquired(Self {
                path: Some(lock_path.to_path_buf()),
                owner: true,
            })),
            // Lost a race between our write and our verify.
            other => Ok(LockAcquisition::Contended(other)),
        }
    }
}

impl Drop for VaultLock {
    fn drop(&mut self) {
        if !self.owner {
            return;
        }
        let Some(path) = self.path.as_deref() else {
            return;
        };
        // Only remove the file if it *still* holds our identity - never delete a
        // lock that was taken over from under us, and never anyone else's.
        let Ok(text) = fs::read_to_string(path) else {
            return;
        };
        let Ok(owner) = toml::from_str::<LockOwner>(&text) else {
            return;
        };
        let (hostname, boot_id, pid) = this_process_identity();
        if owner.pid == pid
            && owner.boot_id == boot_id
            && owner.hostname == hostname
            && !hostname.is_empty()
        {
            let _ = fs::remove_file(path);
        }
    }
}

/// Whether the owner is proven dead, or merely unverifiable.
enum Verdict {
    Dead(DeadReason),
    Blocked(BlockedReason),
}

/// Reads and classifies `lock_path` for `vault`. Infallible.
fn classify(vault: &Vault, lock_path: &Path) -> LockStatus {
    let text = match fs::read_to_string(lock_path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return LockStatus::Free,
        Err(_) => {
            return LockStatus::Blocked {
                owner: None,
                reason: BlockedReason::Unreadable,
            };
        }
    };

    let owner: LockOwner = match toml::from_str(&text) {
        Ok(owner) => owner,
        Err(error) => {
            return LockStatus::Blocked {
                owner: None,
                reason: BlockedReason::Malformed(error.to_string()),
            };
        }
    };

    if owner.format_version > CURRENT_LOCK_VERSION {
        return LockStatus::Blocked {
            owner: Some(owner),
            reason: BlockedReason::NewerFormat,
        };
    }

    let (our_host, our_boot, our_pid) = this_process_identity();

    // Our own live PID on our own host+boot is tautologically us.
    if !our_host.is_empty()
        && owner.hostname == our_host
        && !our_boot.is_empty()
        && owner.boot_id == our_boot
        && owner.pid == our_pid
    {
        return LockStatus::HeldByThisProcess;
    }

    let verdict = liveness_verdict(&owner, &our_host, &our_boot);
    let vault_matches = owner.vault_id == vault.vault_id();

    match (verdict, vault_matches) {
        // A proven-dead process is a stale leftover whether or not its recorded
        // vault_id matches - reclaiming it cannot disturb anything.
        (Verdict::Dead(reason), _) => LockStatus::ProvenDead { owner, reason },
        // Not provably dead + wrong vault: another session may be involved.
        (Verdict::Blocked(_), false) => LockStatus::Blocked {
            owner: Some(owner),
            reason: BlockedReason::VaultIdMismatch,
        },
        (Verdict::Blocked(reason), true) => LockStatus::Blocked {
            owner: Some(owner),
            reason,
        },
    }
}

/// Can we *positively* establish that `owner`'s process is no longer running?
fn liveness_verdict(owner: &LockOwner, our_host: &str, our_boot: &str) -> Verdict {
    // A different, known hostname could be a live network peer.
    if !owner.hostname.is_empty() && !our_host.is_empty() && owner.hostname != our_host {
        return Verdict::Blocked(BlockedReason::DifferentHost);
    }
    // Cannot even establish that this is the same machine.
    if owner.hostname.is_empty() || our_host.is_empty() {
        return Verdict::Blocked(BlockedReason::LivenessUnknown);
    }

    // Same host. A different, known boot means the machine restarted - proof
    // the owner is gone.
    if !owner.boot_id.is_empty() && !our_boot.is_empty() && owner.boot_id != our_boot {
        return Verdict::Dead(DeadReason::DifferentBoot);
    }
    if owner.boot_id.is_empty() || our_boot.is_empty() {
        return Verdict::Blocked(BlockedReason::LivenessUnknown);
    }

    // Same host, same boot: a PID check is meaningful.
    match kill_probe(owner.pid) {
        ProbeResult::NoSuchProcess => Verdict::Dead(DeadReason::ProcessGone),
        ProbeResult::CannotVerify => Verdict::Blocked(BlockedReason::LivenessUnknown),
        ProbeResult::Alive => {
            if identity_proves_reuse(owner.pid, owner) {
                Verdict::Dead(DeadReason::PidReused)
            } else {
                Verdict::Blocked(BlockedReason::Live)
            }
        }
    }
}

fn sync_directory(path: &Path) {
    #[cfg(unix)]
    if let Ok(directory) = fs::File::open(path) {
        let _ = directory.sync_all();
    }
    #[cfg(not(unix))]
    let _ = path;
}

fn this_process_identity() -> (String, String, u32) {
    (read_hostname(), read_boot_id(), std::process::id())
}

fn read_hostname() -> String {
    fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|value| value.trim().to_string())
        .unwrap_or_default()
}

fn read_boot_id() -> String {
    fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()
        .map(|value| value.trim().to_string())
        .unwrap_or_default()
}

enum ProbeResult {
    Alive,
    NoSuchProcess,
    /// `EPERM`, an unexpected errno, or a platform where this cannot be checked.
    CannotVerify,
}

#[cfg(unix)]
fn kill_probe(pid: u32) -> ProbeResult {
    // SAFETY: `kill` with signal 0 performs only an existence/permission check
    // and delivers no signal.
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if result == 0 {
        return ProbeResult::Alive;
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(code) if code == libc::ESRCH => ProbeResult::NoSuchProcess,
        _ => ProbeResult::CannotVerify,
    }
}

#[cfg(not(unix))]
fn kill_probe(_pid: u32) -> ProbeResult {
    ProbeResult::CannotVerify
}

/// `true` only when the process at `pid` can be **positively** shown to be a
/// different process than the one that took the lock.
fn identity_proves_reuse(pid: u32, owner: &LockOwner) -> bool {
    // A mismatched `comm` is definitive proof of a different process.
    if let Some(comm) = read_proc_comm(pid)
        && !comm.is_empty()
        && !comm_matches(&comm, owner)
    {
        return true;
    }
    // A different, known start time is definitive proof of a different instance
    // - even if the `comm` happens to match (another SenatorialNotes).
    if owner.pid_start_ticks != 0
        && let Some(current) = process_start_ticks(pid)
        && current != 0
        && current != owner.pid_start_ticks
    {
        return true;
    }
    false
}

/// Whether `comm` (from `/proc/<pid>/comm`, truncated to 15 chars) is
/// consistent with the process name the lock recorded.
fn comm_matches(comm: &str, owner: &LockOwner) -> bool {
    let expected = if owner.process_name.is_empty() {
        BINARY_NAME
    } else {
        owner.process_name.as_str()
    };
    comm == expected || (comm.len() >= 10 && expected.starts_with(comm))
}

fn read_proc_comm(pid: u32) -> Option<String> {
    fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|value| value.trim().to_string())
}

/// `/proc/<pid>/stat` field 22 (`starttime`, in clock ticks since boot). The
/// `comm` field can contain spaces and parentheses, so everything up to the
/// final `)` is skipped first.
fn process_start_ticks(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rsplit_once(')')?.1;
    // Post-`)` fields: state(3) ppid(4) ... starttime(22) -> index 19.
    after_comm.split_whitespace().nth(19)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_ticks_parser_handles_a_comm_with_spaces_and_parens() {
        let line = "1234 ((weird ) name)) S 1 1234 1234 0 -1 4194304 100 0 0 0 5 3 0 0 20 0 1 0 987654 12345 0";
        let after = line.rsplit_once(')').unwrap().1;
        let starttime: u64 = after.split_whitespace().nth(19).unwrap().parse().unwrap();
        assert_eq!(starttime, 987654);
    }

    #[test]
    fn comm_matches_tolerates_the_15_char_truncation() {
        let owner = LockOwner {
            format_version: 1,
            vault_id: Uuid::nil(),
            hostname: String::new(),
            boot_id: String::new(),
            pid: 0,
            pid_start_ticks: 0,
            process_name: "senatorial-notes".to_string(),
            app_version: String::new(),
            acquired_at: Utc::now(),
        };
        assert!(comm_matches("senatorial-notes", &owner));
        assert!(comm_matches("senatorial-note", &owner)); // truncated
        assert!(!comm_matches("sleep", &owner));
        assert!(!comm_matches("bash", &owner));
    }

    #[test]
    fn read_only_lock_owns_nothing_and_dropping_it_is_a_no_op() {
        let lock = VaultLock::read_only();
        assert!(!lock.is_owner());
        drop(lock);
    }
}
