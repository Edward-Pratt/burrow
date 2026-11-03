//! What a command returns, one variant per shape of answer.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::base64_bytes;
use super::{
    AddonInfo, Alert, DbSchema, ExecResult, FileEntry, Grant, HistoryEntry, MonitorHistory, Node,
    NodeId, Problem, QueryResult, SessionId, SessionInfo, TransferId, View, monitor,
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Version {
        api: u32,
    },
    Nodes {
        nodes: Vec<Node>,
    },
    Node {
        node: Node,
    },
    Problems {
        problems: Vec<Problem>,
    },
    Session {
        session: SessionId,
    },
    Sessions {
        sessions: Vec<SessionInfo>,
    },
    /// Tunnel nodes that start with Burrow.
    Autostart {
        nodes: Vec<NodeId>,
    },
    /// SSH hosts monitored in the background.
    Monitoring {
        nodes: Vec<NodeId>,
    },
    Alerts {
        alerts: Vec<Alert>,
    },
    /// Points averaged from a host's samples, oldest first: one a minute,
    /// or one every 5 minutes over a week.
    Trend {
        points: Vec<monitor::Sample>,
    },
    /// A remote folder: its absolute `path` and its entries, sorted by name.
    Dir {
        path: String,
        entries: Vec<FileEntry>,
    },
    Transfer {
        transfer: TransferId,
    },
    Query {
        result: QueryResult,
    },
    Exec {
        result: ExecResult,
    },
    Controlled {
        result: ExecResult,
        /// sudo wanted a password, so nothing was done.
        needs_password: bool,
    },
    Monitor {
        history: MonitorHistory,
    },
    /// The start of a file, its whole size, and the SHA-256 of `data` in
    /// lower-case hex. When `data` is the whole file, that hash can go in
    /// [`Command::WriteFile`](crate::api::Command::WriteFile)'s `expect`.
    File {
        #[serde(with = "base64_bytes")]
        data: Vec<u8>,
        size: u64,
        #[serde(default)]
        sha256: String,
    },
    Grants {
        grants: Vec<Grant>,
    },
    Schema {
        schemas: Vec<DbSchema>,
    },
    /// `rows` rows were written.
    Exported {
        rows: u64,
    },
    History {
        entries: Vec<HistoryEntry>,
    },
    Addons {
        addons: Vec<AddonInfo>,
    },
    View {
        view: View,
    },
    /// `message` is something the addon wants shown to the user.
    Invoked {
        message: Option<String>,
    },
    /// Whether the OpenSSH config at `config` reads the hosts added in
    /// Burrow, and the `line` at its top that would make it.
    SshInclude {
        included: bool,
        config: PathBuf,
        line: String,
    },
    Ok,
}
