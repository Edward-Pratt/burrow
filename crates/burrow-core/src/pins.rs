//! Choices the user made, kept in the state folder, each pinned to a
//! fingerprint of what the user saw when they made it.
//!
//! Node files can arrive with a `git pull`, so nothing in them may make
//! such a choice, and a file that keeps its id but points somewhere else
//! withdraws the choice instead of carrying it along. [`Pinned`] is the
//! state file every such choice lives in; [`Pins`] is the plain case, a
//! host chosen as it is.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::api::{Node, NodeId};
use crate::error::{Error, Result};
use crate::grants;
use crate::tree::write_atomic;

/// Looks a node up by id.
pub(crate) trait Lookup: Fn(NodeId) -> Option<Node> {}
impl<F: Fn(NodeId) -> Option<Node>> Lookup for F {}

/// A state file with one table of choices, `[table]`, keyed by what they
/// are about. Every change is written straight back, atomically.
pub(crate) struct Pinned<K, V> {
    path: PathBuf,
    table: &'static str,
    saved: Mutex<BTreeMap<K, V>>,
}

impl<K, V> Pinned<K, V>
where
    K: Ord + Clone + Serialize + DeserializeOwned,
    V: Clone + Serialize + DeserializeOwned,
{
    /// Reads the state file. A file that can't be read is treated as empty
    /// until the next change rewrites it, so nothing is chosen by accident,
    /// and the reason comes back for the caller to log or report.
    pub fn load(path: PathBuf, table: &'static str) -> (Pinned<K, V>, Option<String>) {
        let (saved, problem) = read(&path, table);
        let pinned = Pinned {
            path,
            table,
            saved: Mutex::new(saved),
        };
        (pinned, problem)
    }

    /// [`Pinned::load`], logging a file that can't be read.
    pub fn load_or_warn(path: PathBuf, table: &'static str) -> Pinned<K, V> {
        let (pinned, problem) = Self::load(path, table);
        if let Some(problem) = problem {
            tracing::warn!("ignoring {}: {problem}", pinned.path.display());
        }
        pinned
    }

    /// Reads the file again, as [`Pinned::load`] does.
    pub fn reload(&self) -> Option<String> {
        // Held across the read, so a change made meanwhile isn't replaced
        // by the file as it was before that change wrote it.
        let mut saved = self.saved.lock().unwrap();
        let (fresh, problem) = read(&self.path, self.table);
        *saved = fresh;
        problem
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn get(&self, key: &K) -> Option<V> {
        self.saved.lock().unwrap().get(key).cloned()
    }

    pub fn contains(&self, key: &K) -> bool {
        self.saved.lock().unwrap().contains_key(key)
    }

    /// Every entry, in key order.
    pub fn entries(&self) -> Vec<(K, V)> {
        let saved = self.saved.lock().unwrap();
        saved.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    pub fn insert(&self, key: K, value: V) -> Result<()> {
        let mut saved = self.saved.lock().unwrap();
        saved.insert(key, value);
        save(&self.path, self.table, &saved)
    }

    pub fn remove(&self, key: &K) -> Result<()> {
        let mut saved = self.saved.lock().unwrap();
        saved.remove(key);
        save(&self.path, self.table, &saved)
    }

    /// Drops the entries for `keys`, such as nodes that no longer exist.
    pub fn forget(&self, keys: &[K]) {
        let mut saved = self.saved.lock().unwrap();
        let before = saved.len();
        saved.retain(|key, _| !keys.contains(key));
        if saved.len() != before
            && let Err(e) = save(&self.path, self.table, &saved)
        {
            tracing::warn!("could not update {}: {e}", self.path.display());
        }
    }
}

/// The table in the file, or why it can't be read. A missing file is
/// empty. Other keys in it are ignored, and the next save drops them, as
/// before the stores were shared.
fn read<K, V>(path: &Path, table: &str) -> (BTreeMap<K, V>, Option<String>)
where
    K: Ord + DeserializeOwned,
    V: DeserializeOwned,
{
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (BTreeMap::new(), None),
        Err(e) => return (BTreeMap::new(), Some(e.to_string())),
    };
    let parsed = toml::from_str::<toml::Table>(&text)
        .map_err(|e| e.message().to_string())
        .and_then(|mut file| match file.remove(table) {
            None => Ok(BTreeMap::new()),
            Some(value) => value
                .try_into()
                .map_err(|e: toml::de::Error| e.message().to_string()),
        });
    match parsed {
        Ok(saved) => (saved, None),
        Err(message) => (BTreeMap::new(), Some(message)),
    }
}

fn save<K: Serialize, V: Serialize>(
    path: &Path,
    table: &str,
    saved: &BTreeMap<K, V>,
) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| Error::io(dir, e))?;
    }
    let encode = |message: String| Error::Encode {
        path: path.to_path_buf(),
        message,
    };
    let value = toml::Value::try_from(saved).map_err(|e| encode(e.to_string()))?;
    let mut file = toml::Table::new();
    file.insert(table.to_string(), value);
    let text = toml::to_string_pretty(&file).map_err(|e| encode(e.to_string()))?;
    write_atomic(path, &text)
}

/// Hosts chosen as they are, such as those monitored in the background:
/// host node id to the fingerprint of the host as the user saw it.
pub(crate) struct Pins(Pinned<NodeId, String>);

impl Pins {
    /// Reads the state file. A broken one is logged and treated as empty
    /// until the next change rewrites it, so nothing is chosen by accident.
    pub fn load(path: PathBuf) -> Pins {
        Pins(Pinned::load_or_warn(path, "hosts"))
    }

    /// Whether `node` is chosen, as it is now.
    pub fn has(&self, lookup: &impl Lookup, node: &Node) -> bool {
        self.0.get(&node.id) == Some(grants::fingerprint(lookup, node))
    }

    /// The hosts whose choice still matches their settings.
    pub fn nodes(&self, lookup: &impl Lookup) -> Vec<NodeId> {
        self.0
            .entries()
            .into_iter()
            .filter(|(id, pinned)| {
                lookup(*id).is_some_and(|node| grants::fingerprint(lookup, &node) == *pinned)
            })
            .map(|(id, _)| id)
            .collect()
    }

    /// Chooses `node` as it is now, or drops the choice.
    pub fn set(&self, lookup: &impl Lookup, node: &Node, chosen: bool) -> Result<()> {
        if chosen {
            self.0.insert(node.id, grants::fingerprint(lookup, node))
        } else {
            self.0.remove(&node.id)
        }
    }

    /// Drops choices for hosts that no longer exist.
    pub fn forget(&self, nodes: &[NodeId]) {
        self.0.forget(nodes);
    }
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
        let (pinned, problem) = Pinned::<NodeId, String>::load(path.clone(), "hosts");
        assert!(problem.is_some());
        assert!(pinned.entries().is_empty());
        assert!(Pins::load(path).nodes(&|_| Some(node.clone())).is_empty());
    }

    /// The file exactly as Burrow wrote it before the stores were shared,
    /// for a host with a fixed id. Every tester's background monitors are
    /// in such a file, so a change in its shape or in the fingerprint
    /// would drop them all on the next update.
    const OLD_FILE: &str = "[hosts]\n\
        6f1c2d3e-0000-4000-8000-000000000001 = \
        \"c84b2718c2bafbf87c17a74be79c44c6d889577395672e6a1bf597d25e32a374\"\n";

    #[test]
    fn reads_and_writes_the_file_as_burrow_always_has() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("monitoring.toml");
        let node = Node {
            id: NodeId("6f1c2d3e-0000-4000-8000-000000000001".parse().unwrap()),
            ..host("web")
        };
        let lookup = |id| (id == node.id).then(|| node.clone());
        std::fs::write(&path, OLD_FILE).unwrap();
        assert!(Pins::load(path.clone()).has(&lookup, &node));

        std::fs::remove_file(&path).unwrap();
        Pins::load(path.clone()).set(&lookup, &node, true).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), OLD_FILE);
    }
}
