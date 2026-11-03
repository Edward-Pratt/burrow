//! modular-exempt: one enum, every command of the API in one place with
//! its documentation; it is read as one list.
//!
//! Every command a client can send, as [`Core::execute`](crate::Core::execute)
//! takes it, with what each returns.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::base64_bytes;
use super::{
    Capability, FormValue, GrantShown, NodeId, NodePatch, PromptAnswer, SecretValue, Service,
    ServiceAction, SessionId, TerminalRun, TransferId, TrendRange, TunnelShown,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    /// Returns [`Response::Version`](crate::api::Response::Version).
    Version,
    /// Returns every node as [`Response::Nodes`](crate::api::Response::Nodes).
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
    /// Says whether the OpenSSH config reads `config.d/burrow.conf`, where
    /// hosts added in Burrow go, when it looks up `alias`. Refuses an alias
    /// that [`Command::AddSshHost`] would refuse, so a client asks about
    /// the Include line only for a host it can add. Creates that file,
    /// with only a comment, if it is missing, since `ssh` only reports reading files that
    /// exist. Returns [`Response::SshInclude`](crate::api::Response::SshInclude).
    CheckSshInclude {
        alias: String,
    },
    /// Appends a `Host` block for `alias` to `config.d/burrow.conf` beside
    /// the OpenSSH config, then creates an SSH host node for it named after
    /// the alias. `add_include` allows putting the `Include` line for that
    /// file at the top of the config, which the user must have agreed to.
    /// Refuses an alias the config already names. Returns
    /// [`Response::Node`](crate::api::Response::Node).
    AddSshHost {
        #[serde(default)]
        parent: Option<NodeId>,
        alias: String,
        hostname: String,
        #[serde(default)]
        user: Option<String>,
        #[serde(default)]
        port: Option<u16>,
        #[serde(default)]
        identity_file: Option<PathBuf>,
        #[serde(default)]
        add_include: bool,
    },
    /// Connects to an SSH host node and starts a shell in a terminal of
    /// `cols` x `rows`. Returns [`Response::Session`](crate::api::Response::Session) straight away; progress
    /// arrives as [`Event::SessionChanged`](crate::api::Event::SessionChanged) and output on the session's
    /// output stream ([`Core::take_output`](crate::Core::take_output)).
    OpenTerminal {
        node: NodeId,
        cols: u16,
        rows: u16,
        /// What runs instead of the login shell. The session ends when it
        /// does.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run: Option<TerminalRun>,
    },
    /// Sends keyboard input to the remote shell. `data` is base64 in
    /// JSON, and an array of numbers is still accepted.
    WriteTerminal {
        session: SessionId,
        #[serde(
            serialize_with = "base64_bytes::serialize",
            deserialize_with = "base64_bytes::deserialize_either"
        )]
        data: Vec<u8>,
    },
    ResizeTerminal {
        session: SessionId,
        cols: u16,
        rows: u16,
    },
    AnswerPrompt {
        session: SessionId,
        prompt: u32,
        answer: PromptAnswer,
    },
    /// Disconnects a terminal or stops a tunnel. The session then reports
    /// [`SessionState::Closed`](crate::api::SessionState::Closed).
    CloseSession {
        session: SessionId,
    },
    /// Returns every running session as [`Response::Sessions`](crate::api::Response::Sessions).
    ListSessions,
    /// Starts an SSH tunnel node. Returns [`Response::Session`](crate::api::Response::Session) straight
    /// away, or the tunnel's session if it is already running. Progress
    /// arrives as [`Event::SessionChanged`](crate::api::Event::SessionChanged), as for a terminal.
    StartTunnel {
        node: NodeId,
    },
    /// Opens an SFTP session on an SSH host node, for a file panel. Returns
    /// [`Response::Session`](crate::api::Response::Session) straight away; progress arrives as
    /// [`Event::SessionChanged`](crate::api::Event::SessionChanged), as for a terminal.
    OpenFiles {
        node: NodeId,
    },
    /// Lists a remote folder. An empty `path` is the home folder. Returns
    /// [`Response::Dir`](crate::api::Response::Dir).
    ListDir {
        session: SessionId,
        #[serde(default)]
        path: String,
    },
    MakeDir {
        session: SessionId,
        path: String,
    },
    /// Renames or moves a remote file or folder. Fails if `to` exists.
    RenamePath {
        session: SessionId,
        from: String,
        to: String,
    },
    /// Deletes a remote file, symlink or folder, with everything in it.
    /// Symlinks are removed, never followed.
    RemovePath {
        session: SessionId,
        path: String,
    },
    /// Reads up to `limit` bytes (1 MiB when missing, at most 16 MiB) from
    /// the start of a remote file. Returns [`Response::File`](crate::api::Response::File). For clients
    /// that work with contents rather than local files.
    ReadFile {
        session: SessionId,
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u64>,
    },
    /// Writes `data` (base64 in JSON, at most 16 MiB) to a remote file,
    /// replacing one already there only with `overwrite`. Like an upload, it
    /// goes through a temporary file and never writes through a symlink.
    ///
    /// With `expect`, the SHA-256 a [`Response::File`](crate::api::Response::File) gave for the whole
    /// file, it replaces the file only if it still has that hash, and fails
    /// if it changed or is gone. Then `overwrite` isn't needed.
    WriteFile {
        session: SessionId,
        path: String,
        #[serde(with = "base64_bytes")]
        data: Vec<u8>,
        #[serde(default)]
        overwrite: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expect: Option<String>,
    },
    /// Copies a remote file or folder to `local`, an absolute path that
    /// names the copy itself. Fails if `local` exists, unless `overwrite`
    /// is set: then files are replaced and folders merged. Returns
    /// [`Response::Transfer`](crate::api::Response::Transfer); progress arrives as
    /// [`Event::TransferChanged`](crate::api::Event::TransferChanged).
    Download {
        session: SessionId,
        remote: String,
        local: PathBuf,
        #[serde(default)]
        overwrite: bool,
    },
    /// Copies a local file or folder to `remote`, the same way round as
    /// [`Command::Download`].
    Upload {
        session: SessionId,
        local: PathBuf,
        remote: String,
        #[serde(default)]
        overwrite: bool,
    },
    CancelTransfer {
        transfer: TransferId,
    },
    /// Runs `command` on an SSH host node without a terminal, as
    /// `ssh host command` does, and waits for it. `timeout` covers
    /// connecting too. Returns [`Response::Exec`](crate::api::Response::Exec).
    Exec {
        node: NodeId,
        command: String,
        /// In seconds: 60 when missing, at most 3600.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout: Option<u32>,
    },
    /// Connects to a `vnc` node's desktop through its SSH host and signs
    /// in, asking for the password if the server wants one. Returns
    /// [`Response::Session`](crate::api::Response::Session). Once `connected`, the session's output is the
    /// server's RFB stream, as if from a server that needs no password and
    /// speaks RFB 3.8: the client reads it with `take_output`, writes with
    /// [`Command::WriteTerminal`], and speaks RFB itself.
    OpenVnc {
        node: NodeId,
    },
    /// Starts, stops, restarts, reloads, enables or disables a service on
    /// an SSH host node, and waits for it. A systemd unit's action runs as
    /// root, with `sudo -n` unless the user is root: when sudo wants a
    /// password, it fails with `needs_password`, and the same action can
    /// run in a terminal with [`TerminalRun::Control`], where the user
    /// types it. Returns [`Response::Controlled`](crate::api::Response::Controlled).
    ControlService {
        node: NodeId,
        service: Service,
        action: ServiceAction,
    },
    /// Starts burrow-agent on an SSH host node, putting it there first if
    /// needed, and reports what it sees as [`Event::MonitorSample`](crate::api::Event::MonitorSample),
    /// [`Event::MonitorUnits`](crate::api::Event::MonitorUnits) and [`Event::MonitorApps`](crate::api::Event::MonitorApps). Returns [`Response::Session`](crate::api::Response::Session): the host's
    /// background monitor if it has one, otherwise a new session.
    OpenMonitor {
        node: NodeId,
    },
    /// What a monitor session has seen so far, as [`Response::Monitor`](crate::api::Response::Monitor).
    MonitorHistory {
        session: SessionId,
    },
    /// A background monitor's history on disk over `range`, as
    /// [`Response::Trend`](crate::api::Response::Trend). It reads by host, so it works while no monitor
    /// runs.
    MonitorTrend {
        node: NodeId,
        range: TrendRange,
    },
    /// Returns the SSH hosts monitored in the background, as
    /// [`Response::Monitoring`](crate::api::Response::Monitoring).
    ListMonitoring,
    /// Returns the alerts that hold now, oldest first, as
    /// [`Response::Alerts`](crate::api::Response::Alerts).
    ListAlerts,
    /// Monitors an SSH host in the background, from when Burrow starts, or
    /// stops doing so. Like autostart, the choice is kept on this computer
    /// and only holds while the host's settings stay as they were, and
    /// turning it on needs `expect`, the host as the user was shown it.
    SetMonitoring {
        node: NodeId,
        enabled: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expect: Option<GrantShown>,
    },
    /// Returns the tunnels that start with Burrow, as [`Response::Autostart`](crate::api::Response::Autostart).
    ListAutostart,
    /// Makes a tunnel start with Burrow, or stops it doing so. The choice is
    /// kept on this computer, not in the tunnel's file, and only holds while
    /// the tunnel's settings stay as they were. Enabling needs `expect`,
    /// what the user was shown: if the tunnel or its host differ from it (a
    /// file changed meanwhile), it fails rather than consent to settings
    /// the user never saw.
    SetAutostart {
        node: NodeId,
        enabled: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expect: Option<TunnelShown>,
    },
    /// Returns what the MCP bridge may do, as [`Response::Grants`](crate::api::Response::Grants).
    ListGrants,
    /// Lets the MCP bridge do `capabilities` on `node`, replacing what it
    /// could; none takes everything away. Like autostart, this is kept on
    /// this computer, and granting needs `expect`, what the user was shown.
    /// The MCP bridge itself may not run this.
    SetGrant {
        node: NodeId,
        capabilities: BTreeSet<Capability>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expect: Option<GrantShown>,
    },
    /// Connects to a database node. Returns [`Response::Session`](crate::api::Response::Session) straight
    /// away; progress arrives as [`Event::SessionChanged`](crate::api::Event::SessionChanged), as for a
    /// terminal. The connection starts read-only.
    OpenDatabase {
        node: NodeId,
    },
    /// Runs SQL on a connected database session and waits for it. Each
    /// statement keeps at most `limit` rows (1000 when missing). Returns
    /// [`Response::Query`](crate::api::Response::Query).
    RunQuery {
        session: SessionId,
        sql: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u32>,
    },
    /// Asks the server to stop the query running on the session, if any.
    CancelQuery {
        session: SessionId,
    },
    /// Makes later transactions on the session read-only, or lets them
    /// write.
    SetReadOnly {
        session: SessionId,
        read_only: bool,
    },
    /// Returns the database's schemas, tables and columns as
    /// [`Response::Schema`](crate::api::Response::Schema).
    ListSchema {
        session: SessionId,
    },
    /// Runs SQL in a read-only transaction and writes every row it returns
    /// to `path`, an absolute local path, as CSV. Returns
    /// [`Response::Exported`](crate::api::Response::Exported).
    ExportQuery {
        session: SessionId,
        sql: String,
        path: PathBuf,
    },
    /// Returns the queries run on a database node, oldest first, as
    /// [`Response::History`](crate::api::Response::History).
    ListHistory {
        node: NodeId,
    },
    ClearHistory {
        node: NodeId,
    },
    /// Returns every addon as [`Response::Addons`](crate::api::Response::Addons), sorted by id.
    ListAddons,
    /// Lets an addon run. `fingerprint` must match the one `list_addons`
    /// reported, so a change made meanwhile is not approved unseen.
    ApproveAddon {
        id: String,
        fingerprint: String,
    },
    /// Withdraws an approval and stops the addon.
    RevokeAddon {
        id: String,
    },
    /// Asks the addon that owns the node's kind what to draw for it.
    /// Returns [`Response::View`](crate::api::Response::View). Later versions of the view arrive as
    /// [`Event::ViewChanged`](crate::api::Event::ViewChanged).
    OpenView {
        node: NodeId,
    },
    /// Sends an action to an addon: a palette command (no `node`), a
    /// context-menu action, or a button or form in one of its views.
    /// Returns [`Response::Invoked`](crate::api::Response::Invoked).
    InvokeAction {
        addon: String,
        action: String,
        #[serde(default)]
        node: Option<NodeId>,
        #[serde(default)]
        values: BTreeMap<String, FormValue>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bytes of a `write_terminal` read from `json`.
    fn written(json: &str) -> serde_json::Result<Vec<u8>> {
        match serde_json::from_str(json)? {
            Command::WriteTerminal { data, .. } => Ok(data),
            other => panic!("expected write_terminal, got {other:?}"),
        }
    }

    #[test]
    fn terminal_input_is_sent_as_base64_and_read_from_either() {
        let command = Command::WriteTerminal {
            session: SessionId(3),
            data: b"ls\n".to_vec(),
        };
        let json = serde_json::to_string(&command).unwrap();
        assert_eq!(
            json,
            r#"{"type":"write_terminal","session":3,"data":"bHMK"}"#
        );
        assert_eq!(written(&json).unwrap(), b"ls\n");
        let array = r#"{"type":"write_terminal","session":3,"data":[108,115,10]}"#;
        assert_eq!(written(array).unwrap(), b"ls\n");

        let wrong = [
            r#"{"type":"write_terminal","session":3,"data":[256]}"#,
            r#"{"type":"write_terminal","session":3,"data":"not base64"}"#,
            r#"{"type":"write_terminal","session":3,"data":7}"#,
        ];
        for json in wrong {
            assert!(written(json).is_err(), "{json}");
        }
    }
}
