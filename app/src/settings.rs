//! Persisted user settings (theme, view mode, hidden toggle, sort).
//!
//! Lives at `$XDG_CONFIG_HOME/fm/data/settings.toml`. Missing / malformed file
//! is treated as "use defaults" — we never panic on user data.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedSettings {
    #[serde(default = "default_true")]
    pub dark_theme: bool,
    /// 0=list, 1=gallery. View mode is decoupled from element size since
    /// v0.2 — old configs with values 1..4 are migrated to gallery+item_size.
    #[serde(default)]
    pub view_mode: u8,
    /// Discrete element-size step shared by list + gallery (1..=7, default 3).
    #[serde(default = "default_item_size")]
    pub item_size: u8,
    #[serde(default)]
    pub show_hidden: bool,
    /// 0=name, 1=size, 2=mtime, 3=kind
    #[serde(default)]
    pub sort_key: u8,
    /// 0=asc, 1=desc
    #[serde(default)]
    pub sort_order: u8,
    /// EXPERIMENTAL: watch for filesystem changes in the current folder and
    /// reload automatically. Off by default - enabling it may cause extra CPU
    /// usage or missed events on some systems. Keeping it off is also what makes
    /// the app stay efficient with large directories.
    #[serde(default)]
    pub auto_refresh: bool,
    #[serde(default = "default_window_size")]
    pub window_size: (u32, u32),
    #[serde(default = "default_sidebar_width")]
    pub sidebar_width: u32,
    /// Generated-thumbnail resolution: 0=Normal 1=Large 2=X-Large 3=XX-Large.
    #[serde(default = "default_thumb_size")]
    pub thumb_size: u8,
    /// Icon style: 0=monochrome (default), 1=colored.
    #[serde(default)]
    pub icon_style: u8,
    /// List-view column visibility (Name is always shown).
    #[serde(default = "default_true")]
    pub col_size_visible: bool,
    #[serde(default = "default_true")]
    pub col_modified_visible: bool,
    #[serde(default = "default_true")]
    pub col_kind_visible: bool,
    /// Upper bound on RAM held by loaded thumbnails, in MiB. Once reached, more
    /// images simply stop loading (they fall back to the generic icon) so a
    /// folder with thousands of large images can't exhaust memory. 0 = no cap.
    #[serde(default = "default_thumb_mem_budget_mb")]
    pub thumb_mem_budget_mb: u32,
}

fn default_thumb_mem_budget_mb() -> u32 {
    2048 // 2 GiB
}

fn default_thumb_size() -> u8 {
    2 // X-Large (512px) — matches the previous hard-coded value
}

fn default_sidebar_width() -> u32 {
    220
}

fn default_item_size() -> u8 {
    3
}

fn default_true() -> bool {
    true
}

fn default_window_size() -> (u32, u32) {
    (1100, 700)
}

impl Default for PersistedSettings {
    fn default() -> Self {
        Self {
            dark_theme: true,
            view_mode: 0,
            item_size: default_item_size(),
            show_hidden: false,
            sort_key: 0,
            sort_order: 0,
            auto_refresh: false,
            window_size: default_window_size(),
            sidebar_width: default_sidebar_width(),
            thumb_size: default_thumb_size(),
            icon_style: 0,
            col_size_visible: true,
            col_modified_visible: true,
            col_kind_visible: true,
            thumb_mem_budget_mb: default_thumb_mem_budget_mb(),
        }
    }
}

fn settings_path() -> Option<PathBuf> {
    let dir = dirs::config_dir()?.join("mykrut").join("data");
    Some(dir.join("settings.toml"))
}

pub fn load() -> PersistedSettings {
    let Some(path) = settings_path() else {
        return PersistedSettings::default();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        debug!("settings file not found, using defaults");
        return PersistedSettings::default();
    };
    match toml::from_str::<PersistedSettings>(&text) {
        Ok(s) => {
            debug!(?s, "settings loaded");
            s
        }
        Err(err) => {
            warn!(?err, "malformed settings.toml — using defaults");
            PersistedSettings::default()
        }
    }
}

pub fn save(s: &PersistedSettings) -> std::io::Result<()> {
    let Some(path) = settings_path() else {
        return Err(std::io::Error::other("no XDG config dir"));
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = toml::to_string_pretty(s).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    crate::fs_atomic::write_atomic(&path, text.as_bytes())?;
    debug!(path = %path.display(), "settings saved");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_defaults() {
        let s = PersistedSettings::default();
        let text = toml::to_string_pretty(&s).unwrap();
        let parsed: PersistedSettings = toml::from_str(&text).unwrap();
        assert_eq!(parsed.dark_theme, s.dark_theme);
        assert_eq!(parsed.view_mode, s.view_mode);
        assert_eq!(parsed.window_size, s.window_size);
    }

    #[test]
    fn missing_fields_use_default() {
        let text = "
            dark_theme = false
        ";
        let parsed: PersistedSettings = toml::from_str(text).unwrap();
        assert!(!parsed.dark_theme);
        assert_eq!(parsed.view_mode, 0);
        assert_eq!(parsed.window_size, (1100, 700));
    }
}
