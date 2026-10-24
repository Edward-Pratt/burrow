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

/// Major version of the command/event API.
pub const API_VERSION: u32 = 1;

/// The `kind` of folder nodes. Every other kind is owned by an addon.
pub const FOLDER_KIND: &str = "folder";

/// The `kind` of SSH host nodes. `props.alias` names the `Host` in the
/// user's OpenSSH config; the node's name is used when it is missing.
pub const SSH_HOST_KIND: &str = "ssh-host";

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

/// Identifies a session opened with [`Command::OpenTerminal`]. Unique for
/// as long as the core runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

/// A question a session needs answered before it can go on, such as
/// whether to trust a new host key, or a password.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Prompt {
    /// Unique within its session.
    pub id: u32,
    pub kind: PromptKind,
    pub title: String,
    pub message: String,
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
    /// Disconnects. The session then reports [`SessionState::Closed`].
    CloseSession {
        session: SessionId,
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
    /// The session waits for [`Command::AnswerPrompt`].
    PromptRequested {
        session: SessionId,
        prompt: Prompt,
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
