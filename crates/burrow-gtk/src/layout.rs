//! The open tabs, kept in `tabs.toml` in the state folder so the next start
//! opens them again. It is runtime state, so it stays out of the config
//! folder, which may be shared or synced.

use std::fs;
use std::path::{Path, PathBuf};

use burrow_core::NodeId;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SavedTab {
    /// A local shell, in the folder it was last in, when the shell said.
    Terminal {
        shell: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        folder: Option<PathBuf>,
    },
    /// A node's own tab, such as a database or a tunnel.
    Node {
        node: NodeId,
    },
    /// A shell on an SSH host.
    Shell {
        node: NodeId,
    },
    Files {
        node: NodeId,
    },
    Monitor {
        node: NodeId,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Layout {
    /// The index in `tabs` of the tab that was showing.
    #[serde(default)]
    pub current: usize,
    #[serde(default, rename = "tab")]
    pub tabs: Vec<SavedTab>,
}

fn layout_path(state_dir: &Path) -> PathBuf {
    state_dir.join("tabs.toml")
}

impl Layout {
    /// No tabs when the file is missing or can't be read.
    pub fn load(state_dir: &Path) -> Self {
        fs::read_to_string(layout_path(state_dir))
            .ok()
            .and_then(|text| toml::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Writes a new file and renames it over the old one, so a crash
    /// halfway leaves the old layout rather than half of one.
    pub fn save(&self, state_dir: &Path) -> Result<(), String> {
        let path = layout_path(state_dir);
        let text = toml::to_string(self).map_err(|e| e.to_string())?;
        fs::create_dir_all(state_dir).map_err(|e| format!("{}: {e}", state_dir.display()))?;
        let new = path.with_extension("toml.new");
        fs::write(&new, text).map_err(|e| format!("{}: {e}", new.display()))?;
        fs::rename(&new, &path).map_err(|e| format!("{}: {e}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_saved_layout_loads_the_same() {
        let dir = std::env::temp_dir().join(format!("burrow-layout-{}", std::process::id()));
        let node = NodeId::new();
        let layout = Layout {
            current: 1,
            tabs: vec![
                SavedTab::Terminal {
                    shell: "/usr/bin/zsh".into(),
                    folder: Some("/home/me/src".into()),
                },
                SavedTab::Terminal {
                    shell: "/bin/bash".into(),
                    folder: None,
                },
                SavedTab::Shell { node },
                SavedTab::Files { node },
                SavedTab::Monitor { node },
                SavedTab::Node { node },
            ],
        };
        layout.save(&dir).unwrap();
        assert_eq!(Layout::load(&dir), layout);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_missing_or_broken_file_opens_nothing() {
        assert_eq!(
            Layout::load(Path::new("/nonexistent/burrow")),
            Layout::default()
        );
        let dir = std::env::temp_dir().join(format!("burrow-layout-bad-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(layout_path(&dir), "[[tab]]\nkind = \"rocket\"\n").unwrap();
        assert_eq!(Layout::load(&dir), Layout::default());
        fs::remove_dir_all(&dir).unwrap();
    }
}
