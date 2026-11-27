//! What each [`Command`] is, decided in one place.
//!
//! The socket server, the engine and the addon host each need to know
//! something about a command: whether it opens a session its client owns,
//! whether it can change what a pinned choice covers, and what the MCP
//! bridge or an addon needs before sending it. All of it comes from one
//! `match` with no catch-all, so a new command doesn't compile until it
//! has been classified, rather than being let through or refused by an
//! arm nobody looked at.

use crate::api::{Capability, Command, NodeId, SessionId};

/// What a command is, as data. The callers make the decisions, with the
/// runtime state only they hold.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Class<'a> {
    /// Opens a session that belongs to the client that sent it, and closes
    /// when that client goes. Tunnels don't: they belong to the daemon.
    pub opens_session: bool,
    /// May change what autostart consents, background monitors and grants
    /// cover, so those are checked again after it runs.
    pub changes_tree: bool,
    /// The running session it acts on. The phone may use only the
    /// sessions it opened, and the terminals it kept.
    pub session: Option<SessionId>,
    pub mcp: McpRule,
    pub addon: AddonRule<'a>,
}

/// What the MCP bridge needs before it may run a command.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum McpRule {
    /// Nothing. Reading the sidebar is free: nodes hold no secrets.
    Free,
    /// The capability on the node.
    Node(NodeId, Capability),
    /// A session the bridge opened, on a node it has the capability on.
    Session(SessionId, Capability),
    /// A session the bridge opened, on a node it has `sql-read` on. The
    /// query may write only with `sql-write`.
    Query(SessionId),
    /// `monitor-read` on the node, and the monitor never installs the
    /// agent.
    Monitor(NodeId),
    /// Nothing, but the reply keeps only the alerts of hosts it has
    /// `monitor-read` on.
    Alerts,
    /// A session the bridge opened, or a tunnel it has `tunnel` on.
    Close(SessionId),
    /// Refused whatever was granted, such as changing nodes, or a command
    /// that takes a local path: the bridge reads and writes contents
    /// instead, so it can't reach the user's own files.
    Denied,
}

/// What an addon needs before it may send a command.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum AddonRule<'a> {
    Free,
    /// `nodes:read`.
    ReadNodes,
    /// `nodes:write`, and the new node's kind must be the addon's own.
    CreateNode(&'a str),
    /// `nodes:write`, and the node's kind must be the addon's own.
    ChangeNode(NodeId),
    /// Refused, such as secrets, sessions and managing addons.
    Denied,
}

pub(crate) fn of(command: &Command) -> Class<'_> {
    use Capability::*;
    // Every arm starts from a command that opens nothing, changes
    // nothing, and is refused to the MCP bridge and to addons.
    let base = Class {
        opens_session: false,
        changes_tree: false,
        session: None,
        mcp: McpRule::Denied,
        addon: AddonRule::Denied,
    };
    let mcp = |mcp| Class { mcp, ..base };
    let changes = |addon| Class {
        changes_tree: true,
        addon,
        ..base
    };
    let opens = |mcp| Class {
        opens_session: true,
        mcp,
        ..base
    };
    let on = |session: &SessionId, class| Class {
        session: Some(*session),
        ..class
    };
    match command {
        Command::Version => Class {
            mcp: McpRule::Free,
            addon: AddonRule::Free,
            ..base
        },
        Command::ListNodes | Command::GetNode { .. } => Class {
            mcp: McpRule::Free,
            addon: AddonRule::ReadNodes,
            ..base
        },
        Command::CreateNode { kind, .. } => changes(AddonRule::CreateNode(kind)),
        Command::UpdateNode { id, .. }
        | Command::MoveNode { id, .. }
        | Command::DeleteNode { id, .. } => changes(AddonRule::ChangeNode(*id)),
        Command::SetSecret { .. }
        | Command::ClearSecret { .. }
        | Command::ListProblems
        | Command::ReloadConfig
        | Command::CheckSshInclude { .. }
        | Command::AddSshHost { .. } => base,

        Command::OpenTerminal { .. } => opens(McpRule::Denied),
        Command::WriteTerminal { session, .. }
        | Command::ResizeTerminal { session, .. }
        | Command::KeepTerminal { session }
        | Command::AnswerPrompt { session, .. } => on(session, base),
        Command::CloseSession { session } => on(session, mcp(McpRule::Close(*session))),
        Command::ListSessions => mcp(McpRule::Free),
        Command::StartTunnel { node } => mcp(McpRule::Node(*node, Tunnel)),

        Command::OpenFiles { node } => opens(McpRule::Node(*node, FilesRead)),
        Command::ListDir { session, .. } | Command::ReadFile { session, .. } => {
            on(session, mcp(McpRule::Session(*session, FilesRead)))
        }
        Command::MakeDir { session, .. }
        | Command::RenamePath { session, .. }
        | Command::RemovePath { session, .. }
        | Command::WriteFile { session, .. } => {
            on(session, mcp(McpRule::Session(*session, FilesWrite)))
        }
        Command::Download { session, .. } | Command::Upload { session, .. } => on(session, base),
        // Its session is the transfer's, which only the daemon knows.
        Command::CancelTransfer { .. } => base,
        // No more than `exec` could run.
        Command::Exec { node, .. } | Command::ControlService { node, .. } => {
            mcp(McpRule::Node(*node, Exec))
        }

        Command::OpenMonitor { node } => opens(McpRule::Monitor(*node)),
        Command::MonitorHistory { session } => {
            on(session, mcp(McpRule::Session(*session, MonitorRead)))
        }
        Command::MonitorTrend { node, .. } => mcp(McpRule::Node(*node, MonitorRead)),
        Command::ListMonitoring => base,
        Command::ListAlerts => mcp(McpRule::Alerts),
        Command::SetMonitoring { .. } => changes(AddonRule::Denied),
        Command::ListAutostart => base,
        Command::SetAutostart { .. } => changes(AddonRule::Denied),
        Command::ListGrants => mcp(McpRule::Free),
        Command::SetGrant { .. } => changes(AddonRule::Denied),

        Command::OpenDatabase { node } => opens(McpRule::Node(*node, SqlRead)),
        // The bridge has no way to see a screen.
        Command::OpenVnc { .. } => opens(McpRule::Denied),
        Command::RunQuery { session, .. } => on(session, mcp(McpRule::Query(*session))),
        Command::CancelQuery { session } | Command::ListSchema { session } => {
            on(session, mcp(McpRule::Session(*session, SqlRead)))
        }
        Command::SetReadOnly { session, .. } => {
            on(session, mcp(McpRule::Session(*session, SqlWrite)))
        }
        Command::ExportQuery { session, .. } => on(session, base),
        Command::ListHistory { .. } | Command::ClearHistory { .. } => base,

        Command::ListAddons
        | Command::ApproveAddon { .. }
        | Command::RevokeAddon { .. }
        | Command::OpenView { .. }
        | Command::InvokeAction { .. } => base,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_paths_and_tunnels_are_classified_as_documented() {
        let session = SessionId(1);
        // The bridge works with contents, never the user's own files.
        for command in [
            Command::Download {
                session,
                remote: "a".into(),
                local: "/tmp/a".into(),
                overwrite: false,
            },
            Command::Upload {
                session,
                local: "/tmp/a".into(),
                remote: "a".into(),
                overwrite: false,
            },
            Command::ExportQuery {
                session,
                sql: "select 1".into(),
                path: "/tmp/a.csv".into(),
            },
        ] {
            let class = of(&command);
            assert_eq!(class.mcp, McpRule::Denied, "{command:?}");
            assert_eq!(class.addon, AddonRule::Denied, "{command:?}");
        }
        // Terminals are never the bridge's, kept or not.
        let keep = Command::KeepTerminal { session };
        let keep = of(&keep);
        assert_eq!(keep.mcp, McpRule::Denied);
        assert!(!keep.opens_session);
        // A tunnel belongs to the daemon and outlives the client.
        let node = NodeId::new();
        assert!(!of(&Command::StartTunnel { node }).opens_session);
        assert!(of(&Command::OpenFiles { node }).opens_session);
    }
}
