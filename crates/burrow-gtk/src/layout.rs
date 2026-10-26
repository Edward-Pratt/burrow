//! The open tabs, kept in `tabs.toml` in the state folder so the next start
//! opens them again. It is runtime state, so it stays out of the config
//! folder, which may be shared or synced.

use std::fs;
use std::path::{Path, PathBuf};

use burrow_core::NodeId;
use relm4::gtk::glib;
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

/// Where a local shell may start again for `folder`, with its links
/// resolved, or `None` to use the home folder. Output can name any folder
/// as the shell's (OSC 7), and prompts that run `git` in a folder run hooks
/// from its repository, so the folder must be the user's own and writable
/// by nobody else. So must each folder above it, or someone could swap the
/// folder for their own between this check and the shell starting, except
/// that in a sticky folder such as /tmp others can't rename the user's.
pub fn safe_folder(folder: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    const STICKY: u32 = 0o1000;
    let user = fs::metadata(glib::home_dir()).ok()?.uid();
    let folder = fs::canonicalize(folder).ok()?;
    let meta = fs::metadata(&folder).ok()?;
    if !meta.is_dir() || meta.uid() != user || meta.mode() & 0o022 != 0 {
        return None;
    }
    for parent in folder.ancestors().skip(1) {
        let meta = fs::metadata(parent).ok()?;
        let owner_ok = meta.uid() == user || meta.uid() == 0;
        let others_ok = meta.mode() & 0o022 == 0 || meta.mode() & STICKY != 0;
        if !owner_ok || !others_ok {
            return None;
        }
    }
    Some(folder)
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
    fn shells_start_only_in_folders_no_one_else_can_write() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("burrow-folder-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        let dir = fs::canonicalize(&dir).unwrap();
        assert_eq!(safe_folder(&dir), Some(dir.clone()));
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o775)).unwrap();
        assert_eq!(safe_folder(&dir), None);
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o757)).unwrap();
        assert_eq!(safe_folder(&dir), None);

        // A folder of the user's own, in a folder others can write.
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o777)).unwrap();
        let inner = dir.join("inner");
        fs::create_dir(&inner).unwrap();
        fs::set_permissions(&inner, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(safe_folder(&inner), None);
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(safe_folder(&inner), Some(inner.clone()));

        // A link is followed, and the folder it leads to is checked.
        let link = dir.join("link");
        std::os::unix::fs::symlink(&inner, &link).unwrap();
        assert_eq!(safe_folder(&link), Some(inner.clone()));
        let shared = dir.join("shared");
        fs::create_dir(&shared).unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o777)).unwrap();
        std::os::unix::fs::symlink(&shared, dir.join("to-shared")).unwrap();
        assert_eq!(safe_folder(&dir.join("to-shared")), None);

        // Shared folders such as /tmp. (CI runs as root, so `/` would be
        // the user's own there.)
        assert_eq!(safe_folder(Path::new("/tmp")), None);
        assert_eq!(safe_folder(&dir.join("missing")), None);
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
