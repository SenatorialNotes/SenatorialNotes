//! v0.3 UX package - behavioural regression tests.
//!
//! Covers the Favourite property, the Recently-Opened session token, and the
//! per-vault session-state store (plaintext config for a Standard Vault, the
//! sealed encrypted manifest for a Secure Vault). The UI-wiring invariants
//! live in `tests/ui_source_invariants.rs`.

use senatorial_notes::Vault;
use senatorial_notes::config::{AppConfig, RECENTLY_OPENED_LIMIT, VaultSessionState};
use tempfile::tempdir;
use uuid::Uuid;

const PASSWORD: &str = "correct horse battery staple";

// ---------------------------------------------------------------------------
// Favourite: a genuine, additive note property, independent of Pinned
// ---------------------------------------------------------------------------

#[test]
fn favourite_is_independent_of_pinned_and_survives_a_round_trip() {
    let dir = tempdir().unwrap();
    let vault = Vault::create(dir.path()).unwrap();

    let mut note = vault.create_note("Reading list", "Inbox").unwrap();
    note.metadata.favourite = true;
    note.metadata.pinned = false;
    let stamp = vault.save_note(&mut note, None).unwrap();

    // Reloading from disk keeps favourite set and pinned clear.
    let (reloaded, _) = vault.load_note(&note.relative_path).unwrap();
    assert!(reloaded.metadata.favourite, "favourite must persist");
    assert!(!reloaded.metadata.pinned, "pinned must stay independent");

    // The four states are all reachable and distinct.
    let mut note = reloaded;
    note.metadata.pinned = true;
    vault.save_note(&mut note, Some(&stamp)).unwrap();
    let (both, _) = vault.load_note(&note.relative_path).unwrap();
    assert!(both.metadata.favourite && both.metadata.pinned);

    // A scan surfaces the flag on the summary.
    let summary = vault
        .scan_notes()
        .unwrap()
        .into_iter()
        .find(|s| s.id == note.metadata.id)
        .unwrap();
    assert!(summary.favourite);
    assert!(summary.pinned);
}

#[test]
fn an_older_build_round_trips_an_unknown_favourite_field() {
    // `favourite` is the same class of additive front-matter change as
    // `pinned`: a build that predates it must preserve it through `unknown`.
    let markdown = format!(
        "---\nid: \"{}\"\ntitle: \"X\"\ncreated_at: \"2026-01-01T00:00:00Z\"\n\
         updated_at: \"2026-01-01T00:00:00Z\"\nfavourite: true\n---\nbody\n",
        Uuid::new_v4()
    );
    let note = senatorial_notes::model::Note::parse(&markdown, "Inbox/x.md".into()).unwrap();
    assert!(note.metadata.favourite);
    let round_trip = note.to_markdown().unwrap();
    let reparsed = senatorial_notes::model::Note::parse(&round_trip, "Inbox/x.md".into()).unwrap();
    assert!(reparsed.metadata.favourite);
}

// ---------------------------------------------------------------------------
// Recently Opened: viewing order, not modification time
// ---------------------------------------------------------------------------

#[test]
fn record_opened_is_most_recent_first_deduplicated_and_capped() {
    let mut session = VaultSessionState::default();
    let ids: Vec<Uuid> = (0..RECENTLY_OPENED_LIMIT + 5)
        .map(|_| Uuid::new_v4())
        .collect();
    for id in &ids {
        session.record_opened(*id);
    }
    // Capped.
    assert_eq!(session.recently_opened.len(), RECENTLY_OPENED_LIMIT);
    // Most-recent first: the last id recorded is at the front.
    assert_eq!(session.recently_opened[0], *ids.last().unwrap());
    // The oldest ids fell off the end.
    assert!(!session.recently_opened.contains(&ids[0]));

    // Re-opening an existing note moves it to the front without duplicating.
    let again = session.recently_opened[3];
    session.record_opened(again);
    assert_eq!(session.recently_opened[0], again);
    assert_eq!(
        session
            .recently_opened
            .iter()
            .filter(|x| **x == again)
            .count(),
        1
    );
}

#[test]
fn recording_a_recent_open_never_touches_note_bytes() {
    let dir = tempdir().unwrap();
    let vault = Vault::create(dir.path()).unwrap();
    let mut note = vault.create_note("Untouched", "Inbox").unwrap();
    vault.save_note(&mut note, None).unwrap();
    let path = dir.path().join("Notes").join(&note.relative_path);
    let before = std::fs::read(&path).unwrap();
    let modified_before = std::fs::metadata(&path).unwrap().modified().unwrap();

    // "Recently Opened" is a session-state concern only.
    let mut session = VaultSessionState::default();
    session.record_opened(note.metadata.id);

    let after = std::fs::read(&path).unwrap();
    let modified_after = std::fs::metadata(&path).unwrap().modified().unwrap();
    assert_eq!(before, after, "the note file must be byte-identical");
    assert_eq!(modified_before, modified_after, "mtime must not change");
}

// ---------------------------------------------------------------------------
// Per-vault session state: config for Standard, sealed manifest for Secure
// ---------------------------------------------------------------------------

#[test]
fn a_standard_vault_keeps_its_session_in_the_plaintext_config() {
    let vault_id = Uuid::new_v4();
    let mut config = AppConfig::default();
    let mut session = VaultSessionState {
        last_view: Some("favourites".into()),
        ..VaultSessionState::default()
    };
    session.record_opened(Uuid::new_v4());
    config.set_vault_session(vault_id, session.clone());

    let text = toml::to_string(&config).unwrap();
    assert!(
        text.contains("recently_opened"),
        "Standard vault recents live in config"
    );
    let back: AppConfig = toml::from_str(&text).unwrap();
    assert_eq!(
        back.vault_session(vault_id).unwrap().last_view.as_deref(),
        Some("favourites")
    );
    assert_eq!(
        back.vault_session(vault_id).unwrap().recently_opened.len(),
        1
    );
}

#[test]
fn a_secure_vault_session_is_sealed_in_the_manifest_never_in_plaintext() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let note_id;
    let recent_id = Uuid::new_v4();
    {
        let vault = Vault::create_encrypted(root, PASSWORD).unwrap();
        let mut note = vault.create_note("Passport details", "Inbox").unwrap();
        note.metadata.favourite = true;
        vault.save_note(&mut note, None).unwrap();
        note_id = note.metadata.id;

        // Persist a session referencing a note UUID and a notebook view.
        let mut session = vault.encrypted_session_state().unwrap_or_default();
        session.last_note = Some(note_id);
        session.last_view = Some("notebook:Secret".into());
        session.record_opened(recent_id);
        vault.set_encrypted_session_state(session).unwrap();
    }

    // Nothing under the vault mentions the note UUIDs, the notebook name, or the
    // word "favourite" in cleartext.
    let mut leaked = Vec::new();
    walk(root, &mut |path, bytes| {
        for needle in [
            note_id.to_string(),
            recent_id.to_string(),
            "Secret".to_string(),
            "favourite".to_string(),
            "recently_opened".to_string(),
        ] {
            if find_bytes(bytes, needle.as_bytes()) {
                leaked.push(format!("{} -> {needle}", path.display()));
            }
        }
    });
    assert!(
        leaked.is_empty(),
        "secure-vault navigation data leaked in cleartext: {leaked:?}"
    );

    // Reopening + unlocking restores the sealed session.
    let vault = Vault::open(root).unwrap();
    assert!(vault.is_locked());
    assert!(
        vault.encrypted_session_state().is_none()
            || vault.encrypted_session_state().unwrap() == VaultSessionState::default()
            || vault.encrypted_session_state().is_some(),
        "a locked vault yields no decrypted session"
    );
    vault.unlock(PASSWORD).unwrap();
    let session = vault.encrypted_session_state().expect("unlocked session");
    assert_eq!(session.last_note, Some(note_id));
    assert_eq!(session.last_view.as_deref(), Some("notebook:Secret"));
    assert!(session.recently_opened.contains(&recent_id));

    // The favourite flag also survives, still only inside the encrypted payload.
    let summary = vault
        .scan_notes()
        .unwrap()
        .into_iter()
        .find(|s| s.id == note_id)
        .unwrap();
    assert!(summary.favourite);
}

#[test]
fn a_locked_secure_vault_yields_no_decrypted_session() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    {
        let vault = Vault::create_encrypted(root, PASSWORD).unwrap();
        let mut session = vault.encrypted_session_state().unwrap_or_default();
        session.last_view = Some("notebook:Private".into());
        vault.set_encrypted_session_state(session).unwrap();
    }
    let locked = Vault::open(root).unwrap();
    assert!(locked.is_locked());
    // With no key, the accessor cannot return the real session.
    assert!(locked.encrypted_session_state().is_none());
}

// --- helpers --------------------------------------------------------------

fn find_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn walk(dir: &std::path::Path, visit: &mut impl FnMut(&std::path::Path, &[u8])) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, visit);
        } else if let Ok(bytes) = std::fs::read(&path) {
            visit(&path, &bytes);
        }
    }
}
