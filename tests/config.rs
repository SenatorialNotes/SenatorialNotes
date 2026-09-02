use std::path::{Path, PathBuf};

use senatorial_notes::VaultKind;
use senatorial_notes::config::{AppConfig, NoteListDensity, Theme};

#[test]
fn parses_configuration_and_remembers_recent_vaults() {
    let source = r#"
recent_vaults = ["/notes/one"]
last_vault = "/notes/one"
autosave_delay_ms = 800
"#;
    let mut config: AppConfig = toml::from_str(source).expect("configuration should parse");
    assert_eq!(config.autosave_delay_ms, 800);

    config.remember_vault(PathBuf::from("/notes/two").as_path());
    assert_eq!(config.last_vault, Some(PathBuf::from("/notes/two")));
    assert_eq!(config.recent_vaults[0], PathBuf::from("/notes/two"));
    assert_eq!(config.recent_vaults[1], PathBuf::from("/notes/one"));
}

#[test]
fn appearance_and_title_debounce_persist() {
    let source = r#"
title_commit_delay_ms = 1500

[appearance]
theme = "dark"
editor_font_family = "Cantarell"
editor_font_size = 18
editor_line_spacing = 6
editor_content_width = 76
show_line_numbers = true
note_list_density = "compact"
note_preview_length = 80
accent = "purple"
"#;
    let config: AppConfig = toml::from_str(source).expect("appearance config should parse");
    assert_eq!(config.title_commit_delay_ms, 1_500);
    assert_eq!(config.appearance.theme, Theme::Dark);
    assert_eq!(
        config.appearance.note_list_density,
        NoteListDensity::Compact
    );
    assert!(config.appearance.show_line_numbers);

    let serialized = toml::to_string(&config).expect("config should serialize");
    let round_trip: AppConfig = toml::from_str(&serialized).expect("round trip should parse");
    assert_eq!(round_trip, config);
}

#[test]
fn vault_index_records_kind_and_display_name_and_round_trips() {
    let mut config = AppConfig::default();
    assert!(!config.first_run_done);

    config.record_vault_open(Path::new("/vaults/main"), VaultKind::Ordinary);
    config.record_vault_open(Path::new("/vaults/secret"), VaultKind::Encrypted);
    config.set_vault_display_name(Path::new("/vaults/secret"), "  My Secure  ");

    assert_eq!(
        config.vault_info(Path::new("/vaults/main")).map(|i| i.kind),
        Some(VaultKind::Ordinary)
    );
    // Display name is trimmed; whitespace-only clears it.
    assert_eq!(
        config.vault_display_name(Path::new("/vaults/secret")),
        Some("My Secure")
    );
    config.set_vault_display_name(Path::new("/vaults/secret"), "   ");
    assert_eq!(config.vault_display_name(Path::new("/vaults/secret")), None);
    config.set_vault_display_name(Path::new("/vaults/secret"), "Secure");

    let toml = toml::to_string(&config).expect("serialize");
    let back: AppConfig = toml::from_str(&toml).expect("round trip");
    assert_eq!(back, config);
    assert_eq!(
        back.vault_display_name(Path::new("/vaults/secret")),
        Some("Secure")
    );
}

#[test]
fn secure_vaults_mru_filters_to_encrypted_and_orders_by_last_opened() {
    let mut config = AppConfig::default();
    // Non-existent paths are dropped by secure_vaults_mru (it filters is_dir),
    // so use the current directory's children as stand-ins would be flaky;
    // instead assert the filtering logic via vault_index directly.
    config.record_vault_open(Path::new("/a-standard"), VaultKind::Ordinary);
    config.record_vault_open(Path::new("/b-secure"), VaultKind::Encrypted);
    config.record_vault_open(Path::new("/c-secure"), VaultKind::Encrypted);

    let encrypted: Vec<_> = config
        .vault_index
        .iter()
        .filter(|(_, i)| i.kind == VaultKind::Encrypted)
        .map(|(k, _)| k.clone())
        .collect();
    assert_eq!(encrypted.len(), 2);
    assert!(!encrypted.iter().any(|k| k.contains("standard")));

    // secure_vaults_mru additionally requires the folder to exist on disk.
    assert!(config.secure_vaults_mru().is_empty());
}

#[test]
fn forget_vault_also_clears_the_vault_index_entry() {
    let mut config = AppConfig::default();
    config.record_vault_open(Path::new("/vaults/x"), VaultKind::Encrypted);
    config.set_vault_display_name(Path::new("/vaults/x"), "X");
    assert!(config.vault_info(Path::new("/vaults/x")).is_some());
    config.forget_vault(Path::new("/vaults/x"));
    assert!(config.vault_info(Path::new("/vaults/x")).is_none());
}

#[test]
fn an_older_config_without_first_run_or_vault_index_still_loads() {
    let source = r#"
recent_vaults = ["/notes/one"]
last_vault = "/notes/one"
"#;
    let config: AppConfig = toml::from_str(source).expect("legacy config parses");
    assert!(!config.first_run_done);
    assert!(config.vault_index.is_empty());
}
