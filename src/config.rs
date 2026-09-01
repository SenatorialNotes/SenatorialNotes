use std::fs;
use std::path::{Path, PathBuf};

use directories::BaseDirs;
use serde::{Deserialize, Serialize};

use crate::constants::CONFIG_DIR_NAME;
use crate::error::io_error;
use crate::storage::atomic::atomic_write;
use crate::{Error, Result};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NoteListDensity {
    Compact,
    #[default]
    Comfortable,
    Spacious,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Accent {
    #[default]
    Blue,
    Teal,
    Green,
    Purple,
    Orange,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppearanceConfig {
    pub theme: Theme,
    pub editor_font_family: String,
    pub editor_font_size: u32,
    pub editor_line_spacing: u32,
    pub editor_content_width: u32,
    pub show_line_numbers: bool,
    pub note_list_density: NoteListDensity,
    pub note_preview_length: usize,
    pub accent: Accent,
}

impl Default for AppearanceConfig {
    fn default() -> Self {
        Self {
            theme: Theme::System,
            editor_font_family: "Sans".into(),
            editor_font_size: 16,
            editor_line_spacing: 4,
            editor_content_width: 108,
            show_line_numbers: false,
            note_list_density: NoteListDensity::Comfortable,
            note_preview_length: 120,
            accent: Accent::Blue,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LockingConfig {
    pub on_note_switch: bool,
    pub on_focus_loss: bool,
    pub after_minutes: u32,
    pub on_minimize: bool,
    pub on_exit: bool,
}

impl Default for LockingConfig {
    fn default() -> Self {
        Self {
            on_note_switch: false,
            on_focus_loss: false,
            after_minutes: 0,
            on_minimize: false,
            on_exit: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub recent_vaults: Vec<PathBuf>,
    #[serde(default)]
    pub last_vault: Option<PathBuf>,
    #[serde(default = "default_autosave_delay")]
    pub autosave_delay_ms: u64,
    #[serde(default = "default_title_delay")]
    pub title_commit_delay_ms: u64,
    #[serde(default)]
    pub appearance: AppearanceConfig,
    #[serde(default)]
    pub encrypted_note_locking: LockingConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            recent_vaults: Vec::new(),
            last_vault: None,
            autosave_delay_ms: default_autosave_delay(),
            title_commit_delay_ms: default_title_delay(),
            appearance: AppearanceConfig::default(),
            encrypted_note_locking: LockingConfig::default(),
        }
    }
}

const fn default_autosave_delay() -> u64 {
    750
}

const fn default_title_delay() -> u64 {
    1_500
}

impl AppConfig {
    pub fn load() -> Result<Self> {
        let path = Self::path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let contents = fs::read_to_string(&path).map_err(|source| io_error(&path, source))?;
        toml::from_str(&contents).map_err(|error| Error::Configuration(error.to_string()))
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        let parent = path
            .parent()
            .ok_or_else(|| Error::Configuration("configuration path has no parent".into()))?;
        create_private_directory(parent)?;
        let contents = toml::to_string_pretty(self)
            .map_err(|error| Error::Configuration(error.to_string()))?;
        atomic_write(&path, contents.as_bytes())
    }

    pub fn remember_vault(&mut self, vault: &Path) {
        self.recent_vaults.retain(|candidate| candidate != vault);
        self.recent_vaults.insert(0, vault.to_path_buf());
        self.recent_vaults.truncate(10);
        self.last_vault = Some(vault.to_path_buf());
    }

    pub fn path() -> Result<PathBuf> {
        let base = BaseDirs::new().ok_or(Error::NoConfigDirectory)?;
        Ok(base.config_dir().join(CONFIG_DIR_NAME).join("config.toml"))
    }
}

fn create_private_directory(path: &Path) -> Result<()> {
    let existed = path.exists();
    fs::create_dir_all(path).map_err(|source| io_error(path, source))?;
    #[cfg(unix)]
    if !existed {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|source| io_error(path, source))?;
    }
    Ok(())
}
