//! Sessions and what happens in them: their identity, state and kind,
//! transfers, remote files, and the prompts a session asks.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::{NodeId, SecretValue};

/// Identifies a session opened with [`Command::OpenTerminal`](crate::api::Command::OpenTerminal),
/// [`Command::StartTunnel`](crate::api::Command::StartTunnel) or [`Command::OpenFiles`](crate::api::Command::OpenFiles). Unique for as long as
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
    /// One command run with [`Command::Exec`](crate::api::Command::Exec).
    Exec,
    /// burrow-agent reporting a server's state, for [`Command::OpenMonitor`](crate::api::Command::OpenMonitor).
    Monitor,
    /// A VNC desktop, for [`Command::OpenVnc`](crate::api::Command::OpenVnc).
    Vnc,
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

/// A running session, as [`Command::ListSessions`](crate::api::Command::ListSessions) reports it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session: SessionId,
    /// The SSH host of a terminal, file, command or monitor session, the
    /// tunnel node of a tunnel, and the database node of a database
    /// session.
    pub node: NodeId,
    pub kind: SessionKind,
    #[serde(flatten)]
    pub state: SessionState,
    /// A monitor the core runs for [`Command::SetMonitoring`](crate::api::Command::SetMonitoring) rather than
    /// for a client. It reconnects by itself, and only turning monitoring
    /// off stops it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub background: bool,
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
