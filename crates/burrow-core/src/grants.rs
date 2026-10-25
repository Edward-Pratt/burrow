//! What the MCP bridge may do, node by node.
//!
//! Grants live in the state folder, apart from the node files, for the same
//! reason autostart consents do: node files can arrive with a `git pull`,
//! and nothing in them may grant anything. Each grant is pinned to a
//! fingerprint of the node as the user saw it, and of the node it goes
//! through (`props.via`), so a node file that keeps its id but points
//! somewhere else withdraws the grant instead of carrying it along.

use std::collections::BTreeSet;
use std::path::PathBuf;

use data_encoding::HEXLOWER;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::api::{
    Capability, Grant, GrantShown, Node, NodeId, POSTGRES_KIND, SSH_HOST_KIND, SSH_TUNNEL_KIND,
};
use crate::error::{Error, Result};
use crate::pins::{Lookup, Pinned};

/// The state file's `[nodes]` table, by node id.
pub(crate) struct Grants(Pinned<NodeId, Saving>);

#[derive(Clone, Serialize, Deserialize)]
struct Saving {
    capabilities: BTreeSet<Capability>,
    fingerprint: String,
}

impl Grants {
    /// Reads the state file. A broken one is logged and treated as empty:
    /// nothing is granted until the user grants it again.
    pub fn load(path: PathBuf) -> Grants {
        Grants(Pinned::load_or_warn(path, "nodes"))
    }

    /// Every grant on a node that still exists.
    pub fn list(&self, lookup: &impl Lookup) -> Vec<Grant> {
        self.0
            .entries()
            .into_iter()
            .filter_map(|(id, saving)| {
                let node = lookup(id)?;
                Some(Grant {
                    node: id,
                    stale: fingerprint(lookup, &node) != saving.fingerprint,
                    capabilities: saving.capabilities,
                })
            })
            .collect()
    }

    /// Whether `node` is granted `capability` and is still as it was when
    /// granted. A write grant covers reading too.
    pub fn allows(&self, lookup: &impl Lookup, node: NodeId, capability: Capability) -> bool {
        let Some(saving) = self.0.get(&node) else {
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
        if capabilities.is_empty() {
            return self.0.remove(&node.id);
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
        if !matches(lookup, node, expect) {
            return Err(Error::invalid(
                "expect",
                format!("{} changed after it was shown; show it again", node.name),
            ));
        }
        let saving = Saving {
            capabilities,
            fingerprint: fingerprint(lookup, node),
        };
        self.0.insert(node.id, saving)
    }
}

impl Capability {
    /// Whether the capability means anything on a node of `kind`.
    fn fits(self, kind: &str) -> bool {
        match self {
            Capability::SqlRead | Capability::SqlWrite => kind == POSTGRES_KIND,
            Capability::Exec
            | Capability::FilesRead
            | Capability::FilesWrite
            | Capability::MonitorRead => kind == SSH_HOST_KIND,
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

pub(crate) fn fingerprint(lookup: &impl Lookup, node: &Node) -> String {
    shown_fingerprint(&node.kind, &shown(lookup, node))
}

/// Whether `node` is still as `shown` says the user saw it.
pub(crate) fn matches(lookup: &impl Lookup, node: &Node, shown: &GrantShown) -> bool {
    shown_fingerprint(&node.kind, shown) == fingerprint(lookup, node)
}

/// A digest of everything that decides where a node leads. The name is in
/// it because an SSH host's alias falls back to its name.
fn shown_fingerprint(kind: &str, shown: &GrantShown) -> String {
    let text = serde_json::json!([kind, shown]).to_string();
    HEXLOWER.encode(&Sha256::digest(text.as_bytes()))
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

    /// The file exactly as Burrow wrote it before the stores were shared,
    /// for nodes with fixed ids. Every tester's grants are in such a file,
    /// so a change in its shape or in the fingerprint would revoke them
    /// all on the next update.
    const OLD_FILE: &str = "[nodes.6f1c2d3e-0000-4000-8000-000000000001]
capabilities = [\"exec\"]
fingerprint = \"c84b2718c2bafbf87c17a74be79c44c6d889577395672e6a1bf597d25e32a374\"

[nodes.6f1c2d3e-0000-4000-8000-000000000003]
capabilities = [
    \"sql-read\",
    \"sql-write\",
]
fingerprint = \"fb839698daa6551f1da96fd86a96b083bba3bf37deb8afd75e7f00be4bc9a05a\"
";

    #[test]
    fn reads_and_writes_the_file_as_burrow_always_has() {
        let fixed = |n: u8| {
            NodeId(
                format!("6f1c2d3e-0000-4000-8000-00000000000{n}")
                    .parse()
                    .unwrap(),
            )
        };
        let host = Node {
            id: fixed(1),
            ..node(SSH_HOST_KIND, "web", "alias = \"web\"")
        };
        let db = Node {
            id: fixed(3),
            ..node(
                POSTGRES_KIND,
                "app",
                &format!(
                    "via = \"{}\"\nhost = \"localhost\"\ndatabase = \"app\"",
                    host.id
                ),
            )
        };
        let nodes = [host.clone(), db.clone()];
        let lookup = |id| nodes.iter().find(|n| n.id == id).cloned();
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("mcp.toml");
        std::fs::write(&path, OLD_FILE).unwrap();
        let grants = Grants::load(path.clone());
        assert!(grants.allows(&lookup, host.id, Capability::Exec));
        assert!(grants.allows(&lookup, db.id, Capability::SqlWrite));

        std::fs::remove_file(&path).unwrap();
        let grants = Grants::load(path.clone());
        let exec = caps(&[Capability::Exec]);
        let sql = caps(&[Capability::SqlRead, Capability::SqlWrite]);
        grants
            .set(&lookup, &host, exec, Some(&shown(&lookup, &host)))
            .unwrap();
        grants
            .set(&lookup, &db, sql, Some(&shown(&lookup, &db)))
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), OLD_FILE);
    }
}
