//! What the MCP bridge may do, node by node.
//!
//! Grants live in the state folder, apart from the node files, for the same
//! reason autostart consents do: node files can arrive with a `git pull`,
//! and nothing in them may grant anything. Each grant is pinned to a
//! fingerprint of the node as the user saw it, and of the node it goes
//! through (`props.via`), so a node file that keeps its id but points
//! somewhere else withdraws the grant instead of carrying it along.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use data_encoding::HEXLOWER;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::api::{
    Capability, Grant, GrantShown, Node, NodeId, POSTGRES_KIND, SSH_HOST_KIND, SSH_TUNNEL_KIND,
};
use crate::error::{Error, Result};
use crate::tree::write_atomic;

/// Looks a node up by id.
pub(crate) trait Lookup: Fn(NodeId) -> Option<Node> {}
impl<F: Fn(NodeId) -> Option<Node>> Lookup for F {}

pub(crate) struct Grants {
    path: PathBuf,
    saved: Mutex<Saved>,
}

/// The state file.
#[derive(Default, Serialize, Deserialize)]
struct Saved {
    #[serde(default)]
    nodes: BTreeMap<NodeId, Saving>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Saving {
    capabilities: BTreeSet<Capability>,
    fingerprint: String,
}

impl Grants {
    /// Reads the state file. A broken one is logged and treated as empty:
    /// nothing is granted until the user grants it again.
    pub fn load(path: PathBuf) -> Grants {
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
        Grants {
            path,
            saved: Mutex::new(saved),
        }
    }

    /// Every grant on a node that still exists.
    pub fn list(&self, lookup: &impl Lookup) -> Vec<Grant> {
        let saved = self.saved.lock().unwrap();
        saved
            .nodes
            .iter()
            .filter_map(|(id, saving)| {
                let node = lookup(*id)?;
                Some(Grant {
                    node: *id,
                    capabilities: saving.capabilities.clone(),
                    stale: fingerprint(lookup, &node) != saving.fingerprint,
                })
            })
            .collect()
    }

    /// Whether `node` is granted `capability` and is still as it was when
    /// granted. A write grant covers reading too.
    pub fn allows(&self, lookup: &impl Lookup, node: NodeId, capability: Capability) -> bool {
        let saved = self.saved.lock().unwrap();
        let Some(saving) = saved.nodes.get(&node) else {
            return false;
        };
        let covered = match capability {
            Capability::SqlRead => [Capability::SqlRead, Capability::SqlWrite]
                .iter()
                .any(|c| saving.capabilities.contains(c)),
            Capability::FilesRead => [Capability::FilesRead, Capability::FilesWrite]
                .iter()
                .any(|c| saving.capabilities.contains(c)),
            other => saving.capabilities.contains(&other),
        };
        covered && lookup(node).is_some_and(|n| fingerprint(lookup, &n) == saving.fingerprint)
    }

    /// Grants `capabilities` on `node`, replacing what it had; none revokes.
    /// Granting needs `expect`, what the user was shown. If the node or the
    /// one it goes through differs from it, the grant fails rather than
    /// cover settings the user never saw.
    pub fn set(
        &self,
        lookup: &impl Lookup,
        node: &Node,
        capabilities: BTreeSet<Capability>,
        expect: Option<&GrantShown>,
    ) -> Result<()> {
        let mut saved = self.saved.lock().unwrap();
        if capabilities.is_empty() {
            saved.nodes.remove(&node.id);
            return save(&self.path, &saved);
        }
        for capability in &capabilities {
            if !capability.fits(&node.kind) {
                return Err(Error::invalid(
                    "capabilities",
                    format!("{capability} does not apply to a {} node", node.kind),
                ));
            }
        }
        let expect = expect.ok_or_else(|| {
            Error::invalid("expect", "granting needs the settings the user was shown")
        })?;
        let current = fingerprint(lookup, node);
        if shown_fingerprint(&node.kind, expect) != current {
            return Err(Error::invalid(
                "expect",
                format!("{} changed after it was shown; show it again", node.name),
            ));
        }
        saved.nodes.insert(
            node.id,
            Saving {
                capabilities,
                fingerprint: current,
            },
        );
        save(&self.path, &saved)
    }
}

impl Capability {
    /// Whether the capability means anything on a node of `kind`.
    fn fits(self, kind: &str) -> bool {
        match self {
            Capability::SqlRead | Capability::SqlWrite => kind == POSTGRES_KIND,
            Capability::Exec | Capability::FilesRead | Capability::FilesWrite => {
                kind == SSH_HOST_KIND
            }
            Capability::Tunnel => kind == SSH_TUNNEL_KIND,
        }
    }
}

/// What a client shows of `node` when asking to grant it, and what the
/// grant is pinned to.
pub(crate) fn shown(lookup: &impl Lookup, node: &Node) -> GrantShown {
    GrantShown {
        name: node.name.clone(),
        props: node.props.clone(),
        via: via(lookup, node).map(|via| {
            Box::new(GrantShown {
                name: via.name,
                props: via.props,
                via: None,
            })
        }),
    }
}

/// The node `node` goes through, when its `props.via` names one.
fn via(lookup: &impl Lookup, node: &Node) -> Option<Node> {
    let via = node.props.get("via")?.as_str()?.parse().ok()?;
    lookup(NodeId(via))
}

fn fingerprint(lookup: &impl Lookup, node: &Node) -> String {
    shown_fingerprint(&node.kind, &shown(lookup, node))
}

/// A digest of everything that decides where a node leads. The name is in
/// it because an SSH host's alias falls back to its name.
fn shown_fingerprint(kind: &str, shown: &GrantShown) -> String {
    let text = serde_json::json!([kind, shown]).to_string();
    HEXLOWER.encode(&Sha256::digest(text.as_bytes()))
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

    fn node(kind: &str, name: &str, props: &str) -> Node {
        Node {
            id: NodeId::new(),
            parent: None,
            kind: kind.into(),
            name: name.into(),
            color: None,
            props: toml::from_str(props).unwrap(),
            secrets: BTreeSet::new(),
        }
    }

    fn caps(list: &[Capability]) -> BTreeSet<Capability> {
        list.iter().copied().collect()
    }

    #[test]
    fn a_grant_holds_only_while_the_node_is_as_shown() {
        let dir = tempfile::TempDir::new().unwrap();
        let host = node(SSH_HOST_KIND, "web", "alias = \"web\"");
        let db = node(
            POSTGRES_KIND,
            "app",
            &format!(
                "via = \"{}\"\nhost = \"localhost\"\ndatabase = \"app\"",
                host.id
            ),
        );
        let nodes = std::sync::Mutex::new(vec![host.clone(), db.clone()]);
        let lookup = |id| nodes.lock().unwrap().iter().find(|n| n.id == id).cloned();
        let grants = Grants::load(dir.path().join("mcp.toml"));

        // Nothing without the settings the user saw, or with other ones.
        let read = caps(&[Capability::SqlRead]);
        assert!(grants.set(&lookup, &db, read.clone(), None).is_err());
        let mut wrong = shown(&lookup, &db);
        wrong.props.insert("database".into(), "other".into());
        assert!(
            grants
                .set(&lookup, &db, read.clone(), Some(&wrong))
                .is_err()
        );

        grants
            .set(&lookup, &db, read, Some(&shown(&lookup, &db)))
            .unwrap();
        assert!(grants.allows(&lookup, db.id, Capability::SqlRead));
        assert!(!grants.allows(&lookup, db.id, Capability::SqlWrite));

        // It survives a restart.
        let grants = Grants::load(dir.path().join("mcp.toml"));
        assert!(grants.allows(&lookup, db.id, Capability::SqlRead));

        // Pointing the host it goes through elsewhere withdraws it.
        nodes.lock().unwrap()[0]
            .props
            .insert("alias".into(), "evil".into());
        assert!(!grants.allows(&lookup, db.id, Capability::SqlRead));
        assert!(grants.list(&lookup)[0].stale);
    }

    #[test]
    fn grants_fit_the_kind_and_write_covers_read() {
        let dir = tempfile::TempDir::new().unwrap();
        let host = node(SSH_HOST_KIND, "web", "");
        let lookup = |id| (id == host.id).then(|| host.clone());
        let grants = Grants::load(dir.path().join("mcp.toml"));
        let shown = shown(&lookup, &host);

        let sql = caps(&[Capability::SqlRead]);
        assert!(grants.set(&lookup, &host, sql, Some(&shown)).is_err());

        let write = caps(&[Capability::FilesWrite]);
        grants.set(&lookup, &host, write, Some(&shown)).unwrap();
        assert!(grants.allows(&lookup, host.id, Capability::FilesRead));
        assert!(!grants.allows(&lookup, host.id, Capability::Exec));

        grants.set(&lookup, &host, BTreeSet::new(), None).unwrap();
        assert!(!grants.allows(&lookup, host.id, Capability::FilesWrite));
        assert!(grants.list(&lookup).is_empty());
    }
}
