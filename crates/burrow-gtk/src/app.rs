//! The main window: header bar, sidebar tree, tabs and status bar.
//!
//! State flows one way. User actions become core [`Command`]s; the core
//! answers with [`Event`]s; events update `App` and the view is redrawn from
//! it. The UI never edits its node list directly, so it always matches the
//! core, including after edits made outside Burrow.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use burrow_core::ipc::{self, Client, Role};
use burrow_core::{
    AddonInfo, Alert, Capability, Command, Event, FOLDER_KIND, FormValue, Grant, GrantShown, Node,
    NodeId, NodePatch, POSTGRES_KIND, Problem, Prompt, Response, SSH_HOST_KIND, SSH_TUNNEL_KIND,
    SessionId, SessionInfo, SessionKind, SessionState, TunnelShown, View,
};
use relm4::gtk::prelude::*;
use relm4::gtk::{gdk, gio, glib};
use relm4::{Component, ComponentParts, ComponentSender, gtk};
use tokio::sync::broadcast::error::RecvError;
use vte4::TerminalExt;

use crate::addons;
use crate::database::{self, Database};
use crate::dialogs;
use crate::files::{self, Files};
use crate::hosts;
use crate::monitor::{self, Monitor};
use crate::palette::{self, Item};
use crate::remote::{self, Remote, Unclaimed};
use crate::runtime::runtime;
use crate::session;
use crate::settings::{Theme, UiSettings};
use crate::sidebar;
use crate::terminal;
use crate::tunnels;

pub struct Init {
    pub core: Client,
    pub config_dir: PathBuf,
}

/// How long to wait between attempts to reach a daemon that went away.
const RECONNECT_DELAY: Duration = Duration::from_secs(2);

/// How long a message from an addon stays in the status bar.
const NOTICE_TIME: Duration = Duration::from_secs(6);

/// How many errors the status bar's list keeps.
const MAX_ERRORS: usize = 50;

/// What "New …" creates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NewKind {
    Folder,
    SshHost,
    SshTunnel,
    Database,
    /// A kind owned by an addon.
    Addon(String),
}

/// The kinds Burrow knows itself: their id, what "New …" calls them, and
/// what it creates.
const BUILT_IN_KINDS: [(&str, &str, NewKind); 4] = [
    (FOLDER_KIND, "folder", NewKind::Folder),
    (SSH_HOST_KIND, "SSH host", NewKind::SshHost),
    (SSH_TUNNEL_KIND, "SSH tunnel", NewKind::SshTunnel),
    (POSTGRES_KIND, "PostgreSQL database", NewKind::Database),
];

impl NewKind {
    /// The kind of node it creates.
    fn kind(&self) -> &str {
        match self {
            NewKind::Folder => FOLDER_KIND,
            NewKind::SshHost => SSH_HOST_KIND,
            NewKind::SshTunnel => SSH_TUNNEL_KIND,
            NewKind::Database => POSTGRES_KIND,
            NewKind::Addon(kind) => kind,
        }
    }
}

/// How long after "New …" a node of that kind, created in that folder,
/// counts as the one the user made. Its dialog can stay open a while.
const NEW_NODE_WAIT: Duration = Duration::from_secs(600);

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
    /// The core accepted `open_terminal` for an SSH host tab.
    SessionStarted {
        node: NodeId,
        session: SessionId,
    },
    /// `open_terminal` itself failed.
    SessionFailed {
        node: NodeId,
        error: String,
    },
    SessionOutput {
        session: SessionId,
        data: Vec<u8>,
    },
    SessionOutputEnded {
        session: SessionId,
    },
    /// Typed into an SSH host tab.
    RemoteInput {
        node: NodeId,
        data: Vec<u8>,
    },
    RemoteResize {
        node: NodeId,
        cols: u16,
        rows: u16,
    },
    /// The title the shell on an SSH host set, as it arrived.
    RemoteTitle {
        node: NodeId,
        title: String,
    },
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
    ManageAddons,
    ReviewAddon(String),
    RevokeAddon(String),
    OpenAddonsFolder,
    ShowPalette,
    ToggleSidebar,
    ShowSettings,
    SetTheme(Theme),
    ReloadConfig,
    OpenConfigFolder,
}

impl AppMsg {
    /// Whether the user did this, rather than the core or a timer. A user
    /// action clears the error in the status bar, which nothing else does
    /// but a newer error: events arrive every few seconds and would wipe
    /// it before it could be read. Messages that can come either way, such
    /// as a panel's own messages, don't count.
    fn is_user_action(&self) -> bool {
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
                | AppMsg::OpenFiles(_)
                | AppMsg::OpenMonitor(_)
                | AppMsg::StartTunnel(_)
                | AppMsg::StopTunnel(_)
                | AppMsg::RestartTunnel(_)
                | AppMsg::EditTunnel(_)
                | AppMsg::EditDatabase(_)
                | AppMsg::EditGrant(_)
                | AppMsg::SetAutostart { .. }
                | AppMsg::Invoke { .. }
                | AppMsg::ClearErrors
                | AppMsg::CloseRequested
                | AppMsg::ManageAddons
                | AppMsg::ReviewAddon(_)
                | AppMsg::RevokeAddon(_)
                | AppMsg::OpenAddonsFolder
                | AppMsg::ShowPalette
                | AppMsg::ToggleSidebar
                | AppMsg::ShowSettings
                | AppMsg::SetTheme(_)
                | AppMsg::ReloadConfig
                | AppMsg::OpenConfigFolder
        )
    }
}

/// What a tab shows. Node tabs follow their node; terminal tabs belong to
/// no node and are numbered as they open.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TabKey {
    Node(NodeId),
    /// An SSH host's file panel. The host's own tab is its terminal.
    Files(NodeId),
    /// An SSH host's monitor.
    Monitor(NodeId),
    Terminal(u32),
}

impl TabKey {
    fn node(self) -> Option<NodeId> {
        match self {
            TabKey::Node(id) | TabKey::Files(id) | TabKey::Monitor(id) => Some(id),
            TabKey::Terminal(_) => None,
        }
    }
}

struct Tab {
    /// A `gtk::Box` for node tabs, a [`terminal::page`] for terminals.
    page: gtk::Widget,
    title: gtk::Label,
}

pub struct App {
    core: Client,
    config_dir: PathBuf,
    settings: UiSettings,
    nodes: HashMap<NodeId, Node>,
    problems: Vec<Problem>,
    addons: Vec<AddonInfo>,
    /// The error the status bar shows, until the user does something or a
    /// newer error replaces it.
    error: Option<String>,
    /// Recent errors, for the list behind the status bar's errors button.
    errors: ErrorLog,
    /// The daemon went away and a new connection isn't up yet.
    reconnecting: bool,
    /// A message from an addon, and a counter so an old timer does not
    /// clear a newer one.
    notice: Option<String>,
    notice_id: u32,
    tabs: HashMap<TabKey, Tab>,
    next_terminal: u32,
    /// SSH host tabs, by node.
    remotes: HashMap<NodeId, Remote>,
    /// File panels, by SSH host node.
    files: HashMap<NodeId, Files>,
    /// Monitors, by SSH host node.
    monitors: HashMap<NodeId, Monitor>,
    /// Database tabs, by node.
    databases: HashMap<NodeId, Database>,
    /// Tabs drawn by addons: the box their view goes in, by node.
    views: HashMap<NodeId, gtk::Box>,
    unclaimed: Unclaimed,
    /// Whether a `list_sessions` to place an unknown session's prompt is
    /// on its way.
    looking_up: bool,
    /// Every tunnel that has run, by node, whether or not its tab is open.
    tunnels: HashMap<NodeId, tunnels::Status>,
    /// Tunnel tabs: the box their page is drawn in, by node.
    tunnel_pages: HashMap<NodeId, gtk::Box>,
    /// Tunnels to start again once their session has closed.
    restarting: HashSet<NodeId>,
    /// Tunnels that start with Burrow.
    autostart: HashSet<NodeId>,
    /// SSH hosts monitored in the background.
    monitoring: HashSet<NodeId>,
    /// Their alerts that hold now.
    alerts: Vec<Alert>,
    /// Prompt dialogs on screen, by session and prompt id, to close when
    /// another client answers first. A `RefCell` because prompts are asked
    /// while a tab's panel is borrowed.
    prompts: RefCell<HashMap<(SessionId, u32), dialogs::Dismiss>>,
    /// The kind and folder "New …" was last asked for, and when, so the
    /// node it makes is selected when it arrives. The SSH host dialog sends
    /// its command itself, so the reply can't be waited for instead.
    creating: Option<(String, Option<NodeId>, Instant)>,
}

pub struct Widgets {
    sidebar_pane: gtk::Box,
    sidebar: sidebar::Sidebar,
    content: gtk::Stack,
    notebook: gtk::Notebook,
    new_button: gtk::MenuButton,
    status: gtk::Label,
    problems_button: gtk::MenuButton,
    problems_list: gtk::Label,
    errors_button: gtk::MenuButton,
    errors_list: gtk::Label,
}

/// The errors the status bar has shown, oldest first, with the time each
/// arrived. Only the newest is on the status line, and a burst of them
/// would otherwise replace one another before they could be read.
#[derive(Default)]
struct ErrorLog(VecDeque<(String, String)>);

impl ErrorLog {
    fn push(&mut self, time: String, message: String) {
        if self.0.len() == MAX_ERRORS {
            self.0.pop_front();
        }
        self.0.push_back((time, message));
    }

    /// One line per error, newest first.
    fn text(&self) -> String {
        (self.0.iter().rev())
            .map(|(time, message)| format!("{time}  {message}"))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl Component for App {
    type Init = Init;
    type Input = AppMsg;
    type Output = ();
    type CommandOutput = ();
    type Widgets = Widgets;
    type Root = gtk::ApplicationWindow;

    fn init_root() -> Self::Root {
        gtk::ApplicationWindow::builder()
            .title("Burrow")
            .default_width(1100)
            .default_height(720)
            .build()
    }

    fn init(init: Init, window: Self::Root, sender: ComponentSender<Self>) -> ComponentParts<Self> {
        let Init { core, config_dir } = init;
        let settings = UiSettings::load(&config_dir);
        // Registers the SQL editor's widgets and finds its languages.
        sourceview5::init();

        // Subscribe before listing, so nothing that happens in between is
        // missed.
        listen(&core, &sender);
        load(&core, &sender);

        let widgets = build(&window, &sender);
        widgets.sidebar_pane.set_visible(settings.sidebar_visible);
        add_shortcuts(&window, &sender);
        // Only `App` knows what is open, so it decides, and destroys the
        // window itself once it may close.
        window.connect_close_request({
            let sender = sender.clone();
            move |_| {
                sender.input(AppMsg::CloseRequested);
                glib::Propagation::Stop
            }
        });

        let mut model = App {
            core,
            config_dir,
            settings,
            nodes: HashMap::new(),
            problems: Vec::new(),
            addons: Vec::new(),
            error: None,
            errors: ErrorLog::default(),
            reconnecting: false,
            notice: None,
            notice_id: 0,
            tabs: HashMap::new(),
            next_terminal: 1,
            remotes: HashMap::new(),
            files: HashMap::new(),
            monitors: HashMap::new(),
            databases: HashMap::new(),
            views: HashMap::new(),
            unclaimed: Unclaimed::default(),
            looking_up: false,
            tunnels: HashMap::new(),
            tunnel_pages: HashMap::new(),
            restarting: HashSet::new(),
            autostart: HashSet::new(),
            monitoring: HashSet::new(),
            alerts: Vec::new(),
            prompts: RefCell::default(),
            creating: None,
        };
        if let Some(error) = stale_daemon(&model.core) {
            model.fail(error);
        }
        model.apply_theme();
        model.refresh_status(&widgets);
        ComponentParts { model, widgets }
    }

    fn update_with_view(
        &mut self,
        widgets: &mut Widgets,
        message: AppMsg,
        sender: ComponentSender<Self>,
        window: &Self::Root,
    ) {
        if message.is_user_action() {
            self.error = None;
        }
        match message {
            AppMsg::Loaded {
                nodes,
                problems,
                addons,
                sessions,
                autostart,
                monitoring,
                alerts,
            } => {
                let loaded = Loaded {
                    nodes,
                    problems,
                    addons,
                    sessions,
                    autostart,
                    monitoring,
                    alerts,
                };
                self.loaded(loaded, widgets, window, &sender);
            }
            AppMsg::CoreEvent(
                event @ (Event::SessionChanged { .. } | Event::PromptRequested { .. }),
            ) => self.session_event(event, window, &sender),
            AppMsg::CoreEvent(event) => self.apply_event(event, widgets, &sender),
            AppMsg::SessionsListed(sessions) => {
                self.looking_up = false;
                self.take_sessions(sessions, window, &sender);
            }
            AppMsg::Resync => load(&self.core, &sender),
            AppMsg::Disconnected => {
                self.reconnecting = true;
                // It would read as why reconnecting fails. The list keeps it.
                self.error = None;
            }
            AppMsg::Reconnected(client) => self.reconnected(client, window, &sender),
            AppMsg::Failed(error) => self.fail(error),

            AppMsg::RowActivated(id) => self.activate(id, widgets, &sender),
            AppMsg::ContextMenu { node, x, y } => self.context_menu(node, x, y, widgets, &sender),
            AppMsg::New { kind, parent } => {
                let parent = parent.unwrap_or_else(|| self.selection_folder(widgets));
                self.creating = Some((kind.kind().to_string(), parent, Instant::now()));
                self.new_node(kind, parent, window, &sender);
            }
            AppMsg::Open(id) => self.open_tab(id, widgets, &sender),
            AppMsg::Rename(id) => self.rename(id, window, &sender),
            AppMsg::Delete(id) => self.delete(id, window, &sender),
            AppMsg::MoveNode { id, parent } => {
                execute(&self.core, &sender, Command::MoveNode { id, parent });
            }
            AppMsg::MoveTo(id) => self.move_to(id, window, &sender),
            AppMsg::Colour(id) => self.colour(id, window, &sender),
            AppMsg::RenameSelected => {
                if let Some(id) = selected(widgets) {
                    self.rename(id, window, &sender);
                }
            }
            AppMsg::DeleteSelected => {
                if let Some(id) = selected(widgets) {
                    self.delete(id, window, &sender);
                }
            }
            AppMsg::FocusSidebar => {
                if !self.settings.sidebar_visible {
                    self.settings.sidebar_visible = true;
                    widgets.sidebar_pane.set_visible(true);
                    self.save_settings();
                }
                widgets.sidebar.focus();
            }
            AppMsg::CloseTab(key) => self.close_tab(key, widgets),
            AppMsg::CloseCurrentTab => {
                let current = widgets.notebook.nth_page(widgets.notebook.current_page());
                let key = self
                    .tabs
                    .iter()
                    .find(|(_, tab)| current.as_ref() == Some(&tab.page))
                    .map(|(key, _)| *key);
                if let Some(key) = key {
                    self.close_tab(key, widgets);
                }
            }
            AppMsg::SwitchTab(step) => {
                let notebook = &widgets.notebook;
                let pages = notebook.n_pages() as i32;
                if let Some(current) = notebook.current_page().filter(|_| pages > 0) {
                    let next = (current as i32 + step).rem_euclid(pages);
                    notebook.set_current_page(Some(next as u32));
                }
            }
            AppMsg::NewTerminal(shell) => self.open_terminal(shell, widgets, &sender),
            AppMsg::TerminalTitle { id, title } => {
                if let Some(tab) = self.tabs.get(&TabKey::Terminal(id)) {
                    set_tab_title(&tab.title, &title);
                }
            }
            AppMsg::TerminalExited(id) => self.close_tab(TabKey::Terminal(id), widgets),
            AppMsg::SessionStarted { node, session } => {
                match self.remotes.get_mut(&node) {
                    Some(remote) if remote.state == session::State::Starting => {
                        remote.state = session::State::Running(session);
                        // Sizes seen while starting had no session to go to.
                        let (cols, rows) = terminal::size(&remote.terminal);
                        execute(
                            &self.core,
                            &sender,
                            Command::ResizeTerminal {
                                session,
                                cols,
                                rows,
                            },
                        );
                        for event in self.unclaimed.claim(session) {
                            self.session_event(event, window, &sender);
                        }
                    }
                    // The tab was closed while the session started.
                    _ => execute(&self.core, &sender, Command::CloseSession { session }),
                }
            }
            AppMsg::SessionFailed { node, error } => {
                if let Some(remote) = self.remotes.get_mut(&node) {
                    remote.terminal.feed(remote::failure(&error).as_bytes());
                    remote.state = session::State::Closed;
                }
            }
            AppMsg::SessionOutput { session, data } => {
                if let Some(remote) = self.remote_for(session) {
                    remote.terminal.feed(&data);
                }
            }
            AppMsg::SessionOutputEnded { session } => {
                if let Some(remote) = self
                    .remotes
                    .values_mut()
                    .find(|r| r.state == session::State::Running(session))
                {
                    remote.ending.output_ended = true;
                    remote.finish_if_done();
                }
            }
            AppMsg::RemoteInput { node, data } => {
                let Some(remote) = self.remotes.get_mut(&node) else {
                    return;
                };
                match remote.state {
                    session::State::Running(session) => execute(
                        &self.core,
                        &sender,
                        Command::WriteTerminal { session, data },
                    ),
                    session::State::Closed if data.contains(&b'\r') => {
                        remote.terminal.feed(b"\r\n");
                        remote.state = session::State::Starting;
                        remote::start(&self.core, node, &remote.terminal, &sender);
                    }
                    _ => {}
                }
            }
            AppMsg::RemoteResize { node, cols, rows } => {
                if let Some(&Remote {
                    state: session::State::Running(session),
                    ..
                }) = self.remotes.get(&node)
                {
                    let command = Command::ResizeTerminal {
                        session,
                        cols,
                        rows,
                    };
                    execute(&self.core, &sender, command);
                }
            }
            AppMsg::RemoteTitle { node, title } => {
                if let Some(remote) = self.remotes.get_mut(&node) {
                    // The server sets it.
                    remote.title = Some(files::printable(&title));
                    self.set_remote_tooltip(node);
                }
            }
            AppMsg::OpenFiles(id) => self.open_files(id, widgets, &sender),
            AppMsg::Files {
                node,
                msg: files::Msg::Session(session::Msg::Started(session)),
            } => match self.files.get_mut(&node) {
                Some(panel) if panel.session.state == session::State::Starting => {
                    panel.session.state = session::State::Running(session);
                    for event in self.unclaimed.claim(session) {
                        self.session_event(event, window, &sender);
                    }
                }
                // The tab was closed while the session started.
                _ => execute(&self.core, &sender, Command::CloseSession { session }),
            },
            AppMsg::Files { node, msg } => {
                if let Some(panel) = self.files.get_mut(&node) {
                    panel.update(msg, &self.core, window, &sender);
                }
            }
            AppMsg::OpenMonitor(id) => self.open_monitor(id, widgets, &sender),
            AppMsg::Monitor {
                node,
                msg: monitor::Msg::Session(session::Msg::Started(session)),
            } => match self.monitors.get_mut(&node) {
                Some(panel) if panel.session.state == session::State::Starting => {
                    panel.session.state = session::State::Running(session);
                    // A background monitor may have connected long ago,
                    // so no "connected" event is coming.
                    panel.fetch_history(&self.core, &sender);
                    for event in self.unclaimed.claim(session) {
                        self.session_event(event, window, &sender);
                    }
                }
                // The tab was closed while the session started.
                _ => execute(&self.core, &sender, Command::CloseSession { session }),
            },
            AppMsg::Monitor {
                node,
                msg: monitor::Msg::SetBackground(on),
            } => self.set_monitoring(node, on, window, &sender),
            AppMsg::Monitor { node, msg } => {
                if let Some(panel) = self.monitors.get_mut(&node) {
                    panel.update(msg, &self.core, &sender);
                }
            }
            AppMsg::MonitoringFailed(error) => {
                self.fail(error);
                self.refresh_monitors();
            }
            AppMsg::StartTunnel(node) => {
                execute(&self.core, &sender, Command::StartTunnel { node });
            }
            AppMsg::StopTunnel(node) => {
                self.restarting.remove(&node);
                if let Some(session) = self.tunnels.get(&node).and_then(|s| s.running()) {
                    execute(&self.core, &sender, Command::CloseSession { session });
                }
            }
            AppMsg::RestartTunnel(node) => {
                if let Some(session) = self.tunnels.get(&node).and_then(|s| s.running()) {
                    self.restarting.insert(node);
                    execute(&self.core, &sender, Command::CloseSession { session });
                }
            }
            AppMsg::EditTunnel(node) => self.edit_tunnel(Some(node), None, window, &sender),
            AppMsg::Database {
                node,
                msg: database::Msg::Session(session::Msg::Started(session)),
            } => match self.databases.get_mut(&node) {
                Some(panel) if panel.session.state == session::State::Starting => {
                    panel.session.state = session::State::Running(session);
                    for event in self.unclaimed.claim(session) {
                        self.session_event(event, window, &sender);
                    }
                }
                // The tab was closed while the session started.
                _ => execute(&self.core, &sender, Command::CloseSession { session }),
            },
            AppMsg::Database { node, msg } => {
                if let Some(panel) = self.databases.get_mut(&node) {
                    panel.update(msg, &self.core, window, &sender);
                }
            }
            AppMsg::EditDatabase(node) => self.edit_database(Some(node), None, window, &sender),
            AppMsg::EditGrant(node) => {
                let (core, input) = (self.core.clone(), sender.input_sender().clone());
                runtime().spawn(async move {
                    let message = match run(&core, Command::ListGrants).await {
                        Ok(Response::Grants { grants }) => AppMsg::GrantLoaded {
                            node,
                            grant: grants.into_iter().find(|g| g.node == node),
                        },
                        Ok(_) => return,
                        Err(error) => AppMsg::Failed(error),
                    };
                    let _ = input.send(message);
                });
            }
            AppMsg::GrantLoaded { node, grant } => self.edit_grant(node, grant, window, &sender),
            AppMsg::SetAutostart { node, enabled } => {
                // What the tab shows: it is redrawn on every change.
                let expect = self
                    .nodes
                    .get(&node)
                    .and_then(|n| shown(&self.nodes, &n.props));
                let (core, input) = (self.core.clone(), sender.input_sender().clone());
                runtime().spawn(async move {
                    let command = Command::SetAutostart {
                        node,
                        enabled,
                        expect,
                    };
                    if let Err(error) = run(&core, command).await {
                        let _ = input.send(AppMsg::AutostartFailed { node, error });
                    }
                });
            }
            AppMsg::AutostartFailed { node, error } => {
                self.fail(error);
                self.refresh_tunnel(node, &sender);
            }
            AppMsg::LoadView(id) => self.load_view(id, &sender),
            AppMsg::ViewLoaded { node, result } => self.show_view(node, result, &sender),
            AppMsg::Invoke {
                addon,
                action,
                node,
                values,
            } => {
                let command = Command::InvokeAction {
                    addon,
                    action,
                    node,
                    values,
                };
                let (core, input) = (self.core.clone(), sender.input_sender().clone());
                runtime().spawn(async move {
                    let message = match run(&core, command).await {
                        Ok(Response::Invoked {
                            message: Some(message),
                        }) => AppMsg::Notice(message),
                        Ok(_) => return,
                        Err(error) => AppMsg::Failed(error),
                    };
                    let _ = input.send(message);
                });
            }
            AppMsg::Notice(message) => {
                self.notice = Some(message);
                self.notice_id += 1;
                let (id, sender) = (self.notice_id, sender.clone());
                glib::timeout_add_local_once(NOTICE_TIME, move || {
                    sender.input(AppMsg::ClearNotice(id));
                });
            }
            AppMsg::ClearNotice(id) => {
                if id == self.notice_id {
                    self.notice = None;
                }
            }
            AppMsg::ClearErrors => {
                self.errors = ErrorLog::default();
                widgets.errors_button.popdown();
            }
            AppMsg::CloseRequested => self.close_window(window),
            AppMsg::ManageAddons => {
                let review = {
                    let sender = sender.clone();
                    move |id| sender.input(AppMsg::ReviewAddon(id))
                };
                let revoke = {
                    let sender = sender.clone();
                    move |id| sender.input(AppMsg::RevokeAddon(id))
                };
                let open_folder = {
                    let sender = sender.clone();
                    move || sender.input(AppMsg::OpenAddonsFolder)
                };
                addons::manage(window, &self.addons, review, revoke, open_folder);
            }
            AppMsg::ReviewAddon(id) => {
                if let Some(addon) = self.addons.iter().find(|a| a.id == id) {
                    let command = Command::ApproveAddon {
                        id,
                        fingerprint: addon.fingerprint.clone(),
                    };
                    let (core, sender) = (self.core.clone(), sender.clone());
                    addons::review(window, addon, move || {
                        execute(&core, &sender, command.clone())
                    });
                }
            }
            AppMsg::RevokeAddon(id) => {
                execute(&self.core, &sender, Command::RevokeAddon { id });
            }
            AppMsg::OpenAddonsFolder => {
                let folder = gio::File::for_path(&self.core.info().addons_dir);
                gtk::FileLauncher::new(Some(&folder)).launch(
                    Some(window),
                    None::<&gio::Cancellable>,
                    |_| {},
                );
            }
            AppMsg::ShowPalette => self.show_palette(window, &sender),
            AppMsg::ToggleSidebar => {
                self.settings.sidebar_visible = !self.settings.sidebar_visible;
                widgets
                    .sidebar_pane
                    .set_visible(self.settings.sidebar_visible);
                self.save_settings();
            }
            AppMsg::ShowSettings => show_settings(window, self.settings.theme, &sender),
            AppMsg::SetTheme(theme) => {
                self.settings.theme = theme;
                self.apply_theme();
                self.save_settings();
            }
            AppMsg::ReloadConfig => execute(&self.core, &sender, Command::ReloadConfig),
            AppMsg::OpenConfigFolder => {
                let folder = gio::File::for_path(&self.config_dir);
                gtk::FileLauncher::new(Some(&folder)).launch(
                    Some(window),
                    None::<&gio::Cancellable>,
                    |_| {},
                );
            }
        }

        // Tunnels start and stop in many places; checking here, against
        // what the sidebar last showed, catches them all.
        widgets.sidebar.set_running(self.running());
        widgets
            .content
            .set_visible_child_name(if self.tabs.is_empty() {
                "welcome"
            } else {
                "tabs"
            });
        self.refresh_status(widgets);
    }
}

/// What the core sent on load, together, since the window takes it in
/// one go.
struct Loaded {
    nodes: Vec<Node>,
    problems: Vec<Problem>,
    addons: Vec<AddonInfo>,
    sessions: Vec<SessionInfo>,
    autostart: Vec<NodeId>,
    monitoring: Vec<NodeId>,
    alerts: Vec<Alert>,
}

impl App {
    /// Takes the core's state, on connecting and after every change that
    /// needs a full reload.
    fn loaded(
        &mut self,
        loaded: Loaded,
        widgets: &mut Widgets,
        window: &gtk::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        let Loaded {
            nodes,
            problems,
            addons,
            sessions,
            autostart,
            monitoring,
            alerts,
        } = loaded;
        self.nodes = nodes.into_iter().map(|n| (n.id, n)).collect();
        self.problems = problems;
        self.autostart = autostart.into_iter().collect();
        self.monitoring = monitoring.into_iter().collect();
        self.alerts = alerts;
        self.refresh_monitors();
        self.set_addons(addons, widgets, sender);
        // Events may have been missed, so a tunnel that isn't running any
        // more has stopped.
        for status in self.tunnels.values_mut() {
            if let Some(session) = status.running()
                && !sessions.iter().any(|s| s.session == session)
            {
                status.session = Some((session, SessionState::Closed { error: None }));
            }
        }
        self.take_sessions(sessions, window, sender);
        let gone: Vec<TabKey> = self
            .tabs
            .keys()
            .filter(|key| key.node().is_some_and(|id| !self.nodes.contains_key(&id)))
            .copied()
            .collect();
        for key in gone {
            self.close_tab(key, widgets);
        }
        widgets.sidebar.replace(&self.nodes, &self.icons());
    }

    /// Switches to a new daemon after the old one went away. What ran in
    /// the old one is over, and the new one is loaded from scratch.
    fn reconnected(
        &mut self,
        client: Client,
        window: &gtk::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        self.core = client;
        self.reconnecting = false;
        // Why reconnecting failed is over now; the list still has it.
        self.error = None;
        if let Some(error) = stale_daemon(&self.core) {
            self.fail(error);
        }
        // Their sessions are gone, and no answer could reach them.
        for (_, dialog) in self.prompts.get_mut().drain() {
            dialog.dismiss();
        }
        // Sessions ended with the daemon that ran them. Tunnels are settled
        // by the reload; the rest are only known here.
        let ended: Vec<(NodeId, SessionId)> = (self.remotes.iter())
            .map(|(node, r)| (*node, &r.state))
            .chain(self.files.iter().map(|(node, p)| (*node, &p.session.state)))
            .chain(
                self.monitors
                    .iter()
                    .map(|(node, p)| (*node, &p.session.state)),
            )
            .chain(
                self.databases
                    .iter()
                    .map(|(node, p)| (*node, &p.session.state)),
            )
            .filter_map(|(node, state)| match state {
                session::State::Running(session) => Some((node, *session)),
                _ => None,
            })
            .collect();
        for (node, session) in ended {
            let state = SessionState::Closed {
                error: Some("Burrow's core stopped".to_string()),
            };
            let event = Event::SessionChanged {
                session,
                node,
                state,
            };
            self.session_event(event, window, sender);
        }
        listen(&self.core, sender);
        load(&self.core, sender);
        sender.input(AppMsg::Notice("Connected to burrowd again".to_string()));
    }

    /// Whether `node` is the one "New …" in this window was asked for.
    fn made_here(&mut self, node: &Node) -> bool {
        let Some((kind, parent, since)) = &self.creating else {
            return false;
        };
        let made = *kind == node.kind && *parent == node.parent && since.elapsed() < NEW_NODE_WAIT;
        if made {
            self.creating = None;
        }
        made
    }

    /// The nodes the sidebar marks as running: tunnels that are up.
    fn running(&self) -> HashSet<NodeId> {
        self.tunnels
            .iter()
            .filter(|(_, status)| status.is_up())
            .map(|(id, _)| *id)
            .collect()
    }

    fn apply_event(&mut self, event: Event, widgets: &Widgets, sender: &ComponentSender<Self>) {
        match event {
            Event::NodeCreated { node } => {
                widgets.sidebar.insert(&node, &self.icons());
                if self.made_here(&node) {
                    widgets.sidebar.show_new(node.id);
                } else if let Some(parent) = node.parent {
                    // Show where a node made elsewhere went, without taking
                    // the selection.
                    widgets.sidebar.expand(parent);
                }
                self.nodes.insert(node.id, node);
            }
            Event::NodeUpdated { node } => {
                let id = node.id;
                if let Some(tab) = self.tabs.get(&TabKey::Files(id)) {
                    set_tab_title(&tab.title, &files_title(&node.name));
                }
                if let Some(tab) = self.tabs.get(&TabKey::Monitor(id)) {
                    set_tab_title(&tab.title, &monitor_title(&node.name));
                }
                if let Some(tab) = self.tabs.get(&TabKey::Node(id)) {
                    set_tab_title(&tab.title, &node.name);
                    if !self.tunnel_pages.contains_key(&id)
                        && !self.databases.contains_key(&id)
                        && let Some(page) = tab.page.downcast_ref::<gtk::Box>()
                    {
                        fill_page(page, &node, &self.nodes);
                    }
                }
                // A running tunnel keeps the settings it started with.
                if let Some(status) = self.tunnels.get_mut(&id)
                    && status.running().is_some()
                    && self
                        .nodes
                        .get(&id)
                        .is_some_and(|old| old.props != node.props)
                {
                    status.stale = true;
                }
                widgets.sidebar.update(&node, &self.icons());
                self.nodes.insert(id, node);
                // Setting the tab's title replaced its tooltip.
                self.set_remote_tooltip(id);
                // Tunnel pages name the host they go through.
                self.refresh_tunnels(sender);
                // The addon draws from the node, so ask it again.
                if self.views.contains_key(&id) {
                    self.load_view(id, sender);
                }
            }
            Event::NodeDeleted { id } => {
                self.nodes.remove(&id);
                self.views.remove(&id);
                self.tunnels.remove(&id);
                self.restarting.remove(&id);
                self.close_tab(TabKey::Node(id), widgets);
                self.close_tab(TabKey::Files(id), widgets);
                self.close_tab(TabKey::Monitor(id), widgets);
                widgets.sidebar.remove(id);
                self.refresh_tunnels(sender);
            }
            Event::ProblemsChanged { problems } => self.problems = problems,
            // Handled by `session_event`, which needs the window.
            // The grant dialog lists grants when it opens.
            Event::SessionChanged { .. }
            | Event::PromptRequested { .. }
            | Event::GrantsChanged { .. } => {}
            Event::MonitoringChanged { nodes } => {
                self.monitoring = nodes.into_iter().collect();
                self.refresh_monitors();
            }
            Event::AlertRaised { alert } => {
                self.alerts.push(alert);
                self.refresh_monitors();
            }
            Event::AlertCleared { alert } => {
                self.alerts.retain(|a| a.id != alert.id);
                self.refresh_monitors();
            }
            Event::PromptClosed { session, prompt } => {
                if let Some(dialog) = self.prompts.get_mut().remove(&(session, prompt)) {
                    dialog.dismiss();
                }
            }
            Event::AddonsChanged { addons } => self.set_addons(addons, widgets, sender),
            Event::ViewChanged { node, view } => self.show_view(node, Ok(view), sender),
            Event::MonitorSample {
                session, sample, ..
            } => {
                let running = session::State::Running(session);
                if let Some(panel) = self
                    .monitors
                    .values_mut()
                    .find(|p| p.session.state == running)
                {
                    panel.sample(&sample, &self.core, sender);
                }
            }
            Event::MonitorUnits { session, units, .. } => {
                let running = session::State::Running(session);
                if let Some(panel) = self
                    .monitors
                    .values_mut()
                    .find(|p| p.session.state == running)
                {
                    panel.set_units(units);
                }
            }
            Event::AutostartChanged { nodes } => {
                self.autostart = nodes.into_iter().collect();
                self.refresh_tunnels(sender);
            }
            Event::TransferChanged {
                transfer,
                session,
                state,
            } => {
                // A transfer's last events can come after its session
                // ended, so the panel showing it comes first.
                let running = session::State::Running(session);
                let panel = self
                    .files
                    .values_mut()
                    .find(|p| p.has_transfer(transfer) || p.session.state == running);
                if let Some(panel) = panel {
                    panel.transfer_changed(transfer, &state, &self.core, sender);
                }
            }
        }
    }

    /// Takes a new addon list. Tabs whose addon was approved or revoked are
    /// drawn again.
    fn set_addons(
        &mut self,
        addons: Vec<AddonInfo>,
        widgets: &Widgets,
        sender: &ComponentSender<Self>,
    ) {
        let usable = |list: &[AddonInfo], kind: &str| {
            addons::owner(list, kind).map(|a| (a.id.clone(), addons::usable(a)))
        };
        let stale: Vec<NodeId> = self
            .views
            .keys()
            .filter(|id| {
                self.nodes.get(id).is_none_or(|node| {
                    usable(&self.addons, &node.kind) != usable(&addons, &node.kind)
                })
            })
            .copied()
            .collect();
        self.addons = addons;
        for id in stale {
            self.load_view(id, sender);
        }
        widgets
            .new_button
            .set_popover(Some(&menu_popover(self.new_entries(None, ""), sender)));
        widgets.sidebar.set_icons(&self.icons());
    }

    /// The icon for each addon kind.
    fn icons(&self) -> HashMap<String, String> {
        self.addons
            .iter()
            .flat_map(|a| &a.kinds)
            .filter_map(|k| Some((k.kind.clone(), k.icon.clone()?)))
            .collect()
    }

    /// "New …" entries for folders, SSH hosts and the kinds of approved
    /// addons. `suffix` follows each label, such as " here".
    fn new_entries(&self, parent: Option<Option<NodeId>>, suffix: &str) -> Vec<(String, AppMsg)> {
        let mut kinds: Vec<(String, NewKind)> = (BUILT_IN_KINDS.iter())
            .map(|(_, label, kind)| (label.to_string(), kind.clone()))
            .collect();
        for addon in self.addons.iter().filter(|a| addons::usable(a)) {
            for kind in &addon.kinds {
                kinds.push((kind.label.clone(), NewKind::Addon(kind.kind.clone())));
            }
        }
        // Each opens a dialog.
        kinds
            .into_iter()
            .map(|(label, kind)| {
                (
                    format!("New {label}{suffix}…"),
                    AppMsg::New { kind, parent },
                )
            })
            .collect()
    }

    /// What to call a node's kind, such as "SSH host": the raw kind only
    /// when no addon names it.
    fn kind_label(&self, kind: &str) -> String {
        let built_in = BUILT_IN_KINDS.iter().find(|(id, ..)| *id == kind);
        let label = built_in.map(|(_, label, _)| *label).or_else(|| {
            (self.addons.iter().flat_map(|a| &a.kinds))
                .find(|k| k.kind == kind)
                .map(|k| k.label.as_str())
        });
        label.map_or_else(|| kind.to_string(), sentence_case)
    }

    /// Asks the node's addon for its view, or explains why it can't.
    fn load_view(&self, id: NodeId, sender: &ComponentSender<Self>) {
        let (Some(page), Some(node)) = (self.views.get(&id), self.nodes.get(&id)) else {
            return;
        };
        let Some(addon) = addons::owner(&self.addons, &node.kind) else {
            let message = "The addon for this kind of item is no longer installed.";
            addons::render_message(page, &node.name, message, Vec::new());
            return;
        };
        if !addons::usable(addon) {
            let button = gtk::Button::with_label("Review addon…");
            send_on_click(&button, sender, AppMsg::ReviewAddon(addon.id.clone()));
            let message = format!(
                "This item is drawn by the {} addon, which you have not approved.",
                addon.name
            );
            addons::render_message(page, &node.name, &message, vec![button]);
            return;
        }
        let (core, input) = (self.core.clone(), sender.input_sender().clone());
        runtime().spawn(async move {
            let result = match core.execute(Command::OpenView { node: id }).await {
                Ok(Response::View { view }) => Ok(view),
                Ok(other) => Err(format!("unexpected reply {other:?}")),
                Err(e) => Err(e.to_string()),
            };
            let _ = input.send(AppMsg::ViewLoaded { node: id, result });
        });
    }

    fn show_view(&self, id: NodeId, result: Result<View, String>, sender: &ComponentSender<Self>) {
        let (Some(page), Some(node)) = (self.views.get(&id), self.nodes.get(&id)) else {
            return;
        };
        let Some(addon) = addons::owner(&self.addons, &node.kind) else {
            return;
        };
        match result {
            Ok(view) => {
                let (addon, sender) = (addon.id.clone(), sender.clone());
                let on_action: addons::OnAction = Rc::new(move |action, values| {
                    sender.input(AppMsg::Invoke {
                        addon: addon.clone(),
                        action,
                        node: Some(id),
                        values,
                    })
                });
                addons::render(page, &view, on_action);
            }
            Err(error) => {
                let button = gtk::Button::with_label("Try again");
                send_on_click(&button, sender, AppMsg::LoadView(id));
                addons::render_message(page, &node.name, &error, vec![button]);
            }
        }
    }

    /// An SSH host tab's tooltip: the host, and the title its shell set, as
    /// a local terminal's tab shows its shell's.
    fn set_remote_tooltip(&self, id: NodeId) {
        let (Some(tab), Some(node)) = (self.tabs.get(&TabKey::Node(id)), self.nodes.get(&id))
        else {
            return;
        };
        if let Some(title) = self.remotes.get(&id).and_then(|r| r.title.as_ref()) {
            let tooltip = format!("{}\n{title}", node.name);
            tab.title.set_tooltip_text(Some(&tooltip));
        }
    }

    fn remote_for(&self, session: SessionId) -> Option<&Remote> {
        self.remotes
            .values()
            .find(|r| r.state == session::State::Running(session))
    }

    fn session_event(
        &mut self,
        event: Event,
        window: &gtk::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        let session = match &event {
            Event::SessionChanged { session, .. } | Event::PromptRequested { session, .. } => {
                *session
            }
            _ => return,
        };
        // A tunnel's events name the tunnel, so they need no claiming.
        if let Event::SessionChanged { node, state, .. } = &event
            && self
                .nodes
                .get(node)
                .is_some_and(|n| n.kind == SSH_TUNNEL_KIND)
        {
            self.tunnel_changed(*node, session, state.clone(), sender);
            return;
        }
        if let Event::PromptRequested { prompt, .. } = &event
            && let Some(node) = self.tunnel_of(session)
        {
            let prompt = self.for_tunnel(node, prompt);
            self.ask(session, &prompt, window, sender);
            return;
        }
        let running = session::State::Running(session);
        if let Some(panel) = self
            .databases
            .values_mut()
            .find(|p| p.session.state == running)
        {
            match event {
                Event::SessionChanged { state, .. } => {
                    panel.session_changed(&state, &self.core, sender)
                }
                Event::PromptRequested { prompt, .. } => self.ask(session, &prompt, window, sender),
                _ => {}
            }
            return;
        }
        if let Some(panel) = self
            .monitors
            .values_mut()
            .find(|p| p.session.state == running)
        {
            match event {
                Event::SessionChanged { state, .. } => {
                    panel.session_changed(&state, &self.core, sender)
                }
                Event::PromptRequested { prompt, .. } => self.ask(session, &prompt, window, sender),
                _ => {}
            }
            return;
        }
        if let Some(panel) = self.files.values_mut().find(|p| p.session.state == running) {
            match event {
                Event::SessionChanged { state, .. } => {
                    panel.session_changed(&state, &self.core, sender)
                }
                Event::PromptRequested { prompt, .. } => self.ask(session, &prompt, window, sender),
                _ => {}
            }
            return;
        }
        let Some(remote) = self
            .remotes
            .values_mut()
            .find(|r| r.state == session::State::Running(session))
        else {
            // A prompt from a session nobody here started, such as a
            // tunnel that started with Burrow before its first event
            // arrived. Look up whose it is.
            if matches!(event, Event::PromptRequested { .. }) && !self.looking_up {
                self.looking_up = true;
                list_sessions(&self.core, sender);
            }
            self.unclaimed.hold(session, event);
            return;
        };
        match event {
            Event::SessionChanged { state, .. } => {
                let text = remote::status_text(&state);
                if matches!(state, SessionState::Closed { .. }) {
                    remote.ending.status = Some(text.unwrap_or_default());
                    remote.finish_if_done();
                } else if let Some(text) = text {
                    remote.terminal.feed(text.as_bytes());
                }
            }
            Event::PromptRequested { prompt, .. } => self.ask(session, &prompt, window, sender),
            _ => {}
        }
    }

    fn ask(
        &self,
        session: SessionId,
        prompt: &Prompt,
        window: &gtk::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        let (core, sender) = (self.core.clone(), sender.clone());
        let id = prompt.id;
        let dialog = dialogs::answer(window, prompt, move |answer| {
            let command = Command::AnswerPrompt {
                session,
                prompt: id,
                answer,
            };
            execute(&core, &sender, command);
        });
        self.prompts.borrow_mut().insert((session, id), dialog);
    }

    fn tunnel_of(&self, session: SessionId) -> Option<NodeId> {
        self.tunnels
            .iter()
            .find(|(_, status)| status.running() == Some(session))
            .map(|(id, _)| *id)
    }

    /// A tunnel can ask while its tab is closed, or when it started with
    /// Burrow, so its questions say which tunnel is asking.
    fn for_tunnel(&self, node: NodeId, prompt: &Prompt) -> Prompt {
        let name = self
            .nodes
            .get(&node)
            .map_or("a tunnel", |n| n.name.as_str());
        Prompt {
            message: format!("For the tunnel “{name}”.\n\n{}", prompt.message),
            ..prompt.clone()
        }
    }

    /// Takes the core's list of sessions: tunnels found there are tracked,
    /// and prompts they asked before we knew them are shown.
    fn take_sessions(
        &mut self,
        sessions: Vec<SessionInfo>,
        window: &gtk::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        for info in sessions {
            if info.kind != SessionKind::Tunnel {
                continue;
            }
            let status = self.tunnels.entry(info.node).or_default();
            // Events already seen for this session are newer than the list.
            if status
                .session
                .as_ref()
                .is_none_or(|(s, _)| *s < info.session)
            {
                status.session = Some((info.session, info.state));
                status.stale = false;
            }
            for event in self.unclaimed.claim(info.session) {
                if let Event::PromptRequested { prompt, .. } = event {
                    let prompt = self.for_tunnel(info.node, &prompt);
                    self.ask(info.session, &prompt, window, sender);
                }
            }
        }
        self.refresh_tunnels(sender);
    }

    fn tunnel_changed(
        &mut self,
        node: NodeId,
        session: SessionId,
        state: SessionState,
        sender: &ComponentSender<Self>,
    ) {
        let status = self.tunnels.entry(node).or_default();
        match &status.session {
            // A late event from a session that has been replaced.
            Some((current, _)) if session < *current => return,
            Some((current, _)) if session == *current => {}
            _ => status.stale = false,
        }
        let closed = matches!(state, SessionState::Closed { .. });
        status.session = Some((session, state));
        if closed && self.restarting.remove(&node) {
            execute(&self.core, sender, Command::StartTunnel { node });
        }
        self.refresh_tunnel(node, sender);
    }

    fn refresh_tunnels(&self, sender: &ComponentSender<Self>) {
        for &id in self.tunnel_pages.keys() {
            self.refresh_tunnel(id, sender);
        }
    }

    /// Draws a tunnel's tab again, if it is open.
    fn refresh_tunnel(&self, id: NodeId, sender: &ComponentSender<Self>) {
        let (Some(page), Some(node)) = (self.tunnel_pages.get(&id), self.nodes.get(&id)) else {
            return;
        };
        let send = |message: AppMsg| -> Rc<dyn Fn()> {
            let sender = sender.clone();
            Rc::new(move || sender.input(message.clone()))
        };
        let actions = tunnels::Actions {
            start: send(AppMsg::StartTunnel(id)),
            stop: send(AppMsg::StopTunnel(id)),
            restart: send(AppMsg::RestartTunnel(id)),
            edit: send(AppMsg::EditTunnel(id)),
            set_autostart: {
                let sender = sender.clone();
                Rc::new(move |enabled| sender.input(AppMsg::SetAutostart { node: id, enabled }))
            },
        };
        let status = self.tunnels.get(&id).cloned().unwrap_or_default();
        let autostart = self.autostart.contains(&id);
        tunnels::fill(page, node, &self.nodes, &status, autostart, &actions);
    }

    /// Creates a tunnel in `parent`, or edits the tunnel `id`.
    fn edit_tunnel(
        &self,
        id: Option<NodeId>,
        parent: Option<NodeId>,
        window: &gtk::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        let node = id.and_then(|id| self.nodes.get(&id));
        if id.is_some() && node.is_none() {
            return;
        }
        let mut hosts: Vec<(NodeId, String)> = self
            .nodes
            .values()
            .filter(|n| n.kind == SSH_HOST_KIND)
            .map(|n| (n.id, n.name.clone()))
            .collect();
        hosts.sort_by_key(|(id, name)| (name.to_lowercase(), *id));
        // Starting with Burrow is pinned to the settings, so a change
        // withdraws it. A change made here is the user's own, so it is
        // asked for again.
        let autostart = id.is_some_and(|id| self.autostart.contains(&id));
        // The hosts as they were when the dialog opened.
        let host_nodes: HashMap<NodeId, Node> = self
            .nodes
            .iter()
            .filter(|(_, n)| n.kind == SSH_HOST_KIND)
            .map(|(id, n)| (*id, n.clone()))
            .collect();
        let (core, input) = (self.core.clone(), sender.input_sender().clone());
        tunnels::edit(window, node, hosts, move |name, props| {
            let saved = shown(&host_nodes, &props);
            let command = match id {
                Some(id) => Command::UpdateNode {
                    id,
                    patch: NodePatch {
                        name: Some(name),
                        props: Some(props),
                        ..Default::default()
                    },
                },
                None => Command::CreateNode {
                    parent,
                    kind: SSH_TUNNEL_KIND.into(),
                    name,
                    color: None,
                    props,
                },
            };
            let (core, input) = (core.clone(), input.clone());
            runtime().spawn(async move {
                let result = match run(&core, command).await {
                    // Only the settings saved here: a file that changed
                    // meanwhile makes this fail rather than be approved.
                    Ok(Response::Node { node }) if autostart => {
                        let command = Command::SetAutostart {
                            node: node.id,
                            enabled: true,
                            expect: saved,
                        };
                        run(&core, command).await
                    }
                    other => other,
                };
                if let Err(error) = result {
                    let _ = input.send(AppMsg::Failed(error));
                }
            });
        });
    }

    /// Creates a database in `parent`, or edits the database `id`.
    fn edit_database(
        &self,
        id: Option<NodeId>,
        parent: Option<NodeId>,
        window: &gtk::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        let node = id.and_then(|id| self.nodes.get(&id));
        if id.is_some() && node.is_none() {
            return;
        }
        let mut hosts: Vec<(NodeId, String)> = self
            .nodes
            .values()
            .filter(|n| n.kind == SSH_HOST_KIND)
            .map(|n| (n.id, n.name.clone()))
            .collect();
        hosts.sort_by_key(|(id, name)| (name.to_lowercase(), *id));
        let (core, sender) = (self.core.clone(), sender.clone());
        database::edit(window, node, hosts, move |name, props| {
            let command = match id {
                Some(id) => Command::UpdateNode {
                    id,
                    patch: NodePatch {
                        name: Some(name),
                        props: Some(props),
                        ..Default::default()
                    },
                },
                None => Command::CreateNode {
                    parent,
                    kind: POSTGRES_KIND.into(),
                    name,
                    color: None,
                    props,
                },
            };
            execute(&core, &sender, command);
        });
    }

    fn activate(&mut self, id: NodeId, widgets: &Widgets, sender: &ComponentSender<Self>) {
        let Some(node) = self.nodes.get(&id) else {
            return;
        };
        if node.is_folder() {
            widgets.sidebar.toggle(id);
        } else {
            self.open_tab_now(id, widgets, sender);
        }
    }

    fn open_tab(&mut self, id: NodeId, widgets: &Widgets, sender: &ComponentSender<Self>) {
        // Reveal the node in the sidebar, then open it.
        widgets.sidebar.reveal(id);
        if self.nodes.get(&id).is_some_and(|n| !n.is_folder()) {
            self.open_tab_now(id, widgets, sender);
        }
    }

    fn open_tab_now(&mut self, id: NodeId, widgets: &Widgets, sender: &ComponentSender<Self>) {
        let key = TabKey::Node(id);
        if let Some(tab) = self.tabs.get(&key) {
            if let Some(index) = widgets.notebook.page_num(&tab.page) {
                widgets.notebook.set_current_page(Some(index));
            }
            return;
        }
        let Some(node) = self.nodes.get(&id) else {
            return;
        };
        let icon = sidebar::icon_name(node, false, &self.icons()).to_string();
        let (icon, name) = (icon.as_str(), node.name.clone());
        if node.kind == SSH_HOST_KIND {
            let term = terminal::remote(
                {
                    let sender = sender.clone();
                    move |title| sender.input(AppMsg::RemoteTitle { node: id, title })
                },
                {
                    let sender = sender.clone();
                    move |data| sender.input(AppMsg::RemoteInput { node: id, data })
                },
                {
                    let sender = sender.clone();
                    move |cols, rows| {
                        sender.input(AppMsg::RemoteResize {
                            node: id,
                            cols,
                            rows,
                        })
                    }
                },
            );
            let page = terminal::page(&term);
            self.add_tab(key, page.upcast(), icon, &name, widgets, sender);
            term.grab_focus();
            remote::start(&self.core, id, &term, sender);
            self.remotes.insert(id, Remote::new(term));
            return;
        }
        if node.kind == POSTGRES_KIND {
            let panel = Database::new(id, sender);
            self.add_tab(
                key,
                panel.page.clone().upcast(),
                icon,
                &name,
                widgets,
                sender,
            );
            database::start(&self.core, id, sender);
            self.databases.insert(id, panel);
            return;
        }
        if node.kind == SSH_TUNNEL_KIND {
            let page = gtk::Box::new(gtk::Orientation::Vertical, 8);
            page.add_css_class("tunnel-view");
            self.add_tab(key, page.clone().upcast(), icon, &name, widgets, sender);
            self.tunnel_pages.insert(id, page);
            self.refresh_tunnel(id, sender);
            return;
        }
        if addons::owner(&self.addons, &node.kind).is_some() {
            let page = gtk::Box::new(gtk::Orientation::Vertical, 12);
            page.add_css_class("addon-view");
            let scroller = gtk::ScrolledWindow::builder()
                .child(&page)
                .hscrollbar_policy(gtk::PolicyType::Never)
                .build();
            self.add_tab(key, scroller.upcast(), icon, &name, widgets, sender);
            self.views.insert(id, page);
            self.load_view(id, sender);
            return;
        }
        let page = gtk::Box::new(gtk::Orientation::Vertical, 8);
        fill_page(&page, node, &self.nodes);
        self.add_tab(key, page.upcast(), icon, &name, widgets, sender);
    }

    fn open_files(&mut self, id: NodeId, widgets: &Widgets, sender: &ComponentSender<Self>) {
        let key = TabKey::Files(id);
        if let Some(tab) = self.tabs.get(&key) {
            if let Some(index) = widgets.notebook.page_num(&tab.page) {
                widgets.notebook.set_current_page(Some(index));
            }
            return;
        }
        let Some(node) = self.nodes.get(&id).filter(|n| n.kind == SSH_HOST_KIND) else {
            return;
        };
        let title = files_title(&node.name);
        let panel = Files::new(id, sender);
        let page = panel.page.clone().upcast();
        self.add_tab(key, page, "folder-remote-symbolic", &title, widgets, sender);
        files::start(&self.core, id, sender);
        self.files.insert(id, panel);
    }

    fn open_monitor(&mut self, id: NodeId, widgets: &Widgets, sender: &ComponentSender<Self>) {
        let key = TabKey::Monitor(id);
        if let Some(tab) = self.tabs.get(&key) {
            if let Some(index) = widgets.notebook.page_num(&tab.page) {
                widgets.notebook.set_current_page(Some(index));
            }
            return;
        }
        let Some(node) = self.nodes.get(&id).filter(|n| n.kind == SSH_HOST_KIND) else {
            return;
        };
        let title = monitor_title(&node.name);
        let panel = Monitor::new(id, sender);
        let page = panel.page.clone().upcast();
        let icon = "utilities-system-monitor-symbolic";
        self.add_tab(key, page, icon, &title, widgets, sender);
        monitor::start(&self.core, id, sender);
        self.monitors.insert(id, panel);
        self.refresh_monitors();
    }

    /// Shows each monitor tab whether its host is monitored in the
    /// background, and its alerts.
    fn refresh_monitors(&mut self) {
        for (node, panel) in &mut self.monitors {
            panel.set_background(self.monitoring.contains(node));
            let alerts: Vec<&Alert> = self.alerts.iter().filter(|a| a.node == *node).collect();
            panel.set_alerts(&alerts);
        }
    }

    /// Turns background monitoring of `id` on, after saying what that
    /// means, or off.
    fn set_monitoring(
        &mut self,
        id: NodeId,
        on: bool,
        window: &gtk::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        let Some(node) = self.nodes.get(&id).cloned() else {
            return;
        };
        let (core, input) = (self.core.clone(), sender.input_sender().clone());
        let send = move |command: Command| {
            runtime().spawn(async move {
                if let Err(error) = run(&core, command).await {
                    let _ = input.send(AppMsg::MonitoringFailed(error));
                }
            });
        };
        if !on {
            send(Command::SetMonitoring {
                node: id,
                enabled: false,
                expect: None,
            });
            return;
        }
        // The checkbox follows the core, so it stays off until the core
        // says otherwise.
        self.refresh_monitors();
        let nodes: Vec<Node> = self.nodes.values().cloned().collect();
        let expect = Some(GrantShown::of(&node, &nodes));
        let name = files::printable(&node.name);
        let detail = format!(
            "While Burrow runs, it keeps a monitor open on {name}, even with no window open, \
             keeps a week of its history on this computer, and shows a notification when a \
             service fails, a disk is nearly full, memory runs low, or {name} stops answering.\n\n\
             It installs burrow-agent there if needed, as an open monitor does. It holds only \
             while {name}'s settings stay as they are. To monitor from login, enable \
             burrowd.service (systemctl --user enable burrowd.service)."
        );
        dialogs::confirm(
            window,
            &format!("Monitor {name} in the background?"),
            &detail,
            "Monitor",
            move || {
                send(Command::SetMonitoring {
                    node: id,
                    enabled: true,
                    expect,
                })
            },
        );
    }

    fn open_terminal(
        &mut self,
        shell: Option<String>,
        widgets: &Widgets,
        sender: &ComponentSender<Self>,
    ) {
        let id = self.next_terminal;
        self.next_terminal += 1;
        let shell = shell.unwrap_or_else(terminal::default_shell);
        let term = terminal::new(
            &shell,
            {
                let sender = sender.clone();
                move |title| sender.input(AppMsg::TerminalTitle { id, title })
            },
            {
                let sender = sender.clone();
                move || sender.input(AppMsg::TerminalExited(id))
            },
        );
        let page = terminal::page(&term);
        let key = TabKey::Terminal(id);
        let name = terminal::shell_name(&shell);
        self.add_tab(
            key,
            page.upcast(),
            "utilities-terminal-symbolic",
            name,
            widgets,
            sender,
        );
        term.grab_focus();
    }

    fn add_tab(
        &mut self,
        key: TabKey,
        page: gtk::Widget,
        icon: &str,
        name: &str,
        widgets: &Widgets,
        sender: &ComponentSender<Self>,
    ) {
        let title = gtk::Label::new(None);
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);
        set_tab_title(&title, name);
        let close = gtk::Button::from_icon_name("window-close-symbolic");
        close.add_css_class("flat");
        close.add_css_class("tab-close");
        close.set_tooltip_text(Some("Close tab (Ctrl+Shift+W)"));
        let tab_label = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        tab_label.append(&gtk::Image::from_icon_name(icon));
        tab_label.append(&title);
        tab_label.append(&close);

        // Middle-click closes a tab, as in browsers and other terminals.
        let middle_click = gtk::GestureClick::new();
        middle_click.set_button(gdk::BUTTON_MIDDLE);
        middle_click.connect_released({
            let sender = sender.clone();
            move |_, _, _, _| sender.input(AppMsg::CloseTab(key))
        });
        tab_label.add_controller(middle_click);

        let notebook = &widgets.notebook;
        let index = notebook.append_page(&page, Some(&tab_label));
        notebook.set_tab_reorderable(&page, true);
        notebook.set_current_page(Some(index));

        send_on_click(&close, sender, AppMsg::CloseTab(key));
        self.tabs.insert(key, Tab { page, title });
    }

    fn close_tab(&mut self, key: TabKey, widgets: &Widgets) {
        if let Some(tab) = self.tabs.remove(&key) {
            close_page(&widgets.notebook, &tab.page);
        }
        if let TabKey::Node(id) = key {
            self.views.remove(&id);
            self.tunnel_pages.remove(&id);
        }
        // A session still starting is closed when its id arrives. Closing
        // a file panel's session stops its transfers too.
        let state = match key {
            TabKey::Node(id) => self
                .remotes
                .remove(&id)
                .map(|r| r.state)
                .or_else(|| self.databases.remove(&id).map(|d| d.session.state)),
            TabKey::Files(id) => self.files.remove(&id).map(|f| f.session.state),
            TabKey::Monitor(id) => self.monitors.remove(&id).map(|m| m.session.state),
            TabKey::Terminal(_) => None,
        };
        if let Some(session::State::Running(session)) = state {
            let core = self.core.clone();
            runtime().spawn(async move {
                let _ = core.execute(Command::CloseSession { session }).await;
            });
        }
    }

    /// The folder new nodes go into: the selected folder, or the folder of
    /// the selected leaf, or the top level.
    fn selection_folder(&self, widgets: &Widgets) -> Option<NodeId> {
        let node = self.nodes.get(&selected(widgets)?)?;
        if node.is_folder() {
            Some(node.id)
        } else {
            node.parent
        }
    }

    fn new_node(
        &self,
        kind: NewKind,
        parent: Option<NodeId>,
        window: &gtk::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        let (title, label) = match &kind {
            NewKind::Folder => ("New folder".to_string(), "Folder name"),
            NewKind::SshHost => return hosts::new_host(window, self.core.clone(), parent),
            NewKind::SshTunnel => return self.edit_tunnel(None, parent, window, sender),
            NewKind::Database => return self.edit_database(None, parent, window, sender),
            NewKind::Addon(kind) => {
                let label = self
                    .addons
                    .iter()
                    .flat_map(|a| &a.kinds)
                    .find(|k| &k.kind == kind)
                    .map_or(kind.as_str(), |k| k.label.as_str());
                (format!("New {label}"), "Name")
            }
        };
        let (core, sender) = (self.core.clone(), sender.clone());
        dialogs::prompt(window, &title, label, "", "Create", move |name| {
            let (kind, props) = match &kind {
                NewKind::Folder => ("folder", toml::Table::new()),
                NewKind::Addon(kind) => (kind.as_str(), toml::Table::new()),
                NewKind::SshHost | NewKind::SshTunnel | NewKind::Database => {
                    unreachable!("hosts, tunnels and databases have their own dialogs")
                }
            };
            execute(
                &core,
                &sender,
                Command::CreateNode {
                    parent,
                    kind: kind.into(),
                    name,
                    color: None,
                    props,
                },
            );
        });
    }

    fn rename(&self, id: NodeId, window: &gtk::ApplicationWindow, sender: &ComponentSender<Self>) {
        let Some(node) = self.nodes.get(&id) else {
            return;
        };
        let (core, sender) = (self.core.clone(), sender.clone());
        dialogs::prompt(
            window,
            "Rename",
            "Name",
            &node.name,
            "Rename",
            move |name| {
                let patch = NodePatch {
                    name: Some(name),
                    ..Default::default()
                };
                execute(&core, &sender, Command::UpdateNode { id, patch });
            },
        );
    }

    fn delete(&self, id: NodeId, window: &gtk::ApplicationWindow, sender: &ComponentSender<Self>) {
        let Some(node) = self.nodes.get(&id) else {
            return;
        };
        // The core closes the sessions of every node it deletes.
        let detail = if node.is_folder() {
            "Everything inside it is deleted too, along with any saved passwords. \
             Anything running on what it holds, such as a terminal or a tunnel, stops."
        } else {
            "Its saved passwords are removed from the keyring too. Anything running \
             on it, such as a terminal or a tunnel, stops."
        };
        let (core, sender) = (self.core.clone(), sender.clone());
        dialogs::confirm(
            window,
            &format!("Delete “{}”?", node.name),
            detail,
            "Delete",
            move || {
                execute(
                    &core,
                    &sender,
                    Command::DeleteNode {
                        id,
                        recursive: true,
                    },
                );
            },
        );
    }

    /// "Move to…", for keyboard users and long lists, where dragging is
    /// awkward.
    fn move_to(&self, id: NodeId, window: &gtk::ApplicationWindow, sender: &ComponentSender<Self>) {
        let Some(node) = self.nodes.get(&id) else {
            return;
        };
        let choices = sidebar::move_choices(&self.nodes, id);
        let (core, sender) = (self.core.clone(), sender.clone());
        sidebar::move_dialog(window, node, choices, move |parent| {
            execute(&core, &sender, Command::MoveNode { id, parent });
        });
    }

    fn colour(&self, id: NodeId, window: &gtk::ApplicationWindow, sender: &ComponentSender<Self>) {
        let Some(node) = self.nodes.get(&id) else {
            return;
        };
        let (core, sender, current) = (self.core.clone(), sender.clone(), node.color.clone());
        sidebar::colour_dialog(window, node, move |color| {
            if color != current {
                let patch = NodePatch {
                    color: Some(color),
                    ..Default::default()
                };
                execute(&core, &sender, Command::UpdateNode { id, patch });
            }
        });
    }

    fn context_menu(
        &self,
        id: Option<NodeId>,
        x: f64,
        y: f64,
        widgets: &Widgets,
        sender: &ComponentSender<Self>,
    ) {
        let node = id.and_then(|id| self.nodes.get(&id));
        let mut entries: Vec<(String, AppMsg)> = Vec::new();
        match node {
            Some(node) if node.is_folder() => {
                entries.extend(self.new_entries(Some(Some(node.id)), " here"));
            }
            Some(node) => {
                entries.push(("Open".into(), AppMsg::Open(node.id)));
                if node.kind == SSH_HOST_KIND {
                    entries.push(("Browse files".into(), AppMsg::OpenFiles(node.id)));
                    entries.push(("Monitor".into(), AppMsg::OpenMonitor(node.id)));
                }
                if node.kind == SSH_TUNNEL_KIND {
                    let running = self.tunnels.get(&node.id).and_then(|s| s.running());
                    entries.push(match running {
                        Some(_) => ("Stop".into(), AppMsg::StopTunnel(node.id)),
                        None => ("Start".into(), AppMsg::StartTunnel(node.id)),
                    });
                    entries.push(("Edit…".into(), AppMsg::EditTunnel(node.id)));
                }
                if node.kind == POSTGRES_KIND {
                    entries.push(("Edit…".into(), AppMsg::EditDatabase(node.id)));
                }
                if !capabilities(&node.kind).is_empty() {
                    entries.push(("Claude access…".into(), AppMsg::EditGrant(node.id)));
                }
                for addon in self.addons.iter().filter(|a| addons::usable(a)) {
                    let actions = addon.actions.iter();
                    for action in actions.filter(|a| a.kinds.contains(&node.kind)) {
                        entries.push((
                            action.title.clone(),
                            AppMsg::Invoke {
                                addon: addon.id.clone(),
                                action: action.id.clone(),
                                node: Some(node.id),
                                values: BTreeMap::new(),
                            },
                        ));
                    }
                }
            }
            None => entries.extend(self.new_entries(Some(None), "")),
        }
        if let Some(node) = node {
            entries.push(("Rename…".into(), AppMsg::Rename(node.id)));
            entries.push(("Move to…".into(), AppMsg::MoveTo(node.id)));
            entries.push(("Colour…".into(), AppMsg::Colour(node.id)));
            entries.push(("Delete…".into(), AppMsg::Delete(node.id)));
        }
        let popover = menu_popover(entries, sender);
        // Parented to the pane, never to the list view: the view owns its
        // children and adds and removes them as rows come and go, and a
        // popover among a list's children once hung the window when the
        // list changed while the menu was open (commit 9fa1e36).
        popover.set_parent(&widgets.sidebar_pane);
        let (x, y) = (x as i32, y as i32);
        popover.set_pointing_to(Some(&gdk::Rectangle::new(x, y, 1, 1)));
        popover.connect_closed(|popover| {
            // Unparented once the signal has returned, not inside it.
            let popover = popover.clone();
            glib::idle_add_local_once(move || popover.unparent());
        });
        popover.popup();
    }

    /// The grant dialog. The grant is pinned to the node as the dialog
    /// shows it, so a change made meanwhile makes saving fail instead.
    fn edit_grant(
        &self,
        id: NodeId,
        grant: Option<Grant>,
        window: &gtk::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        let Some(node) = self.nodes.get(&id) else {
            return;
        };
        let nodes: Vec<Node> = self.nodes.values().cloned().collect();
        let shown = GrantShown::of(node, &nodes);
        let mut details = format!(
            "What Claude may do on {} through Burrow's MCP bridge (burrow mcp serve). \
             It holds only while these settings stay as they are:\n",
            files::printable(&node.name)
        );
        for (key, value) in &shown.props {
            details.push_str(&format!(
                "\n{} = {}",
                files::printable(key),
                files::printable(&value.to_string())
            ));
        }
        if let Some(via) = &shown.via {
            details.push_str(&format!("\n\nThrough {}:", files::printable(&via.name)));
            for (key, value) in &via.props {
                details.push_str(&format!(
                    "\n{} = {}",
                    files::printable(key),
                    files::printable(&value.to_string())
                ));
            }
        }
        let granted = match &grant {
            Some(grant) if grant.stale => {
                details.push_str(
                    "\n\nThese changed since access was given, so nothing is allowed \
                     until you save again.",
                );
                Default::default()
            }
            Some(grant) => grant.capabilities.clone(),
            None => Default::default(),
        };
        if node.kind == POSTGRES_KIND {
            details.push_str(
                "\n\nRead-only SQL is refused for a superuser, or a role that may write \
                 files or run programs on the server, since Burrow can't keep those \
                 read-only. For full safety, connect as a role that can only read. \
                 Allowing SQL that writes as a superuser also lets Claude run \
                 programs on the database server.",
            );
        }
        if node.kind == SSH_HOST_KIND {
            details.push_str(&format!(
                "\n\n“{}” amounts to running commands, since it can change \
                 ~/.ssh/authorized_keys or shell start-up files. Give it as carefully \
                 as “{}”.",
                capability_label(Capability::FilesWrite),
                capability_label(Capability::Exec),
            ));
        }
        let offered = capabilities(&node.kind);
        let options = offered
            .iter()
            .map(|c| (capability_label(*c).to_string(), granted.contains(c)))
            .collect();
        let (core, sender) = (self.core.clone(), sender.clone());
        dialogs::choose(window, "Claude access", &details, options, move |ticked| {
            let capabilities = offered
                .iter()
                .zip(ticked)
                .filter_map(|(c, on)| on.then_some(*c))
                .collect();
            let command = Command::SetGrant {
                node: id,
                capabilities,
                expect: Some(shown),
            };
            execute(&core, &sender, command);
        });
    }

    fn show_palette(&self, window: &gtk::ApplicationWindow, sender: &ComponentSender<Self>) {
        let command = |title: &str, message: AppMsg| Item {
            title: title.to_string(),
            subtitle: String::new(),
            message,
        };
        let mut items = vec![command("New terminal", AppMsg::NewTerminal(None))];
        items.extend(
            self.new_entries(None, "")
                .into_iter()
                .map(|(title, message)| command(&title, message)),
        );
        items.extend([
            command("Toggle sidebar", AppMsg::ToggleSidebar),
            command("Reload config", AppMsg::ReloadConfig),
            command("Open config folder", AppMsg::OpenConfigFolder),
            command("Manage addons…", AppMsg::ManageAddons),
            command("Settings…", AppMsg::ShowSettings),
        ]);
        for addon in self.addons.iter().filter(|a| addons::usable(a)) {
            items.extend(addon.commands.iter().map(|c| Item {
                title: c.title.clone(),
                subtitle: addon.name.clone(),
                message: AppMsg::Invoke {
                    addon: addon.id.clone(),
                    action: c.id.clone(),
                    node: None,
                    values: BTreeMap::new(),
                },
            }));
        }
        items.extend(terminal::shells().into_iter().map(|shell| Item {
            title: format!("New terminal: {}", terminal::shell_name(&shell)),
            subtitle: shell.clone(),
            message: AppMsg::NewTerminal(Some(shell)),
        }));
        let mut nodes: Vec<&Node> = self.nodes.values().collect();
        nodes.sort_by_key(|n| (n.is_folder(), n.name.to_lowercase()));
        items.extend(
            nodes
                .iter()
                .filter(|n| n.kind == SSH_HOST_KIND)
                .flat_map(|node| {
                    let subtitle = sidebar::location(&self.nodes, node);
                    [
                        Item {
                            title: format!("Browse files: {}", node.name),
                            subtitle: subtitle.clone(),
                            message: AppMsg::OpenFiles(node.id),
                        },
                        Item {
                            title: format!("Monitor: {}", node.name),
                            subtitle,
                            message: AppMsg::OpenMonitor(node.id),
                        },
                    ]
                }),
        );
        items.extend(nodes.into_iter().map(|node| {
            Item {
                title: node.name.clone(),
                subtitle: [
                    self.kind_label(&node.kind),
                    sidebar::location(&self.nodes, node),
                ]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" · "),
                message: AppMsg::Open(node.id),
            }
        }));
        let sender = sender.clone();
        palette::show(window, items, move |message| sender.input(message));
    }

    /// Closes the window, first asking when that would end terminals: local
    /// shells end with the window, and burrowd closes this client's SSH
    /// sessions when it goes.
    fn close_window(&self, window: &gtk::ApplicationWindow) {
        let remote = (self.remotes.values())
            .filter(|r| matches!(r.state, session::State::Running(_)))
            .count();
        let local = (self.tabs.keys())
            .filter(|key| matches!(key, TabKey::Terminal(_)))
            .count();
        let open = remote + local;
        if open == 0 {
            window.destroy();
            return;
        }
        let detail = if open == 1 {
            "A terminal is open. Closing Burrow ends it.".to_string()
        } else {
            format!("{open} terminals are open. Closing Burrow ends them.")
        };
        let target = window.clone();
        dialogs::confirm(window, "Close Burrow?", &detail, "Close", move || {
            target.destroy()
        });
    }

    fn apply_theme(&self) {
        let Some(settings) = gtk::Settings::default() else {
            return;
        };
        match self.settings.theme {
            // Resetting, rather than setting the startup value, hands the
            // setting back to the desktop so later light/dark switches from
            // the portal still apply.
            Theme::System => settings.reset_property("gtk-interface-color-scheme"),
            Theme::Light => {
                settings.set_gtk_interface_color_scheme(gtk::InterfaceColorScheme::Light)
            }
            Theme::Dark => settings.set_gtk_interface_color_scheme(gtk::InterfaceColorScheme::Dark),
        }
    }

    fn save_settings(&mut self) {
        if let Err(e) = self.settings.save(&self.config_dir) {
            self.fail(format!("Could not save settings: {e}"));
        }
    }

    /// Shows `error` in the status bar until the user does something or a
    /// newer error replaces it, and keeps it in the errors list.
    fn fail(&mut self, error: String) {
        let time = glib::DateTime::now_local()
            .and_then(|now| now.format("%H:%M:%S"))
            .map(|time| time.to_string())
            .unwrap_or_default();
        self.errors.push(time, error.clone());
        self.error = Some(error);
    }

    fn refresh_status(&self, widgets: &Widgets) {
        let (text, is_error) = if self.reconnecting {
            // Stays up until the new connection is, with why the last try
            // failed beside it.
            let lost = "Lost the connection to burrowd. Reconnecting…";
            let text = match &self.error {
                Some(error) => format!("{lost} ({error})"),
                None => lost.to_string(),
            };
            (text, true)
        } else if let Some(error) = &self.error {
            (error.clone(), true)
        } else if let Some(notice) = &self.notice {
            (notice.clone(), false)
        } else {
            let leaves = self.nodes.values().filter(|n| !n.is_folder()).count();
            let folders = self.nodes.len() - leaves;
            let text = format!(
                "{leaves} {} · {folders} {}",
                plural(leaves, "item", "items"),
                plural(folders, "folder", "folders"),
            );
            (text, false)
        };
        widgets.status.set_label(&text);
        if is_error {
            widgets.status.add_css_class("error");
        } else {
            widgets.status.remove_css_class("error");
        }
        let errors = self.errors.0.len();
        widgets.errors_button.set_visible(errors > 0);
        widgets
            .errors_button
            .set_label(&format!("{errors} {}", plural(errors, "error", "errors")));
        widgets.errors_list.set_label(&self.errors.text());
        widgets
            .problems_button
            .set_visible(!self.problems.is_empty());
        widgets.problems_button.set_label(&format!(
            "{} config {}",
            self.problems.len(),
            plural(self.problems.len(), "problem", "problems")
        ));
        widgets.problems_list.set_label(
            &self
                .problems
                .iter()
                .map(|p| format!("{}\n    {}", p.path.display(), p.message))
                .collect::<Vec<_>>()
                .join("\n\n"),
        );
    }
}

/// Runs a command on the core's runtime. Successful results arrive as
/// events, so only failures are reported back here.
fn execute(core: &Client, sender: &ComponentSender<App>, command: Command) {
    let (core, input) = (core.clone(), sender.input_sender().clone());
    runtime().spawn(async move {
        if let Err(error) = run(&core, command).await {
            let _ = input.send(AppMsg::Failed(error));
        }
    });
}

/// Runs a command, naming it in the error, so that "permission denied"
/// says what was denied.
async fn run(core: &Client, command: Command) -> Result<Response, String> {
    let name = command_name(&command);
    core.execute(command)
        .await
        .map_err(|e| format!("{name}: {e}"))
}

/// The command's `type` as the API names it, such as `start_tunnel`. It is
/// read from the start of the command's `Debug` text, and the writer stops
/// there, so a command carrying a file's bytes isn't formatted whole.
fn command_name(command: &Command) -> String {
    use std::fmt::Write;

    /// serde's `snake_case` of the variant name.
    struct Name(String);
    impl Write for Name {
        fn write_str(&mut self, text: &str) -> std::fmt::Result {
            for c in text.chars() {
                if !c.is_ascii_alphanumeric() {
                    return Err(std::fmt::Error);
                }
                if c.is_ascii_uppercase() && !self.0.is_empty() {
                    self.0.push('_');
                }
                self.0.push(c.to_ascii_lowercase());
            }
            Ok(())
        }
    }
    let mut name = Name(String::new());
    // Fails, on purpose, at the first character after the name.
    let _ = write!(name, "{command:?}");
    name.0
}

/// Loads the full node list, problems, addons, sessions and autostart
/// tunnels.
fn load(core: &Client, sender: &ComponentSender<App>) {
    let (core, input) = (core.clone(), sender.input_sender().clone());
    runtime().spawn(async move {
        let result = async {
            let nodes = match run(&core, Command::ListNodes).await? {
                Response::Nodes { nodes } => nodes,
                _ => Vec::new(),
            };
            let problems = match run(&core, Command::ListProblems).await? {
                Response::Problems { problems } => problems,
                _ => Vec::new(),
            };
            let addons = match run(&core, Command::ListAddons).await? {
                Response::Addons { addons } => addons,
                _ => Vec::new(),
            };
            let sessions = match run(&core, Command::ListSessions).await? {
                Response::Sessions { sessions } => sessions,
                _ => Vec::new(),
            };
            let autostart = match run(&core, Command::ListAutostart).await? {
                Response::Autostart { nodes } => nodes,
                _ => Vec::new(),
            };
            let monitoring = match run(&core, Command::ListMonitoring).await? {
                Response::Monitoring { nodes } => nodes,
                _ => Vec::new(),
            };
            let alerts = match run(&core, Command::ListAlerts).await? {
                Response::Alerts { alerts } => alerts,
                _ => Vec::new(),
            };
            Ok::<_, String>(AppMsg::Loaded {
                nodes,
                problems,
                addons,
                sessions,
                autostart,
                monitoring,
                alerts,
            })
        }
        .await;
        let _ = input.send(result.unwrap_or_else(AppMsg::Failed));
    });
}

/// Looks up every running session, for [`AppMsg::SessionsListed`].
fn list_sessions(core: &Client, sender: &ComponentSender<App>) {
    let (core, input) = (core.clone(), sender.input_sender().clone());
    runtime().spawn(async move {
        let sessions = match core.execute(Command::ListSessions).await {
            Ok(Response::Sessions { sessions }) => sessions,
            _ => Vec::new(),
        };
        let _ = input.send(AppMsg::SessionsListed(sessions));
    });
}

/// Forwards core events to the window until it closes. If the daemon goes
/// away, connects again, which starts a new one.
fn listen(core: &Client, sender: &ComponentSender<App>) {
    let mut events = core.subscribe();
    let input = sender.input_sender().clone();
    runtime().spawn(async move {
        loop {
            let message = match events.recv().await {
                Ok(event) => AppMsg::CoreEvent(event),
                Err(RecvError::Lagged(_)) => AppMsg::Resync,
                Err(RecvError::Closed) => break,
            };
            if input.send(message).is_err() {
                return; // The window is gone.
            }
        }
        if input.send(AppMsg::Disconnected).is_err() {
            return;
        }
        let mut last = String::new();
        loop {
            match ipc::connect(Role::Gui).await {
                Ok(client) => {
                    let _ = input.send(AppMsg::Reconnected(client));
                    return;
                }
                Err(e) => {
                    let error = format!("could not reach burrowd: {e}");
                    eprintln!("burrow: {error}");
                    // Only a new reason: one every two seconds would push
                    // everything else out of the errors list.
                    if error != last {
                        if input.send(AppMsg::Failed(error.clone())).is_err() {
                            return; // The window is gone.
                        }
                        last = error;
                    }
                    tokio::time::sleep(RECONNECT_DELAY).await;
                }
            }
        }
    });
}

/// What to say when the daemon is from another build: one that kept
/// running through an upgrade because tunnels or terminals run in it.
fn stale_daemon(core: &Client) -> Option<String> {
    let build = &core.info().build;
    (*build != ipc::BUILD).then(|| {
        format!(
            "Burrow's core ({}) is from before an update. Stop what runs in it, \
             then run \"burrow daemon stop\".",
            files::printable(build)
        )
    })
}

/// What the MCP bridge may be granted on a node of `kind`.
fn capabilities(kind: &str) -> Vec<Capability> {
    match kind {
        POSTGRES_KIND => vec![Capability::SqlRead, Capability::SqlWrite],
        SSH_HOST_KIND => vec![
            Capability::Exec,
            Capability::FilesRead,
            Capability::FilesWrite,
            Capability::MonitorRead,
        ],
        SSH_TUNNEL_KIND => vec![Capability::Tunnel],
        _ => Vec::new(),
    }
}

fn capability_label(capability: Capability) -> &'static str {
    match capability {
        Capability::SqlRead => "Run read-only SQL",
        Capability::SqlWrite => "Run SQL that writes",
        Capability::Exec => "Run commands",
        Capability::FilesRead => "List and read files",
        Capability::FilesWrite => "Create, change and delete files",
        Capability::Tunnel => "Start and stop the tunnel",
        Capability::MonitorRead => "Read system stats and services",
    }
}

fn selected(widgets: &Widgets) -> Option<NodeId> {
    widgets.sidebar.selected()
}

fn close_page(notebook: &gtk::Notebook, page: &gtk::Widget) {
    if let Some(index) = notebook.page_num(page) {
        notebook.remove_page(Some(index));
    }
}

/// Long titles (a shell's working folder, say) are cut short with "…".
/// Notebooks give tab labels only their minimum width, and an ellipsized
/// label's minimum is just the "…", so the minimum is set to fit the text.
/// A title that is cut short shows in full on hover.
fn set_tab_title(label: &gtk::Label, title: &str) {
    const MAX_CHARS: usize = 24;
    let chars = title.chars().count();
    label.set_label(title);
    label.set_width_chars(chars.min(MAX_CHARS) as i32);
    label.set_tooltip_text((chars > MAX_CHARS).then_some(title));
}

/// A tunnel's settings as the window shows them, for `set_autostart`:
/// `props` and the host they go through, as found in `nodes`.
fn shown(nodes: &HashMap<NodeId, Node>, props: &toml::Table) -> Option<TunnelShown> {
    let via = props.get("via")?.as_str()?.parse().ok().map(NodeId)?;
    let host = nodes.get(&via)?;
    Some(TunnelShown {
        props: props.clone(),
        host_name: host.name.clone(),
        host_props: host.props.clone(),
    })
}

/// The title of an SSH host's file panel tab.
fn monitor_title(host: &str) -> String {
    format!("{host} monitor")
}

fn files_title(host: &str) -> String {
    format!("{host} files")
}

fn plural<'a>(n: usize, one: &'a str, many: &'a str) -> &'a str {
    if n == 1 { one } else { many }
}

/// `text` with its first letter capitalised, as "Folder" for "folder".
fn sentence_case(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

/// Placeholder page for a node until its addon provides a real view.
fn fill_page(page: &gtk::Box, node: &Node, nodes: &HashMap<NodeId, Node>) {
    while let Some(child) = page.first_child() {
        page.remove(&child);
    }
    page.set_margin_top(24);
    page.set_margin_start(24);
    page.set_margin_end(24);

    let title = gtk::Label::new(Some(&node.name));
    title.set_xalign(0.0);
    title.add_css_class("page-title");
    page.append(&title);

    let location = sidebar::location(nodes, node);
    let subtitle = gtk::Label::new(Some(&if location.is_empty() {
        node.kind.clone()
    } else {
        format!("{} · {location}", node.kind)
    }));
    subtitle.set_xalign(0.0);
    subtitle.add_css_class("dim");
    page.append(&subtitle);

    if !node.props.is_empty() {
        let props = gtk::Label::new(Some(
            toml::to_string_pretty(&node.props)
                .unwrap_or_default()
                .trim_end(),
        ));
        props.set_xalign(0.0);
        props.set_selectable(true);
        props.add_css_class("props");
        page.append(&props);
    }

    let note = gtk::Label::new(Some("Burrow can't open this kind of item yet."));
    note.set_xalign(0.0);
    note.set_margin_top(12);
    note.add_css_class("dim");
    page.append(&note);
}

fn menu_popover(
    entries: Vec<(impl AsRef<str>, AppMsg)>,
    sender: &ComponentSender<App>,
) -> gtk::Popover {
    let popover = gtk::Popover::new();
    popover.set_has_arrow(false);
    popover.add_css_class("menu");
    let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    for (label, message) in entries {
        let button = gtk::Button::with_label(label.as_ref());
        button.add_css_class("flat");
        if let Some(child) = button.child().and_downcast::<gtk::Label>() {
            child.set_xalign(0.0);
        }
        // Weak: the button is inside the popover.
        button.connect_clicked(glib::clone!(
            #[weak]
            popover,
            #[strong]
            sender,
            move |_| {
                popover.popdown();
                sender.input(message.clone());
            }
        ));
        column.append(&button);
    }
    popover.set_child(Some(&column));
    popover
}

fn build(window: &gtk::ApplicationWindow, sender: &ComponentSender<App>) -> Widgets {
    // Header bar.
    let header = gtk::HeaderBar::new();
    let sidebar_button = gtk::Button::from_icon_name("sidebar-show-symbolic");
    sidebar_button.set_tooltip_text(Some("Toggle sidebar (Ctrl+Shift+B)"));
    send_on_click(&sidebar_button, sender, AppMsg::ToggleSidebar);
    header.pack_start(&sidebar_button);

    let new_button = gtk::MenuButton::new();
    new_button.set_icon_name("list-add-symbolic");
    new_button.set_tooltip_text(Some("New"));
    // Filled in once the addons, and their kinds, are known.
    header.pack_start(&new_button);

    let main_menu = gtk::MenuButton::new();
    main_menu.set_icon_name("open-menu-symbolic");
    main_menu.set_popover(Some(&menu_popover(
        vec![
            ("Command palette    Ctrl+Shift+P", AppMsg::ShowPalette),
            ("Reload config", AppMsg::ReloadConfig),
            ("Open config folder", AppMsg::OpenConfigFolder),
            ("Addons…", AppMsg::ManageAddons),
            ("Settings…    Ctrl+,", AppMsg::ShowSettings),
        ],
        sender,
    )));
    header.pack_end(&main_menu);
    let palette_button = gtk::Button::from_icon_name("system-search-symbolic");
    palette_button.set_tooltip_text(Some("Command palette (Ctrl+Shift+P)"));
    send_on_click(&palette_button, sender, AppMsg::ShowPalette);
    header.pack_end(&palette_button);
    window.set_titlebar(Some(&header));

    // Sidebar.
    let sidebar = sidebar::Sidebar::new(sender);
    let sidebar_pane = sidebar.pane.clone();

    // Tabs, with a welcome page when none are open.
    let notebook = gtk::Notebook::new();
    notebook.set_scrollable(true);
    // Typing goes straight to a terminal when its tab is chosen. The new
    // page isn't shown yet during the signal, so focus it just after.
    notebook.connect_switch_page(|_, page, _| {
        if let Some(terminal) = terminal::in_page(page) {
            glib::idle_add_local_once(move || {
                terminal.grab_focus();
            });
        }
    });
    let content = gtk::Stack::new();
    content.set_hexpand(true);
    content.add_named(&welcome(), Some("welcome"));
    content.add_named(&notebook, Some("tabs"));
    content.set_visible_child_name("welcome");

    let paned = gtk::Paned::new(gtk::Orientation::Horizontal);
    paned.set_start_child(Some(&sidebar_pane));
    paned.set_end_child(Some(&content));
    paned.set_resize_start_child(false);
    paned.set_shrink_start_child(false);
    paned.set_position(260);
    paned.set_vexpand(true);

    // Status bar.
    let status = gtk::Label::new(None);
    status.set_xalign(0.0);
    status.set_hexpand(true);
    status.set_ellipsize(gtk::pango::EllipsizeMode::End);
    let problems_list = gtk::Label::new(None);
    problems_list.set_xalign(0.0);
    problems_list.set_selectable(true);
    problems_list.set_margin_top(8);
    problems_list.set_margin_bottom(8);
    problems_list.set_margin_start(8);
    problems_list.set_margin_end(8);
    let problems_popover = gtk::Popover::new();
    problems_popover.set_child(Some(&problems_list));
    let problems_button = gtk::MenuButton::new();
    problems_button.add_css_class("flat");
    problems_button.add_css_class("warning");
    problems_button.set_popover(Some(&problems_popover));
    problems_button.set_visible(false);

    let errors_list = gtk::Label::new(None);
    errors_list.set_xalign(0.0);
    errors_list.set_selectable(true);
    errors_list.set_wrap(true);
    errors_list.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    errors_list.set_max_width_chars(72);
    let errors_scroller = gtk::ScrolledWindow::builder()
        .child(&errors_list)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_width(true)
        .propagate_natural_height(true)
        .max_content_height(320)
        .build();
    let clear_errors = gtk::Button::with_label("Clear");
    clear_errors.set_halign(gtk::Align::End);
    send_on_click(&clear_errors, sender, AppMsg::ClearErrors);
    let errors_content = gtk::Box::new(gtk::Orientation::Vertical, 8);
    errors_content.set_margin_top(8);
    errors_content.set_margin_bottom(8);
    errors_content.set_margin_start(8);
    errors_content.set_margin_end(8);
    errors_content.append(&errors_scroller);
    errors_content.append(&clear_errors);
    let errors_popover = gtk::Popover::new();
    errors_popover.set_child(Some(&errors_content));
    let errors_button = gtk::MenuButton::new();
    errors_button.add_css_class("flat");
    errors_button.add_css_class("error");
    errors_button.set_tooltip_text(Some("Recent errors"));
    errors_button.set_popover(Some(&errors_popover));
    errors_button.set_visible(false);

    let statusbar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    statusbar.add_css_class("statusbar");
    statusbar.append(&status);
    statusbar.append(&errors_button);
    statusbar.append(&problems_button);

    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.append(&paned);
    root.append(&statusbar);
    window.set_child(Some(&root));

    Widgets {
        sidebar_pane,
        sidebar,
        content,
        notebook,
        new_button,
        status,
        problems_button,
        problems_list,
        errors_button,
        errors_list,
    }
}

fn welcome() -> gtk::Box {
    let page = gtk::Box::new(gtk::Orientation::Vertical, 10);
    page.set_valign(gtk::Align::Center);
    page.set_halign(gtk::Align::Center);
    let title = gtk::Label::new(Some("Burrow"));
    title.add_css_class("welcome-title");
    page.append(&title);
    for (keys, what) in [
        ("Ctrl+Shift+T", "New terminal"),
        ("Ctrl+Shift+P", "Command palette"),
        ("Ctrl+Shift+B", "Toggle sidebar"),
        ("Ctrl+Shift+S", "Focus sidebar"),
        ("Ctrl+Shift+W", "Close tab"),
        ("F2", "Rename the selected item"),
        // Not keys, but the two things the window doesn't show by itself.
        ("+", "Add a host, tunnel or database"),
        ("Right-click", "Open a host's files or monitor"),
    ] {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        let key = gtk::Label::new(Some(keys));
        key.add_css_class("kbd");
        key.set_width_chars(12);
        let label = gtk::Label::new(Some(what));
        label.add_css_class("dim");
        row.append(&key);
        row.append(&label);
        page.append(&row);
    }
    page
}

fn send_on_click(button: &gtk::Button, sender: &ComponentSender<App>, message: AppMsg) {
    let sender = sender.clone();
    button.connect_clicked(move |_| sender.input(message.clone()));
}

fn add_shortcuts(window: &gtk::ApplicationWindow, sender: &ComponentSender<App>) {
    // Plain Ctrl keys belong to the shell (Ctrl+W deletes a word, Ctrl+R
    // searches history), so Burrow's own use Ctrl+Shift, as other GTK
    // terminals do.
    let controller = gtk::ShortcutController::new();
    controller.set_scope(gtk::ShortcutScope::Global);
    // Capture runs before the focused widget sees the key. In the default
    // bubble phase a terminal would send these keys to the shell first.
    controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    let shortcuts = [
        ("<Control><Shift>t", AppMsg::NewTerminal(None)),
        ("<Control><Shift>p", AppMsg::ShowPalette),
        ("<Control><Shift>b", AppMsg::ToggleSidebar),
        // A terminal keeps Tab for itself, so this is the way back.
        ("<Control><Shift>s", AppMsg::FocusSidebar),
        ("<Control><Shift>w", AppMsg::CloseCurrentTab),
        ("<Control><Shift>r", AppMsg::ReloadConfig),
        ("<Control>comma", AppMsg::ShowSettings),
        ("<Control>Page_Down", AppMsg::SwitchTab(1)),
        ("<Control>Page_Up", AppMsg::SwitchTab(-1)),
        ("<Control>Tab", AppMsg::SwitchTab(1)),
        // GTK matches this to the ISO_Left_Tab that Shift+Tab sends.
        ("<Control><Shift>Tab", AppMsg::SwitchTab(-1)),
    ];
    for (trigger, message) in shortcuts {
        let sender = sender.clone();
        let action = gtk::CallbackAction::new(move |_, _| {
            sender.input(message.clone());
            glib::Propagation::Stop
        });
        controller.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string(trigger),
            Some(action),
        ));
    }
    window.add_controller(controller);
}

fn show_settings(window: &gtk::ApplicationWindow, current: Theme, sender: &ComponentSender<App>) {
    let dialog = gtk::Window::builder()
        .title("Settings")
        .modal(true)
        .transient_for(window)
        .resizable(false)
        .default_width(360)
        .build();
    let grid = gtk::Grid::builder()
        .row_spacing(12)
        .column_spacing(16)
        .margin_top(18)
        .margin_bottom(18)
        .margin_start(18)
        .margin_end(18)
        .build();

    let label = gtk::Label::new(Some("Theme"));
    label.set_xalign(0.0);
    label.set_hexpand(true);
    let names: Vec<&str> = Theme::ALL.iter().map(|t| t.label()).collect();
    let theme = gtk::DropDown::from_strings(&names);
    theme.set_selected(Theme::ALL.iter().position(|&t| t == current).unwrap_or(0) as u32);
    theme.connect_selected_notify({
        let sender = sender.clone();
        move |dropdown| {
            if let Some(&theme) = Theme::ALL.get(dropdown.selected() as usize) {
                sender.input(AppMsg::SetTheme(theme));
            }
        }
    });
    grid.attach(&label, 0, 0, 1, 1);
    grid.attach(&theme, 1, 0, 1, 1);
    dialog.set_child(Some(&grid));
    dialogs::close_on_escape(&dialog);
    dialog.present();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_are_named_as_the_api_names_them() {
        assert_eq!(command_name(&Command::ListNodes), "list_nodes");
        let node = NodeId::new();
        assert_eq!(command_name(&Command::StartTunnel { node }), "start_tunnel");
        let close = Command::CloseSession {
            session: SessionId(7),
        };
        assert_eq!(command_name(&close), "close_session");
    }

    #[test]
    fn kind_labels_start_with_a_capital() {
        assert_eq!(sentence_case("folder"), "Folder");
        assert_eq!(sentence_case("SSH host"), "SSH host");
        assert_eq!(sentence_case(""), "");
    }

    #[test]
    fn the_error_log_keeps_the_newest() {
        let mut log = ErrorLog::default();
        for n in 0..MAX_ERRORS + 10 {
            log.push(format!("t{n}"), format!("error {n}"));
        }
        assert_eq!(log.0.len(), MAX_ERRORS);
        let text = log.text();
        let first = text.lines().next().unwrap();
        assert_eq!(first, format!("t{0}  error {0}", MAX_ERRORS + 9));
        assert!(!text.contains("error 9\n") && text.ends_with("error 10"));
    }
}
