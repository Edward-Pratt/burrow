//! What the core pushes to subscribers as state changes.

use serde::{Deserialize, Serialize};

use super::{
    AddonInfo, Alert, Grant, Node, NodeId, Problem, Prompt, SessionId, SessionState, TransferId,
    TransferState, View, monitor,
};

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
    /// A monitor's server has different containers or PM2 apps, or one of
    /// them changed.
    MonitorApps {
        session: SessionId,
        node: NodeId,
        apps: monitor::Apps,
    },
    /// The tunnels that start with Burrow changed.
    AutostartChanged {
        nodes: Vec<NodeId>,
    },
    /// The SSH hosts monitored in the background changed.
    MonitoringChanged {
        nodes: Vec<NodeId>,
    },
    /// Something went wrong on a host monitored in the background.
    AlertRaised {
        alert: Alert,
    },
    /// An alert's condition ended, or its host stopped being monitored.
    AlertCleared {
        alert: Alert,
    },
    /// The session waits for [`Command::AnswerPrompt`](crate::api::Command::AnswerPrompt).
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
