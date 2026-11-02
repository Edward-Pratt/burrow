//! Everything the window can be told: by the core, by its own widgets and by its panels.

use std::collections::BTreeMap;
use std::path::PathBuf;

use burrow_core::ipc::Client;
use burrow_core::{
    AddonInfo, Alert, Event, FormValue, Grant, Node, NodeId, Problem, Service, ServiceAction,
    SessionId, SessionInfo, View,
};

use crate::database::{self};
use crate::desktop::{self};
use crate::files::{self};
use crate::monitor::{self};
use crate::settings::Theme;

use super::nodes::NewKind;
use super::tabs::TabKey;

#[derive(Clone, Debug)]
pub enum AppMsg {
    // From the core.
    Loaded {
        nodes: Vec<Node>,
        problems: Vec<Problem>,
        addons: Vec<AddonInfo>,
        sessions: Vec<SessionInfo>,
        autostart: Vec<NodeId>,
        monitoring: Vec<NodeId>,
        alerts: Vec<Alert>,
    },
    CoreEvent(Event),
    /// The core's sessions, looked up to find whose a prompt is.
    SessionsListed(Vec<SessionInfo>),
    /// We missed events; reload everything.
    Resync,
    /// The daemon went away; [`AppMsg::Reconnected`] follows once a new
    /// connection is up.
    Disconnected,
    Reconnected(Client),
    Failed(String),

    // From the user.
    RowActivated(NodeId),
    /// A context menu for a sidebar row, or for the empty space when `node`
    /// is `None`, pointing at `x`, `y` in the sidebar pane.
    ContextMenu {
        node: Option<NodeId>,
        x: f64,
        y: f64,
    },
    /// `parent: None` means "next to the current selection".
    New {
        kind: NewKind,
        parent: Option<Option<NodeId>>,
    },
    Open(NodeId),
    Rename(NodeId),
    Delete(NodeId),
    /// Moves a node into a folder, or to the top level.
    MoveNode {
        id: NodeId,
        parent: Option<NodeId>,
    },
    /// Asks which folder to move a node to.
    MoveTo(NodeId),
    /// Asks which colour to mark a node with.
    Colour(NodeId),
    RenameSelected,
    DeleteSelected,
    /// Moves the keyboard focus to the sidebar, showing it if it's hidden.
    FocusSidebar,
    CloseTab(TabKey),
    CloseCurrentTab,
    /// Moves to the next (`1`) or previous (`-1`) tab, wrapping around.
    SwitchTab(i32),
    /// Opens a local terminal; `None` runs the login shell.
    NewTerminal(Option<String>),
    TerminalTitle {
        id: u32,
        title: String,
    },
    TerminalExited(u32),
    /// The core accepted `open_terminal` for an SSH shell tab.
    SessionStarted {
        tab: TabKey,
        session: SessionId,
    },
    /// `open_terminal` itself failed.
    SessionFailed {
        tab: TabKey,
        error: String,
    },
    SessionOutput {
        session: SessionId,
        data: Vec<u8>,
    },
    SessionOutputEnded {
        session: SessionId,
    },
    /// Typed into an SSH shell tab.
    RemoteInput {
        tab: TabKey,
        data: Vec<u8>,
    },
    RemoteResize {
        tab: TabKey,
        cols: u16,
        rows: u16,
    },
    /// The title the shell on an SSH host set, as it arrived.
    RemoteTitle {
        tab: TabKey,
        title: String,
    },
    /// Opens another shell on an SSH host.
    NewShell(NodeId),
    /// Opens an SSH host's file panel.
    OpenFiles(NodeId),
    /// Opens an SSH host's monitor.
    OpenMonitor(NodeId),
    /// For the monitor of an SSH host.
    Monitor {
        node: NodeId,
        msg: monitor::Msg,
    },
    /// For the file panel of an SSH host.
    Files {
        node: NodeId,
        msg: files::Msg,
    },
    StartTunnel(NodeId),
    StopTunnel(NodeId),
    /// Stops the tunnel and starts it again once it has closed.
    RestartTunnel(NodeId),
    EditTunnel(NodeId),
    /// For the tab of a database node.
    Database {
        node: NodeId,
        msg: database::Msg,
    },
    EditDatabase(NodeId),
    /// For the tab of a VNC desktop node.
    Desktop {
        node: NodeId,
        msg: desktop::Msg,
    },
    EditDesktop(NodeId),
    /// Asks what the MCP bridge may do on a node.
    EditGrant(NodeId),
    GrantLoaded {
        node: NodeId,
        grant: Option<Grant>,
    },
    SetAutostart {
        node: NodeId,
        enabled: bool,
    },
    /// `set_monitoring` failed, so a monitor's checkbox is wrong.
    MonitoringFailed(String),
    /// `set_autostart` failed, so the tunnel's checkbox is wrong.
    AutostartFailed {
        node: NodeId,
        error: String,
    },
    /// (Re)loads an addon node's view.
    LoadView(NodeId),
    ViewLoaded {
        node: NodeId,
        result: Result<View, String>,
    },
    /// Sends an action to an addon.
    Invoke {
        addon: String,
        action: String,
        node: Option<NodeId>,
        values: BTreeMap<String, FormValue>,
    },
    /// An addon's reply to `Invoke`, shown in the status bar.
    Notice(String),
    ClearNotice(u32),
    /// Empties the status bar's list of errors.
    ClearErrors,
    /// The window was asked to close.
    CloseRequested,
    /// The user picked an action on a service in a monitor. Those that
    /// interrupt it ask first.
    Control {
        node: NodeId,
        service: Service,
        action: ServiceAction,
    },
    /// An action to run now, asked about if it needed to be.
    ControlNow {
        node: NodeId,
        service: Service,
        action: ServiceAction,
    },
    /// How an action went: what it printed, and whether sudo wanted a
    /// password.
    Controlled {
        node: NodeId,
        service: Service,
        action: ServiceAction,
        result: Result<(burrow_core::ExecResult, bool), String>,
    },
    /// Follows a service's log in a tab.
    ShowLogs {
        node: NodeId,
        service: Service,
    },
    /// A tab was dragged to another place.
    TabsMoved,
    ManageAddons,
    ReviewAddon(String),
    RevokeAddon(String),
    OpenAddonsFolder,
    ShowPalette,
    ToggleSidebar,
    ShowSettings,
    CheckForUpdates,
    /// Closes the window the usual way, then starts this program, the
    /// updated window, in its place.
    Restart(PathBuf),
    SetTheme(Theme),
    /// `None` goes back to the system's monospace font.
    SetTerminalFont(Option<String>),
    ReloadConfig,
    OpenConfigFolder,
}

impl AppMsg {
    /// Whether the user did this, rather than the core or a timer. A user
    /// action clears the error in the status bar, which nothing else does
    /// but a newer error: events arrive every few seconds and would wipe
    /// it before it could be read. Messages that can come either way, such
    /// as a panel's own messages, don't count.
    pub(super) fn is_user_action(&self) -> bool {
        matches!(
            self,
            AppMsg::RowActivated(_)
                | AppMsg::ContextMenu { .. }
                | AppMsg::New { .. }
                | AppMsg::Open(_)
                | AppMsg::Rename(_)
                | AppMsg::MoveNode { .. }
                | AppMsg::MoveTo(_)
                | AppMsg::Colour(_)
                | AppMsg::Delete(_)
                | AppMsg::RenameSelected
                | AppMsg::DeleteSelected
                | AppMsg::FocusSidebar
                | AppMsg::CloseTab(_)
                | AppMsg::CloseCurrentTab
                | AppMsg::SwitchTab(_)
                | AppMsg::NewTerminal(_)
                | AppMsg::NewShell(_)
                | AppMsg::OpenFiles(_)
                | AppMsg::OpenMonitor(_)
                | AppMsg::StartTunnel(_)
                | AppMsg::StopTunnel(_)
                | AppMsg::RestartTunnel(_)
                | AppMsg::EditTunnel(_)
                | AppMsg::EditDatabase(_)
                | AppMsg::EditDesktop(_)
                | AppMsg::EditGrant(_)
                | AppMsg::SetAutostart { .. }
                | AppMsg::Invoke { .. }
                | AppMsg::ClearErrors
                | AppMsg::CloseRequested
                | AppMsg::TabsMoved
                | AppMsg::Control { .. }
                | AppMsg::ControlNow { .. }
                | AppMsg::ShowLogs { .. }
                | AppMsg::ManageAddons
                | AppMsg::ReviewAddon(_)
                | AppMsg::RevokeAddon(_)
                | AppMsg::OpenAddonsFolder
                | AppMsg::ShowPalette
                | AppMsg::ToggleSidebar
                | AppMsg::ShowSettings
                | AppMsg::CheckForUpdates
                | AppMsg::Restart(_)
                | AppMsg::SetTheme(_)
                | AppMsg::SetTerminalFont(_)
                | AppMsg::ReloadConfig
                | AppMsg::OpenConfigFolder
        )
    }
}
