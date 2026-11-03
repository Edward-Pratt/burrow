//! Consents kept on this computer: a tunnel as the user was shown it
//! when they made it start with Burrow, and what the MCP bridge may do
//! on a node, with the node as it was shown when that was granted.

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};

use super::{Node, NodeId};

/// A tunnel's settings as a client showed them, for
/// [`Command::SetAutostart`](crate::api::Command::SetAutostart). It covers everything an autostart consent is
/// pinned to: the tunnel's props, and the name and props of the SSH host it
/// goes through, whose alias falls back to its name.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TunnelShown {
    pub props: toml::Table,
    pub host_name: String,
    pub host_props: toml::Table,
}

/// Something the MCP bridge may do on a node. A write grant covers
/// reading too.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Capability {
    /// Read-only SQL on a `postgres` node.
    SqlRead,
    /// SQL that writes.
    SqlWrite,
    /// [`Command::Exec`](crate::api::Command::Exec) on an `ssh-host` node.
    Exec,
    /// Listing and reading files on an `ssh-host` node.
    FilesRead,
    /// Creating, changing and removing them.
    FilesWrite,
    /// Starting and stopping an `ssh-tunnel` node.
    Tunnel,
    /// Monitoring an `ssh-host` node with the burrow-agent the user put
    /// there.
    MonitorRead,
}

impl Capability {
    pub const ALL: [Capability; 7] = [
        Capability::SqlRead,
        Capability::SqlWrite,
        Capability::Exec,
        Capability::FilesRead,
        Capability::FilesWrite,
        Capability::Tunnel,
        Capability::MonitorRead,
    ];
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Capability::SqlRead => "sql-read",
            Capability::SqlWrite => "sql-write",
            Capability::Exec => "exec",
            Capability::FilesRead => "files-read",
            Capability::FilesWrite => "files-write",
            Capability::Tunnel => "tunnel",
            Capability::MonitorRead => "monitor-read",
        })
    }
}

/// What the MCP bridge may do on one node, as [`Command::ListGrants`](crate::api::Command::ListGrants)
/// reports it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    pub node: NodeId,
    pub capabilities: BTreeSet<Capability>,
    /// The node, or the one it goes through, changed since it was granted.
    /// The grant does nothing until it is granted again.
    pub stale: bool,
}

/// A node as a client showed it when asking to grant it, for
/// [`Command::SetGrant`](crate::api::Command::SetGrant): its name and props, and those of the node it goes
/// through (`props.via`), if any.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GrantShown {
    pub name: String,
    pub props: toml::Table,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<Box<GrantShown>>,
}

impl GrantShown {
    /// What to show of `node`, looking the node it goes through up in
    /// `nodes`.
    pub fn of(node: &Node, nodes: &[Node]) -> GrantShown {
        crate::grants::shown(&|id| nodes.iter().find(|n| n.id == id).cloned(), node)
    }
}
