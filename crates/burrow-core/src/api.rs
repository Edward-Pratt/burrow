//! The command/event API. These types are the contract between the core and
//! every client, so changes here must bump [`API_VERSION`] when they are not
//! backwards compatible. All of them serialize to JSON for out-of-process
//! clients (addons, CLI, MCP bridge).

use std::collections::BTreeSet;
use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Deserializer, Serialize};
use uuid::Uuid;
use zeroize::Zeroize;

/// Major version of the command/event API.
pub const API_VERSION: u32 = 1;

/// The `kind` of folder nodes. Every other kind is owned by an addon.
pub const FOLDER_KIND: &str = "folder";

/// Stable identity of a node. Stored in the node's file, so it survives
/// renames and moves and can be used in references between nodes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NodeId(pub Uuid);

impl NodeId {
    pub fn new() -> Self {
        NodeId(Uuid::new_v4())
    }
}

impl Default for NodeId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// An item in the sidebar tree: a folder, or a leaf such as an SSH host or a
/// database connection.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Node {
    pub id: NodeId,
    /// `None` for nodes at the top level.
    pub parent: Option<NodeId>,
    /// `"folder"`, or a kind registered by an addon, such as `"ssh-host"`.
    pub kind: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Kind-specific settings, owned by the addon that handles this kind.
    #[serde(default)]
    pub props: toml::Table,
    /// Names of the secret fields that are set. Secret values never leave
    /// the core.
    #[serde(default)]
    pub secrets: BTreeSet<String>,
}

impl Node {
    pub fn is_folder(&self) -> bool {
        self.kind == FOLDER_KIND
    }
}

/// Changes to apply to a node. Fields left as `None` are unchanged.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NodePatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// `Some(None)` (JSON `null`) clears the color.
    #[serde(
        default,
        deserialize_with = "double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub color: Option<Option<String>>,
    /// Replaces all props when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub props: Option<toml::Table>,
}

fn double_option<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

/// A secret passed into the core. Its `Debug` output is redacted and its
/// memory is wiped when dropped.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretValue(String);

impl SecretValue {
    pub fn new(value: impl Into<String>) -> Self {
        SecretValue(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretValue(<redacted>)")
    }
}

impl Drop for SecretValue {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// A config file the core could not load. The rest of the tree still loads.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Problem {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    /// Returns [`Response::Version`].
    Version,
    /// Returns every node as [`Response::Nodes`].
    ListNodes,
    GetNode {
        id: NodeId,
    },
    CreateNode {
        #[serde(default)]
        parent: Option<NodeId>,
        kind: String,
        name: String,
        #[serde(default)]
        color: Option<String>,
        #[serde(default)]
        props: toml::Table,
    },
    UpdateNode {
        id: NodeId,
        patch: NodePatch,
    },
    /// Moves a node into another folder, or to the top level when `parent`
    /// is `None`.
    MoveNode {
        id: NodeId,
        #[serde(default)]
        parent: Option<NodeId>,
    },
    /// Deleting a non-empty folder needs `recursive: true`.
    DeleteNode {
        id: NodeId,
        #[serde(default)]
        recursive: bool,
    },
    /// Stores a secret in the keyring and records a reference to it in the
    /// node's file.
    SetSecret {
        id: NodeId,
        field: String,
        value: SecretValue,
    },
    ClearSecret {
        id: NodeId,
        field: String,
    },
    /// Returns the config files that failed to load.
    ListProblems,
    /// Re-reads the tree from disk. The file watcher does this
    /// automatically.
    ReloadConfig,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Version { api: u32 },
    Nodes { nodes: Vec<Node> },
    Node { node: Node },
    Problems { problems: Vec<Problem> },
    Ok,
}

/// Pushed to subscribers whenever state changes, whether the change came
/// from a command or from a file edited outside Burrow.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    NodeCreated {
        node: Node,
    },
    /// Also sent when a node moves to another folder.
    NodeUpdated {
        node: Node,
    },
    NodeDeleted {
        id: NodeId,
    },
    ProblemsChanged {
        problems: Vec<Problem>,
    },
}
