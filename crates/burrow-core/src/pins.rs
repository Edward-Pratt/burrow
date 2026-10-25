//! Choices the user made about hosts, kept in the state folder and each
//! pinned to a fingerprint of the host as the user saw it.
//!
//! Node files can arrive with a `git pull`, so nothing in them may make
//! such a choice, and a host file that keeps its id but points somewhere
//! else withdraws the choice instead of carrying it along.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::api::{Node, NodeId};
use crate::error::{Error, Result};
use crate::grants::{self, Lookup};
use crate::tree::write_atomic;

pub(crate) struct Pins {
    path: PathBuf,
    saved: Mutex<Saved>,
}

/// The state file: host node id to the fingerprint it was chosen for.
#[derive(Default, Serialize, Deserialize)]
struct Saved {
    #[serde(default)]
    hosts: BTreeMap<NodeId, String>,
}

impl Pins {
    /// Reads the state file. A broken one is logged and treated as empty
    /// until the next change rewrites it, so nothing is chosen by accident.
    pub fn load(path: PathBuf) -> Pins {
        let saved = match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).unwrap_or_else(|e: toml::de::Error| {
                tracing::warn!("ignoring {}: {}", path.display(), e.message());
                Saved::default()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Saved::default(),
            Err(e) => {
                tracing::warn!("ignoring {}: {e}", path.display());
                Saved::default()
            }
        };
        Pins {
            path,
            saved: Mutex::new(saved),
        }
    }

    /// Whether `node` is chosen, as it is now.
    pub fn has(&self, lookup: &impl Lookup, node: &Node) -> bool {
        let saved = self.saved.lock().unwrap();
        saved.hosts.get(&node.id) == Some(&grants::fingerprint(lookup, node))
    }

    /// The hosts whose choice still matches their settings.
    pub fn nodes(&self, lookup: &impl Lookup) -> Vec<NodeId> {
        let saved = self.saved.lock().unwrap();
        saved
            .hosts
            .iter()
            .filter(|(id, pinned)| {
                lookup(**id).is_some_and(|node| grants::fingerprint(lookup, &node) == **pinned)
            })
            .map(|(id, _)| *id)
            .collect()
    }

    /// Chooses `node` as it is now, or drops the choice.
    pub fn set(&self, lookup: &impl Lookup, node: &Node, chosen: bool) -> Result<()> {
        let mut saved = self.saved.lock().unwrap();
        if chosen {
            saved
                .hosts
                .insert(node.id, grants::fingerprint(lookup, node));
        } else {
            saved.hosts.remove(&node.id);
        }
        save(&self.path, &saved)
    }

    /// Drops choices for hosts that no longer exist.
    pub fn forget(&self, nodes: &[NodeId]) {
        let mut saved = self.saved.lock().unwrap();
        let before = saved.hosts.len();
        saved.hosts.retain(|id, _| !nodes.contains(id));
        if saved.hosts.len() != before
            && let Err(e) = save(&self.path, &saved)
        {
            tracing::warn!("could not update {}: {e}", self.path.display());
        }
    }
}

fn save(path: &Path, saved: &Saved) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| Error::io(dir, e))?;
    }
    let text = toml::to_string_pretty(saved).map_err(|e| Error::Encode {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;
    write_atomic(path, &text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(alias: &str) -> Node {
        Node {
            id: NodeId::new(),
            parent: None,
            kind: crate::api::SSH_HOST_KIND.into(),
            name: "web".into(),
            color: None,
            props: toml::from_str(&format!("alias = \"{alias}\"")).unwrap(),
            secrets: Default::default(),
        }
    }

    #[test]
    fn a_choice_holds_only_while_the_host_is_as_it_was() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("pins.toml");
        let node = host("web");
        let lookup = |id| (id == node.id).then(|| node.clone());
        let pins = Pins::load(path.clone());
        pins.set(&lookup, &node, true).unwrap();
        assert!(pins.has(&lookup, &node));
        assert_eq!(Pins::load(path.clone()).nodes(&lookup), vec![node.id]);

        let moved = host("elsewhere");
        let moved = Node {
            id: node.id,
            ..moved
        };
        let lookup = |id| (id == moved.id).then(|| moved.clone());
        assert!(!pins.has(&lookup, &moved));
        assert!(pins.nodes(&lookup).is_empty());

        pins.forget(&[node.id]);
        assert!(Pins::load(path).nodes(&|_| Some(node.clone())).is_empty());
    }

    #[test]
    fn a_broken_file_chooses_nothing() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("pins.toml");
        std::fs::write(&path, "hosts = [").unwrap();
        let node = host("web");
        assert!(Pins::load(path).nodes(&|_| Some(node.clone())).is_empty());
    }
}
