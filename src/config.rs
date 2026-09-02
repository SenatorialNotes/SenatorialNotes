use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use directories::BaseDirs;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::constants::CONFIG_DIR_NAME;
use crate::error::io_error;
use crate::storage::atomic::atomic_write;
use crate::vault_manifest::VaultKind;
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

/// User-controlled note-list ordering.
///
/// `AppConfig::sort_order` is `Option<SortOrder>`, not a plain `SortOrder`,
/// because "no preference saved yet" and "explicitly chose Last Edited" are
/// different states: with no explicit choice (`None`), behavior stays
/// byte-for-byte the v0.1 default (pinned-first, then most-recently-updated,
/// then title). Choosing any variant explicitly (`Some(_)`, including
/// `LastEdited`) makes that field the sole primary key - pinned-first is
/// dropped, since silently regrouping by pin would contradict an explicit
/// choice - with note UUID as the final tie-breaker so equal keys never
/// produce a "flickering" order between renders. Sorting only ever reorders
/// an in-memory list; it never rewrites a note file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SortOrder {
    LastEdited,
    DateCreated,
    TitleAsc,
    TitleZa,
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

/// Per-vault, locally persisted UI state, keyed by `vault_id` in
/// [`AppConfig::vault_sessions`]. Never contains note contents.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VaultSessionState {
    /// UUID of the note that was selected when this vault was last left.
    pub last_note: Option<Uuid>,
    /// Opaque token for the smart view / notebook that was active
    /// (`all-notes`, `pinned`, `favourites`, `recently-opened`, `archive`,
    /// `trash`, `notebook:<relative/path>`). Interpreted by the UI layer.
    pub last_view: Option<String>,
    /// Editor vertical scroll offset, best-effort.
    pub editor_scroll: Option<f64>,
    /// Note UUIDs the user has recently *opened / viewed* (not edited),
    /// most-recent first. Powers the "Recently Opened" smart view for an
    /// ordinary vault. Capped by [`RECENTLY_OPENED_LIMIT`]. A Secure Vault
    /// keeps this inside its sealed manifest instead (never in plaintext
    /// config).
    pub recently_opened: Vec<Uuid>,
}

/// Maximum notes tracked in "Recently Opened".
pub const RECENTLY_OPENED_LIMIT: usize = 25;

impl VaultSessionState {
    /// Records `id` as the most-recently opened note (moves it to the front,
    /// de-duplicated, capped).
    pub fn record_opened(&mut self, id: Uuid) {
        self.recently_opened.retain(|candidate| *candidate != id);
        self.recently_opened.insert(0, id);
        self.recently_opened.truncate(RECENTLY_OPENED_LIMIT);
    }
}

/// A locally-cached fact about a vault SenatorialNotes has opened, keyed by
/// canonical path in [`AppConfig::vault_index`]. Purely a UI convenience: it
/// lets the sidebar render the "Secured Vaults" list and a renameable display
/// name without opening every `vault.toml`. Nothing here is authoritative and
/// nothing here is secret - `kind` and the folder-derived name are already
/// visible on disk. A Secure Vault's *display name* lives here (not in
/// `vault.toml`) so it is renameable without touching the encrypted store.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VaultInfo {
    pub kind: VaultKind,
    /// User-chosen display name. `None` → fall back to the folder basename.
    pub display_name: Option<String>,
    pub last_opened: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub recent_vaults: Vec<PathBuf>,
    #[serde(default)]
    pub last_vault: Option<PathBuf>,
    /// `true` once the one-time first-run setup (managed workspace root +
    /// "Main" Standard Vault) has completed. Additive; an older config loads
    /// with `false`, but a non-empty `recent_vaults` also suppresses first-run.
    #[serde(default)]
    pub first_run_done: bool,
    /// Local index of known vaults, keyed by canonical path string. Additive.
    #[serde(default)]
    pub vault_index: BTreeMap<String, VaultInfo>,
    /// Per-vault UI state, keyed by `vault_id` string. Additive and optional:
    /// an older config without this table still loads.
    #[serde(default)]
    pub vault_sessions: BTreeMap<String, VaultSessionState>,
    #[serde(default = "default_autosave_delay")]
    pub autosave_delay_ms: u64,
    #[serde(default = "default_title_delay")]
    pub title_commit_delay_ms: u64,
    #[serde(default)]
    pub appearance: AppearanceConfig,
    #[serde(default)]
    pub encrypted_note_locking: LockingConfig,
    /// `None` means no explicit preference has been saved yet; see
    /// [`SortOrder`] for why that is distinct from `Some(SortOrder::LastEdited)`.
    #[serde(default)]
    pub sort_order: Option<SortOrder>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            recent_vaults: Vec::new(),
            last_vault: None,
            first_run_done: false,
            vault_index: BTreeMap::new(),
            vault_sessions: BTreeMap::new(),
            autosave_delay_ms: default_autosave_delay(),
            title_commit_delay_ms: default_title_delay(),
            appearance: AppearanceConfig::default(),
            encrypted_note_locking: LockingConfig::default(),
            sort_order: None,
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

    /// Best-effort canonical form of `path`, used only for equality; falls back
    /// to the path as given (e.g. when it does not currently exist).
    fn canonical_key(path: &Path) -> PathBuf {
        fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
    }

    /// The `vault_index` map key for `path`.
    fn index_key(path: &Path) -> String {
        Self::canonical_key(path).to_string_lossy().into_owned()
    }

    /// Records `vault` as the most-recently-used vault, de-duplicated by
    /// canonical path, keeping at most 10 entries.
    pub fn remember_vault(&mut self, vault: &Path) {
        let key = Self::canonical_key(vault);
        self.recent_vaults
            .retain(|candidate| Self::canonical_key(candidate) != key);
        self.recent_vaults.insert(0, key.clone());
        self.recent_vaults.truncate(10);
        self.last_vault = Some(key);
    }

    /// [`remember_vault`](Self::remember_vault) plus a `vault_index` update
    /// (`kind` + `last_opened`), so the sidebar can list Secured Vaults and
    /// resolve display names without opening every `vault.toml`.
    pub fn record_vault_open(&mut self, vault: &Path, kind: VaultKind) {
        self.remember_vault(vault);
        let entry = self.vault_index.entry(Self::index_key(vault)).or_default();
        entry.kind = kind;
        entry.last_opened = Some(Utc::now());
    }

    /// The `VaultInfo` recorded for `path`, if any.
    pub fn vault_info(&self, path: &Path) -> Option<&VaultInfo> {
        self.vault_index.get(&Self::index_key(path))
    }

    /// The user-chosen display name for `path`, if one has been set.
    pub fn vault_display_name(&self, path: &Path) -> Option<&str> {
        self.vault_info(path)
            .and_then(|info| info.display_name.as_deref())
    }

    /// Sets (or, given an empty string, clears) the display name for `path`.
    /// **Display name only** - never renames or moves the vault directory.
    pub fn set_vault_display_name(&mut self, path: &Path, name: &str) {
        let name = name.trim();
        let entry = self.vault_index.entry(Self::index_key(path)).or_default();
        entry.display_name = (!name.is_empty()).then(|| name.to_string());
    }

    /// Canonical paths of known Secure (encrypted) vaults, most-recently-opened
    /// first, filtered to those whose folder still exists.
    pub fn secure_vaults_mru(&self) -> Vec<PathBuf> {
        let mut vaults: Vec<(&String, &VaultInfo)> = self
            .vault_index
            .iter()
            .filter(|(_, info)| info.kind == VaultKind::Encrypted)
            .collect();
        vaults.sort_by(|a, b| b.1.last_opened.cmp(&a.1.last_opened).then(a.0.cmp(b.0)));
        vaults
            .into_iter()
            .map(|(key, _)| PathBuf::from(key))
            .filter(|path| path.is_dir())
            .collect()
    }

    /// Removes `path` from the recent list, the vault index, and `last_vault`
    /// (if it points there), matched by canonical path. Never touches the
    /// filesystem.
    pub fn forget_vault(&mut self, path: &Path) {
        let key = Self::canonical_key(path);
        self.recent_vaults
            .retain(|candidate| Self::canonical_key(candidate) != key);
        self.vault_index.remove(&Self::index_key(path));
        if self.last_vault.as_deref().map(Self::canonical_key) == Some(key) {
            self.last_vault = None;
        }
    }

    /// Recent vaults, most-recent-first, de-duplicated by canonical path.
    pub fn recent_vaults_mru(&self) -> Vec<PathBuf> {
        let mut seen = HashSet::new();
        self.recent_vaults
            .iter()
            .filter(|path| seen.insert(Self::canonical_key(path)))
            .cloned()
            .collect()
    }

    pub fn vault_session(&self, vault_id: Uuid) -> Option<&VaultSessionState> {
        self.vault_sessions.get(&vault_id.to_string())
    }

    /// Stores (or, when fully empty, clears) the per-vault UI state for
    /// `vault_id`.
    pub fn set_vault_session(&mut self, vault_id: Uuid, session: VaultSessionState) {
        let key = vault_id.to_string();
        if session == VaultSessionState::default() {
            self.vault_sessions.remove(&key);
        } else {
            self.vault_sessions.insert(key, session);
        }
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
