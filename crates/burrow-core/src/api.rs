//! The command/event API. These types are the contract between the core and
//! every client, so changes here must bump [`API_VERSION`] when they are not
//! backwards compatible. All of them serialize to JSON for out-of-process
//! clients (addons, CLI, MCP bridge).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Deserializer, Serialize};
use uuid::Uuid;
use zeroize::Zeroize;

/// What burrow-agent reports, as monitor events carry it.
pub use burrow_agent as monitor;

/// Major version of the command/event API.
pub const API_VERSION: u32 = 1;

/// The `kind` of folder nodes. Every other kind is owned by an addon.
pub const FOLDER_KIND: &str = "folder";

/// The `kind` of SSH host nodes. `props.alias` names the `Host` in the
/// user's OpenSSH config; the node's name is used when it is missing.
pub const SSH_HOST_KIND: &str = "ssh-host";

/// The `kind` of SSH tunnel nodes. Their props say which host to go
/// through and what to forward; see [`Command::StartTunnel`].
pub const SSH_TUNNEL_KIND: &str = "ssh-tunnel";

/// The `kind` of PostgreSQL connection nodes. Their props say where the
/// server is and how to reach it; see [`Command::OpenDatabase`].
pub const POSTGRES_KIND: &str = "postgres";

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

/// Identifies a session opened with [`Command::OpenTerminal`],
/// [`Command::StartTunnel`] or [`Command::OpenFiles`]. Unique for as long as
/// the core runs, and a later session has a larger id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(pub u64);

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SessionState {
    /// Still connecting. `step` says what is happening, such as "Checking
    /// the host key".
    Connecting {
        step: String,
    },
    Connected,
    /// Over. `error` is `None` when the remote shell ended normally.
    Closed {
        error: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    Terminal,
    Tunnel,
    /// An SFTP session, for a file panel.
    Files,
    /// A connection to a database.
    Database,
    /// One command run with [`Command::Exec`].
    Exec,
    /// burrow-agent reporting a server's state, for [`Command::OpenMonitor`].
    Monitor,
}

/// Identifies a download or upload. Unique for as long as the core runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TransferId(pub u64);

impl fmt::Display for TransferId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TransferState {
    /// `done` of `total` bytes so far.
    Running {
        done: u64,
        total: u64,
    },
    Finished,
    Failed {
        error: String,
    },
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileKind {
    File,
    Dir,
    Symlink,
    Other,
}

/// One entry of a remote folder.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    /// As the server sent it. It can hold any character but `/`, including
    /// control characters, so clients must not print it raw.
    pub name: String,
    pub kind: FileKind,
    pub size: u64,
    /// Seconds since the Unix epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified: Option<u64>,
    /// Unix mode bits, such as `0o644`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permissions: Option<u32>,
}

/// A running session, as [`Command::ListSessions`] reports it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session: SessionId,
    /// The SSH host of a terminal, or the tunnel node of a tunnel.
    pub node: NodeId,
    pub kind: SessionKind,
    #[serde(flatten)]
    pub state: SessionState,
}

/// A question a session needs answered before it can go on, such as
/// whether to trust a new host key, or a password.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Prompt {
    /// Unique within its session.
    pub id: u32,
    pub kind: PromptKind,
    pub title: String,
    pub message: String,
    /// For a [`PromptKind::Confirm`], what "yes" does, as a button would
    /// say it ("Install"). Clients pick their own words when it's missing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PromptKind {
    /// Yes or no.
    Confirm,
    /// A hidden answer, such as a password. When `can_save` is set the
    /// client may offer to keep it in the keyring.
    Secret { can_save: bool },
    /// A visible answer.
    Text,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PromptAnswer {
    Cancel,
    /// "Yes" to a [`PromptKind::Confirm`].
    Confirm,
    /// The answer to a `Secret` or `Text` prompt. `save` asks the core to
    /// keep a `Secret` answer once it has worked.
    Text {
        value: SecretValue,
        #[serde(default)]
        save: bool,
    },
}

/// An addon found in the `addons/` folder, as [`Command::ListAddons`]
/// reports it. Everything but `state` comes from its `addon.toml`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AddonInfo {
    pub id: String,
    pub name: String,
    pub version: String,
    /// What the addon may do through the API, such as `"nodes:read"`.
    pub permissions: Vec<String>,
    /// Node kinds the addon owns. Only it can create or change their nodes,
    /// and it draws their tabs.
    pub kinds: Vec<AddonKind>,
    /// Palette commands.
    pub commands: Vec<AddonCommand>,
    /// Sidebar context-menu actions.
    pub actions: Vec<AddonAction>,
    /// The program that runs, resolved inside the addon's folder.
    pub program: PathBuf,
    /// A hash of everything an approval covers: the id, permissions, kinds,
    /// program path and arguments, and every file in the addon's folder.
    /// [`Command::ApproveAddon`] must repeat it, so the user approves
    /// exactly what they were shown.
    pub fingerprint: String,
    #[serde(flatten)]
    pub state: AddonState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddonKind {
    pub kind: String,
    /// Shown in "New …" menus, such as "PostgreSQL database".
    pub label: String,
    /// A symbolic icon name from the icon theme.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddonCommand {
    pub id: String,
    pub title: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddonAction {
    pub id: String,
    pub title: String,
    /// The node kinds it is offered on.
    pub kinds: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum AddonState {
    /// Never approved, or `changed` since it was: other files, permissions
    /// or kinds. It does not run until approved.
    NeedsApproval {
        changed: bool,
    },
    /// Approved. It starts when first used.
    Ready,
    Running,
    /// It stopped by itself or broke the protocol. It is started again on
    /// the next use, after a growing delay.
    Failed {
        error: String,
    },
}

/// What an addon draws for one of its nodes. The client turns the blocks
/// into native widgets, top to bottom.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct View {
    pub blocks: Vec<Block>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    Heading {
        text: String,
    },
    Text {
        text: String,
    },
    /// Label and value pairs.
    Fields {
        fields: Vec<Field>,
    },
    Table {
        columns: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    /// Submitting sends `action` with the field values to the addon.
    Form {
        action: String,
        fields: Vec<FormField>,
        submit: String,
    },
    /// Each button sends its `action` to the addon.
    Buttons {
        buttons: Vec<Button>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Field {
    pub label: String,
    pub value: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FormField {
    /// The key of its value in [`Command::InvokeAction`]'s `values`.
    pub name: String,
    pub label: String,
    #[serde(flatten)]
    pub input: FormInput,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "input", rename_all = "snake_case")]
pub enum FormInput {
    Text {
        #[serde(default)]
        value: String,
    },
    Multiline {
        #[serde(default)]
        value: String,
    },
    Check {
        #[serde(default)]
        value: bool,
    },
    /// One of `options`.
    Choice {
        options: Vec<String>,
        #[serde(default)]
        value: String,
    },
}

/// A form field's value: a bool for `check`, text for the rest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FormValue {
    Bool(bool),
    Text(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Button {
    pub action: String,
    pub label: String,
}

/// A tunnel's settings as a client showed them, for
/// [`Command::SetAutostart`]. It covers everything an autostart consent is
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
    /// [`Command::Exec`] on an `ssh-host` node.
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

/// What the MCP bridge may do on one node, as [`Command::ListGrants`]
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
/// [`Command::SetGrant`]: its name and props, and those of the node it goes
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

/// What a monitor has seen, for [`Command::MonitorHistory`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MonitorHistory {
    /// Whether the server runs systemd, so whether `units` will come.
    pub systemd: bool,
    /// The samples of about the last hour, oldest first.
    pub samples: Vec<monitor::Sample>,
    /// The services, as last listed.
    pub units: Option<Vec<monitor::Unit>>,
}

/// What a query sent with [`Command::RunQuery`] returned. A query can hold
/// What a command run with [`Command::Exec`] did. Output is decoded as
/// UTF-8, with invalid bytes replaced, and each stream is cut at 1 MiB.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecResult {
    /// The exit status, when the command exited.
    pub status: Option<u32>,
    /// The signal that ended it, when one did.
    pub signal: Option<String>,
    pub stdout: String,
    pub stderr: String,
    /// Output went past the limit and the rest was dropped.
    pub truncated: bool,
    /// The command was still running at the timeout. It was sent SIGTERM
    /// (on servers that take signals) and the output so far is here.
    pub timed_out: bool,
}

/// several statements; each one that finished has an entry in
/// `statements`, in order. If one failed, `error` says why and the
/// statements after it did not run.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryResult {
    pub statements: Vec<StatementResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<QueryError>,
    /// Set when Burrow stopped the query because it returned more rows
    /// than the limit. The statement that hit it is `truncated`, and
    /// anything after it in the same query did not run.
    #[serde(default)]
    pub stopped: bool,
    pub elapsed_ms: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatementResult {
    /// Empty for a statement that returns no rows, such as an `UPDATE`.
    pub columns: Vec<String>,
    /// Every value as the server prints it, `None` for NULL. Values can
    /// hold any character, so clients must clean them before showing them.
    pub rows: Vec<Vec<Option<String>>>,
    /// Rows the statement changed or returned, as the server reported it.
    /// Zero when the statement was stopped.
    pub count: u64,
    /// More rows were left out than `rows` holds.
    #[serde(default)]
    pub truncated: bool,
}

/// An error the database server reported.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryError {
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// Where in the query the error is, counted in characters from 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<u32>,
    /// The SQLSTATE code, such as `42P01`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

/// A schema and the tables, views and other relations in it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DbSchema {
    pub name: String,
    pub tables: Vec<DbTable>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DbTable {
    pub name: String,
    pub kind: DbTableKind,
    pub columns: Vec<DbColumn>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DbTableKind {
    Table,
    View,
    MaterializedView,
    ForeignTable,
    PartitionedTable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DbColumn {
    pub name: String,
    /// As the server spells it, such as `character varying(40)`.
    pub data_type: String,
    pub nullable: bool,
}

/// A query run on a database node, kept on this computer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub sql: String,
    /// When it last ran, in seconds since the Unix epoch.
    pub at: u64,
    /// Whether it ran without an error.
    pub ok: bool,
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
    /// Says whether the OpenSSH config reads `config.d/burrow.conf`, where
    /// hosts added in Burrow go, when it looks up `alias`. Refuses an alias
    /// that [`Command::AddSshHost`] would refuse, so a client asks about
    /// the Include line only for a host it can add. Creates that file,
    /// with only a comment, if it is missing, since `ssh` only reports reading files that
    /// exist. Returns [`Response::SshInclude`].
    CheckSshInclude {
        alias: String,
    },
    /// Appends a `Host` block for `alias` to `config.d/burrow.conf` beside
    /// the OpenSSH config, then creates an SSH host node for it named after
    /// the alias. `add_include` allows putting the `Include` line for that
    /// file at the top of the config, which the user must have agreed to.
    /// Refuses an alias the config already names. Returns
    /// [`Response::Node`].
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
    /// `cols` x `rows`. Returns [`Response::Session`] straight away; progress
    /// arrives as [`Event::SessionChanged`] and output on the session's
    /// output stream ([`Core::take_output`](crate::Core::take_output)).
    OpenTerminal {
        node: NodeId,
        cols: u16,
        rows: u16,
    },
    /// Sends keyboard input to the remote shell.
    WriteTerminal {
        session: SessionId,
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
    /// [`SessionState::Closed`].
    CloseSession {
        session: SessionId,
    },
    /// Returns every running session as [`Response::Sessions`].
    ListSessions,
    /// Starts an SSH tunnel node. Returns [`Response::Session`] straight
    /// away, or the tunnel's session if it is already running. Progress
    /// arrives as [`Event::SessionChanged`], as for a terminal.
    StartTunnel {
        node: NodeId,
    },
    /// Opens an SFTP session on an SSH host node, for a file panel. Returns
    /// [`Response::Session`] straight away; progress arrives as
    /// [`Event::SessionChanged`], as for a terminal.
    OpenFiles {
        node: NodeId,
    },
    /// Lists a remote folder. An empty `path` is the home folder. Returns
    /// [`Response::Dir`].
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
    /// Copies a remote file or folder to `local`, an absolute path that
    /// names the copy itself. Fails if `local` exists, unless `overwrite`
    /// is set: then files are replaced and folders merged. Returns
    /// [`Response::Transfer`]; progress arrives as
    /// [`Event::TransferChanged`].
    /// Reads up to `limit` bytes (1 MiB when missing, at most 16 MiB) from
    /// the start of a remote file. Returns [`Response::File`]. For clients
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
    WriteFile {
        session: SessionId,
        path: String,
        #[serde(with = "base64_bytes")]
        data: Vec<u8>,
        #[serde(default)]
        overwrite: bool,
    },
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
    /// `ssh host command` does, and waits for it. `timeout` (60 s when
    /// missing, at most 3600) covers connecting too. Returns
    /// [`Response::Exec`].
    Exec {
        node: NodeId,
        command: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout: Option<u32>,
    },
    /// Starts burrow-agent on an SSH host node, putting it there first if
    /// needed, and reports what it sees as [`Event::MonitorSample`] and
    /// [`Event::MonitorUnits`]. Returns [`Response::Session`].
    OpenMonitor {
        node: NodeId,
    },
    /// What a monitor session has seen so far, as [`Response::Monitor`].
    MonitorHistory {
        session: SessionId,
    },
    /// Returns the tunnels that start with Burrow, as [`Response::Autostart`].
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
    /// Returns what the MCP bridge may do, as [`Response::Grants`].
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
    /// Connects to a database node. Returns [`Response::Session`] straight
    /// away; progress arrives as [`Event::SessionChanged`], as for a
    /// terminal. The connection starts read-only.
    OpenDatabase {
        node: NodeId,
    },
    /// Runs SQL on a connected database session and waits for it. Each
    /// statement keeps at most `limit` rows (1000 when missing). Returns
    /// [`Response::Query`].
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
    /// [`Response::Schema`].
    ListSchema {
        session: SessionId,
    },
    /// Runs SQL in a read-only transaction and writes every row it returns
    /// to `path`, an absolute local path, as CSV. Returns
    /// [`Response::Exported`].
    ExportQuery {
        session: SessionId,
        sql: String,
        path: PathBuf,
    },
    /// Returns the queries run on a database node, oldest first, as
    /// [`Response::History`].
    ListHistory {
        node: NodeId,
    },
    ClearHistory {
        node: NodeId,
    },
    /// Returns every addon as [`Response::Addons`], sorted by id.
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
    /// Returns [`Response::View`]. Later versions of the view arrive as
    /// [`Event::ViewChanged`].
    OpenView {
        node: NodeId,
    },
    /// Sends an action to an addon: a palette command (no `node`), a
    /// context-menu action, or a button or form in one of its views.
    /// Returns [`Response::Invoked`].
    InvokeAction {
        addon: String,
        action: String,
        #[serde(default)]
        node: Option<NodeId>,
        #[serde(default)]
        values: BTreeMap<String, FormValue>,
    },
}

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
    Monitor {
        history: MonitorHistory,
    },
    /// The start of a file, and its whole size.
    File {
        #[serde(with = "base64_bytes")]
        data: Vec<u8>,
        size: u64,
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
    SessionChanged {
        session: SessionId,
        node: NodeId,
        state: SessionState,
    },
    /// A download or upload moved on. `running` is sent at most a few
    /// times a second.
    TransferChanged {
        transfer: TransferId,
        session: SessionId,
        state: TransferState,
    },
    /// A monitor's server sent new stats, every 2 seconds.
    MonitorSample {
        session: SessionId,
        node: NodeId,
        sample: monitor::Sample,
    },
    /// A monitor's server has different services, or a service changed
    /// state.
    MonitorUnits {
        session: SessionId,
        node: NodeId,
        units: Vec<monitor::Unit>,
    },
    /// The tunnels that start with Burrow changed.
    AutostartChanged {
        nodes: Vec<NodeId>,
    },
    /// The session waits for [`Command::AnswerPrompt`].
    PromptRequested {
        session: SessionId,
        prompt: Prompt,
    },
    /// A prompt stopped waiting: a client answered it, or its session
    /// ended. Clients still showing it should close it.
    PromptClosed {
        session: SessionId,
        prompt: u32,
    },
    /// What the MCP bridge may do changed.
    GrantsChanged {
        grants: Vec<Grant>,
    },
    /// An addon was added, removed, approved, revoked, started or stopped.
    AddonsChanged {
        addons: Vec<AddonInfo>,
    },
    /// The addon that owns `node` redrew its view.
    ViewChanged {
        node: NodeId,
        view: View,
    },
}

/// Bytes as a base64 string in JSON, rather than an array of numbers.
mod base64_bytes {
    use data_encoding::BASE64;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&BASE64.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(deserializer)?;
        BASE64
            .decode(text.as_bytes())
            .map_err(serde::de::Error::custom)
    }
}
