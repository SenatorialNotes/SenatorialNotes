use std::path::PathBuf;

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
