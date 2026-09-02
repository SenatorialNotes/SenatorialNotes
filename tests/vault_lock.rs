//! Stage C / C.1: the advisory vault-lock protocol.
//!
//! Core rule: a writable takeover is only offered for a `ProvenDead` lock -
//! one where the previous owner is *positively* known not to be running.
//! Everything else is `Blocked`: open read-only or cancel, never take over.

use std::fs;
use std::process::{Child, Command};

use chrono::Utc;
use senatorial_notes::Vault;
use senatorial_notes::vault_lock::{
    BlockedReason, DeadReason, LockAcquisition, LockOwner, LockStatus, VaultLock,
};
use tempfile::tempdir;
use uuid::Uuid;

fn this_hostname() -> String {
    fs::read_to_string("/proc/sys/kernel/hostname")
        .unwrap()
        .trim()
        .to_string()
}

fn this_boot_id() -> String {
    fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .unwrap()
        .trim()
        .to_string()
}

fn comm_of(pid: u32) -> String {
    fs::read_to_string(format!("/proc/{pid}/comm"))
        .unwrap()
        .trim()
        .to_string()
}

fn start_ticks_of(pid: u32) -> u64 {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    stat.rsplit_once(')')
        .unwrap()
        .1
        .split_whitespace()
        .nth(19)
        .unwrap()
        .parse()
        .unwrap()
}

fn lock_path(vault: &Vault) -> std::path::PathBuf {
    vault.state_dir().join("vault.lock")
}

fn write_fabricated_lock(vault: &Vault, owner: &LockOwner) {
    fs::write(lock_path(vault), toml::to_string_pretty(owner).unwrap()).unwrap();
}

fn read_lock(vault: &Vault) -> LockOwner {
    toml::from_str(&fs::read_to_string(lock_path(vault)).unwrap()).unwrap()
}

fn base_owner(vault: &Vault) -> LockOwner {
    LockOwner {
        format_version: 1,
        vault_id: vault.vault_id(),
        hostname: this_hostname(),
        boot_id: this_boot_id(),
        pid: std::process::id(),
        pid_start_ticks: start_ticks_of(std::process::id()),
        process_name: "senatorial-notes".to_string(),
        app_version: "0.3.0-test".to_string(),
        acquired_at: Utc::now(),
    }
}

fn new_vault(name: &str) -> (tempfile::TempDir, Vault) {
    let dir = tempdir().unwrap();
    let vault = Vault::create(dir.path().join(name)).unwrap();
    (dir, vault)
}

/// A real, signalable child process that is *not* SenatorialNotes and is *not*
/// this process. Killed on drop.
struct Sleeper(Child);

impl Sleeper {
    fn spawn() -> Self {
        let child = Command::new("sleep")
            .arg("120")
            .spawn()
            .expect("spawn `sleep`");
        let pid = child.id();
        for _ in 0..200 {
            if comm_of(pid) == "sleep" {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        Self(child)
    }
    fn pid(&self) -> u32 {
        self.0.id()
    }
    fn comm(&self) -> String {
        comm_of(self.pid())
    }
}

impl Drop for Sleeper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A PID that has definitely exited (a reaped child).
fn dead_pid() -> u32 {
    let mut child = Command::new("true").spawn().expect("spawn `true`");
    let pid = child.id();
    child.wait().expect("reap `true`");
    pid
}

fn acquired(vault: &Vault) -> VaultLock {
    match VaultLock::acquire(vault).unwrap() {
        LockAcquisition::Acquired(lock) => lock,
        other => panic!("expected Acquired, got {other:?}"),
    }
}

/// Asserts that `take_over` with *every* proven-dead reason is refused.
fn assert_never_takeover(vault: &Vault) {
    for reason in [
        DeadReason::DifferentBoot,
        DeadReason::ProcessGone,
        DeadReason::PidReused,
    ] {
        match VaultLock::take_over(vault, reason).unwrap() {
            LockAcquisition::Contended(_) => {}
            LockAcquisition::Acquired(_) => {
                panic!("take_over({reason:?}) must be refused for this lock")
            }
        }
    }
}

// --- acquire / release ------------------------------------------------------

#[test]
fn acquire_on_a_free_vault_then_release() {
    let (_dir, vault) = new_vault("V");
    assert!(matches!(VaultLock::inspect(&vault), LockStatus::Free));

    let lock = acquired(&vault);
    assert!(lock.is_owner());
    assert!(lock_path(&vault).is_file());
    assert!(matches!(
        VaultLock::inspect(&vault),
        LockStatus::HeldByThisProcess
    ));

    drop(lock);
    assert!(!lock_path(&vault).exists());
    assert!(matches!(VaultLock::inspect(&vault), LockStatus::Free));
}

#[test]
fn re_acquiring_our_own_lock_is_idempotent() {
    let (_dir, vault) = new_vault("V");
    let first = acquired(&vault);
    let second = acquired(&vault);
    assert!(second.is_owner());
    drop((first, second));
}

// --- Blocked (no takeover) -------------------------------------------------

#[test]
fn a_live_session_on_this_machine_is_blocked_not_takeover_able() {
    let (_dir, vault) = new_vault("V");
    let peer = Sleeper::spawn();
    let mut owner = base_owner(&vault);
    owner.pid = peer.pid();
    owner.pid_start_ticks = start_ticks_of(peer.pid());
    owner.process_name = peer.comm();
    write_fabricated_lock(&vault, &owner);

    assert!(matches!(
        VaultLock::inspect(&vault),
        LockStatus::Blocked {
            reason: BlockedReason::Live,
            ..
        }
    ));
    assert!(matches!(
        VaultLock::acquire(&vault).unwrap(),
        LockAcquisition::Contended(LockStatus::Blocked {
            reason: BlockedReason::Live,
            ..
        })
    ));
    assert_never_takeover(&vault);
    assert_eq!(read_lock(&vault), owner, "the peer's lock is untouched");
}

#[test]
fn a_different_hostname_is_blocked_and_can_never_be_taken_over() {
    let (_dir, vault) = new_vault("V");
    let mut owner = base_owner(&vault);
    owner.hostname = "nas-peer".to_string();
    owner.pid = dead_pid(); // even a dead-looking PID: a foreign host is never verified
    write_fabricated_lock(&vault, &owner);

    match VaultLock::inspect(&vault) {
        LockStatus::Blocked {
            reason: BlockedReason::DifferentHost,
            owner: Some(seen),
        } => {
            let explanation = BlockedReason::DifferentHost.explain(Some(&seen), &lock_path(&vault));
            assert!(explanation.contains("nas-peer"));
            assert!(explanation.to_lowercase().contains("another computer"));
        }
        other => panic!("a foreign hostname must be Blocked(DifferentHost), got {other:?}"),
    }
    assert_never_takeover(&vault);
    assert_eq!(read_lock(&vault).hostname, "nas-peer");
}

#[test]
fn an_eperm_owner_is_blocked_liveness_unknown() {
    let (_dir, vault) = new_vault("V");
    // PID 1 exists but a normal user cannot signal it -> kill(1, 0) == EPERM.
    let mut owner = base_owner(&vault);
    owner.pid = 1;
    owner.pid_start_ticks = 0;
    owner.process_name = "systemd".to_string();
    write_fabricated_lock(&vault, &owner);

    match VaultLock::inspect(&vault) {
        LockStatus::Blocked {
            reason: BlockedReason::LivenessUnknown,
            ..
        } => {}
        other => {
            panic!("an unverifiable (EPERM) owner must be Blocked(LivenessUnknown), got {other:?}")
        }
    }
    assert_never_takeover(&vault);
}

#[test]
fn a_malformed_lock_is_blocked_and_never_overwritten() {
    let (_dir, vault) = new_vault("V");
    fs::write(lock_path(&vault), "this is not a lock =====").unwrap();

    match VaultLock::inspect(&vault) {
        LockStatus::Blocked {
            reason: BlockedReason::Malformed(_),
            owner: None,
        } => {}
        other => panic!("a malformed lock must be Blocked(Malformed), got {other:?}"),
    }
    assert!(matches!(
        VaultLock::acquire(&vault).unwrap(),
        LockAcquisition::Contended(LockStatus::Blocked {
            reason: BlockedReason::Malformed(_),
            ..
        })
    ));
    assert_never_takeover(&vault);
    assert_eq!(
        fs::read_to_string(lock_path(&vault)).unwrap(),
        "this is not a lock =====",
        "a malformed lock is never modified automatically"
    );
    let explanation = BlockedReason::Malformed("bad toml".into()).explain(None, &lock_path(&vault));
    assert!(explanation.contains("read-only"));
    assert!(explanation.to_lowercase().contains("certain"));
}

#[test]
fn a_newer_lock_format_is_blocked_and_never_taken_over() {
    let (_dir, vault) = new_vault("V");
    let mut owner = base_owner(&vault);
    owner.format_version = 999;
    write_fabricated_lock(&vault, &owner);

    match VaultLock::inspect(&vault) {
        LockStatus::Blocked {
            reason: BlockedReason::NewerFormat,
            ..
        } => {}
        other => panic!("a newer lock format must be Blocked(NewerFormat), got {other:?}"),
    }
    assert_never_takeover(&vault);
}

#[test]
fn a_lock_for_another_vault_that_is_not_provably_dead_is_blocked() {
    let (_dir, vault) = new_vault("V");
    let peer = Sleeper::spawn();
    let mut owner = base_owner(&vault);
    owner.vault_id = Uuid::new_v4(); // a different vault
    owner.pid = peer.pid(); // alive -> not provably dead
    owner.pid_start_ticks = start_ticks_of(peer.pid());
    owner.process_name = peer.comm();
    write_fabricated_lock(&vault, &owner);

    match VaultLock::inspect(&vault) {
        LockStatus::Blocked {
            reason: BlockedReason::VaultIdMismatch,
            ..
        } => {}
        other => panic!("a foreign, live vault_id must be Blocked(VaultIdMismatch), got {other:?}"),
    }
    assert_never_takeover(&vault);
}

// --- ProvenDead (reviewed takeover allowed) --------------------------------

#[test]
fn a_different_boot_id_is_proven_dead_and_reclaimable() {
    let (_dir, vault) = new_vault("V");
    let mut owner = base_owner(&vault);
    owner.boot_id = Uuid::new_v4().to_string();
    owner.pid = 1; // alive, but the boot check proves death first
    write_fabricated_lock(&vault, &owner);

    assert!(matches!(
        VaultLock::inspect(&vault),
        LockStatus::ProvenDead {
            reason: DeadReason::DifferentBoot,
            ..
        }
    ));
    match VaultLock::take_over(&vault, DeadReason::DifferentBoot).unwrap() {
        LockAcquisition::Acquired(lock) => {
            assert!(lock.is_owner());
            assert!(matches!(
                VaultLock::inspect(&vault),
                LockStatus::HeldByThisProcess
            ));
        }
        other => panic!("takeover of a DifferentBoot lock must succeed, got {other:?}"),
    }
}

#[test]
fn a_gone_pid_on_the_same_boot_is_proven_dead_and_reclaimable() {
    let (_dir, vault) = new_vault("V");
    let mut owner = base_owner(&vault);
    owner.pid = dead_pid();
    write_fabricated_lock(&vault, &owner);

    assert!(matches!(
        VaultLock::inspect(&vault),
        LockStatus::ProvenDead {
            reason: DeadReason::ProcessGone,
            ..
        }
    ));
    // acquire never silently reclaims it.
    assert!(matches!(
        VaultLock::acquire(&vault).unwrap(),
        LockAcquisition::Contended(LockStatus::ProvenDead {
            reason: DeadReason::ProcessGone,
            ..
        })
    ));
    // But a reviewed takeover does.
    assert!(matches!(
        VaultLock::take_over(&vault, DeadReason::ProcessGone).unwrap(),
        LockAcquisition::Acquired(_)
    ));
}

#[test]
fn a_reused_pid_by_comm_is_proven_dead() {
    let (_dir, vault) = new_vault("V");
    let usurper = Sleeper::spawn(); // now `sleep`
    let mut owner = base_owner(&vault);
    owner.pid = usurper.pid();
    owner.pid_start_ticks = 0;
    owner.process_name = "senatorial-notes".to_string(); // the lock claims otherwise
    write_fabricated_lock(&vault, &owner);

    assert!(matches!(
        VaultLock::inspect(&vault),
        LockStatus::ProvenDead {
            reason: DeadReason::PidReused,
            ..
        }
    ));
    assert!(matches!(
        VaultLock::take_over(&vault, DeadReason::PidReused).unwrap(),
        LockAcquisition::Acquired(_)
    ));
}

#[test]
fn a_reused_pid_by_start_time_is_proven_dead() {
    let (_dir, vault) = new_vault("V");
    // A PID that IS alive and whose comm matches, but with a start time that
    // does not - a different instance took the PID.
    let peer = Sleeper::spawn();
    let mut owner = base_owner(&vault);
    owner.pid = peer.pid();
    owner.process_name = peer.comm();
    owner.pid_start_ticks = start_ticks_of(peer.pid()).wrapping_sub(1); // deliberately wrong
    write_fabricated_lock(&vault, &owner);

    assert!(matches!(
        VaultLock::inspect(&vault),
        LockStatus::ProvenDead {
            reason: DeadReason::PidReused,
            ..
        }
    ));
}

#[test]
fn a_dead_leftover_for_another_vault_can_still_be_reclaimed() {
    let (_dir, vault) = new_vault("V");
    let mut owner = base_owner(&vault);
    owner.vault_id = Uuid::new_v4();
    owner.pid = dead_pid(); // provably gone
    write_fabricated_lock(&vault, &owner);

    assert!(matches!(
        VaultLock::inspect(&vault),
        LockStatus::ProvenDead {
            reason: DeadReason::ProcessGone,
            ..
        }
    ));
    assert!(matches!(
        VaultLock::take_over(&vault, DeadReason::ProcessGone).unwrap(),
        LockAcquisition::Acquired(_)
    ));
}

// --- takeover safety ------------------------------------------------------

#[test]
fn takeover_refuses_a_reason_that_no_longer_applies() {
    let (_dir, vault) = new_vault("V");
    let mut owner = base_owner(&vault);
    owner.pid = dead_pid(); // on disk: ProcessGone
    write_fabricated_lock(&vault, &owner);

    match VaultLock::take_over(&vault, DeadReason::DifferentBoot).unwrap() {
        LockAcquisition::Contended(LockStatus::ProvenDead {
            reason: DeadReason::ProcessGone,
            ..
        }) => {}
        other => panic!("takeover must refuse a mismatched reason, got {other:?}"),
    }
}

#[test]
fn takeover_refuses_a_lock_that_became_live_while_deciding() {
    let (_dir, vault) = new_vault("V");
    let peer = Sleeper::spawn();
    let mut owner = base_owner(&vault);
    owner.pid = peer.pid();
    owner.pid_start_ticks = start_ticks_of(peer.pid());
    owner.process_name = peer.comm();
    write_fabricated_lock(&vault, &owner);

    match VaultLock::take_over(&vault, DeadReason::ProcessGone).unwrap() {
        LockAcquisition::Contended(LockStatus::Blocked {
            reason: BlockedReason::Live,
            ..
        }) => {}
        other => panic!("takeover must refuse a now-live lock, got {other:?}"),
    }
}

#[test]
fn the_crash_left_lock_takeover_path() {
    let (_dir, vault) = new_vault("V");
    let owner = {
        let lock = acquired(&vault);
        let owner = read_lock(&vault);
        std::mem::forget(lock); // the process "crashed"
        owner
    };
    let mut crashed = owner;
    crashed.pid = dead_pid();
    write_fabricated_lock(&vault, &crashed);

    assert!(matches!(
        VaultLock::inspect(&vault),
        LockStatus::ProvenDead {
            reason: DeadReason::ProcessGone,
            ..
        }
    ));
    let lock = match VaultLock::take_over(&vault, DeadReason::ProcessGone).unwrap() {
        LockAcquisition::Acquired(lock) => lock,
        other => panic!("crash-left takeover must succeed, got {other:?}"),
    };
    drop(lock);
    assert!(!lock_path(&vault).exists());
}

// --- read-only never disturbs the owner ----------------------------------

#[test]
fn a_read_only_handle_owns_nothing_and_never_touches_a_blocked_lock() {
    let (_dir, vault) = new_vault("V");
    let peer = Sleeper::spawn();
    let mut owner = base_owner(&vault);
    owner.pid = peer.pid();
    owner.pid_start_ticks = start_ticks_of(peer.pid());
    owner.process_name = peer.comm();
    write_fabricated_lock(&vault, &owner);

    {
        let ro = VaultLock::read_only();
        assert!(!ro.is_owner());
    }
    assert!(lock_path(&vault).is_file());
    assert_eq!(read_lock(&vault), owner);
}

#[test]
fn releasing_a_lock_taken_over_from_under_us_does_not_delete_the_new_owners_lock() {
    let (_dir, vault) = new_vault("V");
    let ours = acquired(&vault);

    let usurper = Sleeper::spawn();
    let mut new_owner = base_owner(&vault);
    new_owner.pid = usurper.pid();
    new_owner.pid_start_ticks = start_ticks_of(usurper.pid());
    new_owner.process_name = usurper.comm();
    new_owner.app_version = "usurper".to_string();
    write_fabricated_lock(&vault, &new_owner);

    drop(ours);
    assert_eq!(read_lock(&vault).app_version, "usurper");
}

// --- misc ----------------------------------------------------------------

#[test]
fn repeated_acquire_release_cycles_are_stable() {
    let (_dir, vault) = new_vault("V");
    for _ in 0..50 {
        drop(acquired(&vault));
        assert!(matches!(VaultLock::inspect(&vault), LockStatus::Free));
    }
}

#[test]
fn two_vaults_lock_independently() {
    let dir = tempdir().unwrap();
    let a = Vault::create(dir.path().join("A")).unwrap();
    let b = Vault::create(dir.path().join("B")).unwrap();
    let lock_a = acquired(&a);
    let lock_b = acquired(&b);
    assert!(matches!(
        VaultLock::inspect(&a),
        LockStatus::HeldByThisProcess
    ));
    assert!(matches!(
        VaultLock::inspect(&b),
        LockStatus::HeldByThisProcess
    ));
    drop((lock_a, lock_b));
}

#[test]
fn lock_file_contains_only_identity_fields() {
    let (_dir, vault) = new_vault("V");
    let text = toml::to_string_pretty(&base_owner(&vault)).unwrap();
    for key in [
        "format_version",
        "vault_id",
        "hostname",
        "boot_id",
        "pid",
        "process_name",
        "app_version",
        "acquired_at",
    ] {
        assert!(text.contains(key), "lock must record `{key}`");
    }
    for secret in ["body", "title", "password", "tags"] {
        assert!(!text.contains(secret), "lock must not contain `{secret}`");
    }
}
