//! UI preferences, kept in the `[ui]` table of `burrow.toml`. Other tables
//! in that file are left untouched.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    /// Whatever GTK is configured for (`settings.ini`, gsettings).
    #[default]
    System,
    Light,
    Dark,
}

impl Theme {
    pub const ALL: [Theme; 3] = [Theme::System, Theme::Light, Theme::Dark];

    pub fn label(self) -> &'static str {
        match self {
            Theme::System => "System",
            Theme::Light => "Light",
            Theme::Dark => "Dark",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UiSettings {
    #[serde(default)]
    pub theme: Theme,
    #[serde(default = "default_true")]
    pub sidebar_visible: bool,
    /// A Pango font description such as "JetBrains Mono 11". Terminals use
    /// the system's monospace font without one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_font: Option<String>,
}

impl Default for UiSettings {
    fn default() -> Self {
        UiSettings {
            theme: Theme::default(),
            sidebar_visible: true,
            terminal_font: None,
        }
    }
}

fn default_true() -> bool {
    true
}

fn settings_path(config_dir: &Path) -> PathBuf {
    config_dir.join("burrow.toml")
}

impl UiSettings {
    /// Falls back to defaults when the file is missing or unreadable.
    pub fn load(config_dir: &Path) -> Self {
        fs::read_to_string(settings_path(config_dir))
            .ok()
            .and_then(|text| text.parse::<toml::Table>().ok())
            .and_then(|table| table.get("ui").cloned())
            .and_then(|ui| ui.try_into().ok())
            .unwrap_or_default()
    }

    pub fn save(&self, config_dir: &Path) -> Result<(), String> {
        let path = settings_path(config_dir);
        let mut table = match fs::read_to_string(&path) {
            Ok(text) => text
                .parse::<toml::Table>()
                .map_err(|e| format!("{}: {e}", path.display()))?,
            Err(_) => toml::Table::new(),
        };
        let ui = toml::Table::try_from(self).map_err(|e| e.to_string())?;
        table.insert("ui".into(), toml::Value::Table(ui));
        let text = toml::to_string_pretty(&table).map_err(|e| e.to_string())?;
        fs::create_dir_all(config_dir).map_err(|e| e.to_string())?;
        fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saving_keeps_other_tables() {
        let dir = std::env::temp_dir().join(format!("burrow-settings-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(settings_path(&dir), "[other]\nkeep = true\n").unwrap();

        let settings = UiSettings {
            theme: Theme::Dark,
            sidebar_visible: false,
            terminal_font: Some("Monospace 12".into()),
        };
        settings.save(&dir).unwrap();
        assert_eq!(UiSettings::load(&dir), settings);
        let text = fs::read_to_string(settings_path(&dir)).unwrap();
        assert!(text.contains("keep = true"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_file_gives_defaults() {
        assert_eq!(
            UiSettings::load(Path::new("/nonexistent/burrow")),
            UiSettings::default()
        );
    }
}
