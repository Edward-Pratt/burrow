//! The main window: header bar, sidebar tree, tabs and status bar.
//!
//! State flows one way. User actions become core [`Command`]s; the core
//! answers with [`Event`]s; events update `App` and the view is redrawn from
//! it. The UI never edits its node list directly, so it always matches the
//! core, including after edits made outside Burrow.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use burrow_core::{
    AddonInfo, Command, Core, Event, FormValue, Node, NodeId, NodePatch, Problem, Prompt, Response,
    SSH_HOST_KIND, SSH_TUNNEL_KIND, SessionId, SessionInfo, SessionKind, SessionState, View,
    Watcher,
};
use relm4::gtk::prelude::*;
use relm4::gtk::{gdk, gio, glib};
use relm4::{Component, ComponentParts, ComponentSender, gtk};
use tokio::sync::broadcast::error::RecvError;
use vte4::TerminalExt;

use crate::addons;
use crate::dialogs;
use crate::files::{self, Files};
use crate::palette::{self, Item};
use crate::remote::{self, Remote, Unclaimed};
use crate::runtime::runtime;
use crate::settings::{Theme, UiSettings};
use crate::sidebar;
use crate::terminal;
use crate::tunnels;

pub struct Init {
    pub core: Core,
    pub config_dir: PathBuf,
}

/// How long a message from an addon stays in the status bar.
const NOTICE_TIME: Duration = Duration::from_secs(6);

/// What "New …" creates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NewKind {
    Folder,
    SshHost,
    SshTunnel,
    /// A kind owned by an addon.
    Addon(String),
}

#[derive(Clone, Debug)]
pub enum AppMsg {
    // From the core.
    Loaded {
        nodes: Vec<Node>,
        problems: Vec<Problem>,
        addons: Vec<AddonInfo>,
        sessions: Vec<SessionInfo>,
        autostart: Vec<NodeId>,
    },
    CoreEvent(Event),
    /// The core's sessions, looked up to find whose a prompt is.
    SessionsListed(Vec<SessionInfo>),
    /// We missed events; reload everything.
    Resync,
    Failed(String),

    // From the user.
    RowActivated(i32),
    ContextMenu {
        row: Option<i32>,
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
    RenameSelected,
    DeleteSelected,
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
    /// Opens an SSH host's file panel.
    OpenFiles(NodeId),
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
    SetAutostart {
        node: NodeId,
        enabled: bool,
    },
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

/// What a tab shows. Node tabs follow their node; terminal tabs belong to
/// no node and are numbered as they open.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TabKey {
    Node(NodeId),
    /// An SSH host's file panel. The host's own tab is its terminal.
    Files(NodeId),
    Terminal(u32),
}

impl TabKey {
    fn node(self) -> Option<NodeId> {
        match self {
            TabKey::Node(id) | TabKey::Files(id) => Some(id),
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
    core: Core,
    config_dir: PathBuf,
    settings: UiSettings,
    nodes: HashMap<NodeId, Node>,
    expanded: HashSet<NodeId>,
    problems: Vec<Problem>,
    addons: Vec<AddonInfo>,
    error: Option<String>,
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
    /// Watches the config folder while the window is open.
    _watcher: Option<Watcher>,
    sidebar_dirty: bool,
}

pub struct Widgets {
    sidebar_pane: gtk::Box,
    list: gtk::ListBox,
    /// The node shown in each sidebar row, by row index.
    row_ids: Vec<NodeId>,
    content: gtk::Stack,
    notebook: gtk::Notebook,
    new_button: gtk::MenuButton,
    status: gtk::Label,
    problems_button: gtk::MenuButton,
    problems_list: gtk::Label,
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

        // Subscribe before listing, so nothing that happens in between is
        // missed.
        listen(&core, &sender);
        load(&core, &sender);
        {
            let _runtime = runtime().enter();
            core.autostart();
        }
        let (watcher, error) = {
            let _runtime = runtime().enter();
            match core.watch() {
                Ok(watcher) => (Some(watcher), None),
                Err(e) => (None, Some(format!("Live reload is off: {e}"))),
            }
        };

        let widgets = build(&window, &sender);
        widgets.sidebar_pane.set_visible(settings.sidebar_visible);
        add_shortcuts(&window, &sender);

        let model = App {
            core,
            config_dir,
            settings,
            nodes: HashMap::new(),
            expanded: HashSet::new(),
            problems: Vec::new(),
            addons: Vec::new(),
            error,
            notice: None,
            notice_id: 0,
            tabs: HashMap::new(),
            next_terminal: 1,
            remotes: HashMap::new(),
            files: HashMap::new(),
            views: HashMap::new(),
            unclaimed: Unclaimed::default(),
            looking_up: false,
            tunnels: HashMap::new(),
            tunnel_pages: HashMap::new(),
            restarting: HashSet::new(),
            autostart: HashSet::new(),
            _watcher: watcher,
            sidebar_dirty: false,
        };
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
        match message {
            AppMsg::Loaded {
                nodes,
                problems,
                addons,
                sessions,
                autostart,
            } => {
                self.nodes = nodes.into_iter().map(|n| (n.id, n)).collect();
                self.problems = problems;
                self.autostart = autostart.into_iter().collect();
                self.set_addons(addons, widgets, &sender);
                // Events may have been missed, so a tunnel that isn't
                // running any more has stopped.
                for status in self.tunnels.values_mut() {
                    if let Some(session) = status.running()
                        && !sessions.iter().any(|s| s.session == session)
                    {
                        status.session = Some((session, SessionState::Closed { error: None }));
                    }
                }
                self.take_sessions(sessions, window, &sender);
                let gone: Vec<TabKey> = self
                    .tabs
                    .keys()
                    .filter(|key| key.node().is_some_and(|id| !self.nodes.contains_key(&id)))
                    .copied()
                    .collect();
                for key in gone {
                    self.close_tab(key, widgets);
                }
                self.sidebar_dirty = true;
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
            AppMsg::Failed(error) => self.error = Some(error),

            AppMsg::RowActivated(index) => {
                if let Some(&id) = widgets.row_ids.get(index as usize) {
                    self.activate(id, widgets, &sender);
                }
            }
            AppMsg::ContextMenu { row, x, y } => {
                let id = row.and_then(|i| widgets.row_ids.get(i as usize).copied());
                if let Some(row) = row.and_then(|i| widgets.list.row_at_index(i)) {
                    widgets.list.select_row(Some(&row));
                }
                self.context_menu(id, x, y, widgets, &sender);
            }
            AppMsg::New { kind, parent } => {
                let parent = parent.unwrap_or_else(|| self.selection_folder(widgets));
                self.new_node(kind, parent, window, &sender);
            }
            AppMsg::Open(id) => self.open_tab(id, widgets, &sender),
            AppMsg::Rename(id) => self.rename(id, window, &sender),
            AppMsg::Delete(id) => self.delete(id, window, &sender),
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
                    Some(remote) if remote.state == remote::State::Starting => {
                        remote.state = remote::State::Running(session);
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
                    remote.state = remote::State::Closed;
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
                    .find(|r| r.state == remote::State::Running(session))
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
                    remote::State::Running(session) => execute(
                        &self.core,
                        &sender,
                        Command::WriteTerminal { session, data },
                    ),
                    remote::State::Closed if data.contains(&b'\r') => {
                        remote.terminal.feed(b"\r\n");
                        remote.state = remote::State::Starting;
                        remote::start(&self.core, node, &remote.terminal, &sender);
                    }
                    _ => {}
                }
            }
            AppMsg::RemoteResize { node, cols, rows } => {
                if let Some(&Remote {
                    state: remote::State::Running(session),
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
            AppMsg::OpenFiles(id) => self.open_files(id, widgets, &sender),
            AppMsg::Files {
                node,
                msg: files::Msg::Started(session),
            } => match self.files.get_mut(&node) {
                Some(panel) if panel.state == remote::State::Starting => {
                    panel.state = remote::State::Running(session);
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
            AppMsg::SetAutostart { node, enabled } => {
                let (core, input) = (self.core.clone(), sender.input_sender().clone());
                runtime().spawn(async move {
                    let command = Command::SetAutostart { node, enabled };
                    if let Err(e) = core.execute(command).await {
                        let error = e.to_string();
                        let _ = input.send(AppMsg::AutostartFailed { node, error });
                    }
                });
            }
            AppMsg::AutostartFailed { node, error } => {
                self.error = Some(error);
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
                    let message = match core.execute(command).await {
                        Ok(Response::Invoked {
                            message: Some(message),
                        }) => AppMsg::Notice(message),
                        Ok(_) => return,
                        Err(e) => AppMsg::Failed(e.to_string()),
                    };
                    let _ = input.send(message);
                });
            }
            AppMsg::Notice(message) => {
                self.error = None;
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
                let folder = gio::File::for_path(self.core.addons_dir());
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

        if self.sidebar_dirty {
            self.sidebar_dirty = false;
            let selection = selected(widgets);
            // Rebuilding destroys the focused row; remember whether the
            // sidebar had focus so keyboard use (arrows, Delete) carries on.
            let had_focus = widgets.list.focus_child().is_some();
            let running: HashSet<NodeId> = self
                .tunnels
                .iter()
                .filter(|(_, status)| status.is_up())
                .map(|(id, _)| *id)
                .collect();
            let look = sidebar::Look {
                expanded: &self.expanded,
                icons: &self.icons(),
                running: &running,
            };
            widgets.row_ids = sidebar::rebuild(&widgets.list, &self.nodes, &look);
            let row = selection
                .and_then(|id| widgets.row_ids.iter().position(|&r| r == id))
                .and_then(|index| widgets.list.row_at_index(index as i32));
            if let Some(row) = row {
                widgets.list.select_row(Some(&row));
                if had_focus {
                    row.grab_focus();
                }
            }
        }
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

impl App {
    fn apply_event(&mut self, event: Event, widgets: &Widgets, sender: &ComponentSender<Self>) {
        self.error = None;
        match event {
            Event::NodeCreated { node } => {
                // Show where the new node went.
                if let Some(parent) = node.parent {
                    self.expanded.insert(parent);
                }
                self.nodes.insert(node.id, node);
                self.sidebar_dirty = true;
            }
            Event::NodeUpdated { node } => {
                let id = node.id;
                if let Some(tab) = self.tabs.get(&TabKey::Files(id)) {
                    set_tab_title(&tab.title, &files_title(&node.name));
                }
                if let Some(tab) = self.tabs.get(&TabKey::Node(id)) {
                    set_tab_title(&tab.title, &node.name);
                    if !self.tunnel_pages.contains_key(&id)
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
                self.nodes.insert(id, node);
                self.sidebar_dirty = true;
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
                self.expanded.remove(&id);
                self.tunnels.remove(&id);
                self.restarting.remove(&id);
                self.close_tab(TabKey::Node(id), widgets);
                self.close_tab(TabKey::Files(id), widgets);
                self.sidebar_dirty = true;
                self.refresh_tunnels(sender);
            }
            Event::ProblemsChanged { problems } => self.problems = problems,
            // Handled by `session_event`, which needs the window.
            Event::SessionChanged { .. } | Event::PromptRequested { .. } => {}
            Event::AddonsChanged { addons } => self.set_addons(addons, widgets, sender),
            Event::ViewChanged { node, view } => self.show_view(node, Ok(view), sender),
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
                let running = remote::State::Running(session);
                let panel = self
                    .files
                    .values_mut()
                    .find(|p| p.has_transfer(transfer) || p.state == running);
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
        self.sidebar_dirty = true;
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
        let mut kinds = vec![
            ("folder".to_string(), NewKind::Folder),
            ("SSH host".to_string(), NewKind::SshHost),
            ("SSH tunnel".to_string(), NewKind::SshTunnel),
        ];
        for addon in self.addons.iter().filter(|a| addons::usable(a)) {
            for kind in &addon.kinds {
                kinds.push((kind.label.clone(), NewKind::Addon(kind.kind.clone())));
            }
        }
        kinds
            .into_iter()
            .map(|(label, kind)| (format!("New {label}{suffix}"), AppMsg::New { kind, parent }))
            .collect()
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

    fn remote_for(&self, session: SessionId) -> Option<&Remote> {
        self.remotes
            .values()
            .find(|r| r.state == remote::State::Running(session))
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
        let running = remote::State::Running(session);
        if let Some(panel) = self.files.values_mut().find(|p| p.state == running) {
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
            .find(|r| r.state == remote::State::Running(session))
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
        dialogs::answer(window, prompt, move |answer| {
            let command = Command::AnswerPrompt {
                session,
                prompt: id,
                answer,
            };
            execute(&core, &sender, command);
        });
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
        self.sidebar_dirty = true;
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
        self.sidebar_dirty = true;
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
        let (core, input) = (self.core.clone(), sender.input_sender().clone());
        tunnels::edit(window, node, hosts, move |name, props| {
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
                let result = match core.execute(command).await {
                    Ok(Response::Node { node }) if autostart => {
                        let command = Command::SetAutostart {
                            node: node.id,
                            enabled: true,
                        };
                        core.execute(command).await
                    }
                    other => other,
                };
                if let Err(e) = result {
                    let _ = input.send(AppMsg::Failed(e.to_string()));
                }
            });
        });
    }

    fn activate(&mut self, id: NodeId, widgets: &Widgets, sender: &ComponentSender<Self>) {
        let Some(node) = self.nodes.get(&id) else {
            return;
        };
        if node.is_folder() {
            if !self.expanded.remove(&id) {
                self.expanded.insert(id);
            }
            self.sidebar_dirty = true;
        } else {
            self.open_tab_now(id, widgets, sender);
        }
    }

    fn open_tab(&mut self, id: NodeId, widgets: &Widgets, sender: &ComponentSender<Self>) {
        // Reveal the node in the sidebar, then open it.
        let mut parent = self.nodes.get(&id).and_then(|n| n.parent);
        while let Some(folder) = parent {
            self.expanded.insert(folder);
            parent = self.nodes.get(&folder).and_then(|n| n.parent);
        }
        self.sidebar_dirty = true;
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
                |_| {},
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
            TabKey::Node(id) => self.remotes.remove(&id).map(|r| r.state),
            TabKey::Files(id) => self.files.remove(&id).map(|f| f.state),
            TabKey::Terminal(_) => None,
        };
        if let Some(remote::State::Running(session)) = state {
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
            NewKind::SshHost => (
                "New SSH host".to_string(),
                "Host alias (as in ~/.ssh/config)",
            ),
            NewKind::SshTunnel => return self.edit_tunnel(None, parent, window, sender),
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
                NewKind::SshHost => {
                    let mut props = toml::Table::new();
                    props.insert("alias".into(), name.clone().into());
                    ("ssh-host", props)
                }
                NewKind::Addon(kind) => (kind.as_str(), toml::Table::new()),
                NewKind::SshTunnel => unreachable!("tunnels have their own dialog"),
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
        let detail = if node.is_folder() {
            "Everything inside it is deleted too, along with any saved passwords."
        } else {
            "Its saved passwords are removed from the keyring too."
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
                }
                if node.kind == SSH_TUNNEL_KIND {
                    let running = self.tunnels.get(&node.id).and_then(|s| s.running());
                    entries.push(match running {
                        Some(_) => ("Stop".into(), AppMsg::StopTunnel(node.id)),
                        None => ("Start".into(), AppMsg::StartTunnel(node.id)),
                    });
                    entries.push(("Edit…".into(), AppMsg::EditTunnel(node.id)));
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
            entries.push(("Delete…".into(), AppMsg::Delete(node.id)));
        }
        let popover = menu_popover(entries, sender);
        popover.set_parent(&widgets.list);
        popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        popover.connect_closed(|popover| {
            // Unparent after the close animation's signal returns.
            let popover = popover.clone();
            glib::idle_add_local_once(move || popover.unparent());
        });
        popover.popup();
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
            command("Manage addons", AppMsg::ManageAddons),
            command("Settings", AppMsg::ShowSettings),
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
                .map(|node| Item {
                    title: format!("Browse files: {}", node.name),
                    subtitle: sidebar::location(&self.nodes, node),
                    message: AppMsg::OpenFiles(node.id),
                }),
        );
        items.extend(nodes.into_iter().map(|node| {
            Item {
                title: node.name.clone(),
                subtitle: [node.kind.as_str(), &sidebar::location(&self.nodes, node)]
                    .iter()
                    .filter(|s| !s.is_empty())
                    .copied()
                    .collect::<Vec<_>>()
                    .join(" · "),
                message: AppMsg::Open(node.id),
            }
        }));
        let sender = sender.clone();
        palette::show(window, items, move |message| sender.input(message));
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
            self.error = Some(format!("Could not save settings: {e}"));
        }
    }

    fn refresh_status(&self, widgets: &Widgets) {
        match (&self.error, &self.notice) {
            (Some(error), _) => {
                widgets.status.set_label(error);
                widgets.status.add_css_class("error");
            }
            (None, Some(notice)) => {
                widgets.status.set_label(notice);
                widgets.status.remove_css_class("error");
            }
            (None, None) => {
                let leaves = self.nodes.values().filter(|n| !n.is_folder()).count();
                let folders = self.nodes.len() - leaves;
                widgets.status.set_label(&format!(
                    "{leaves} {} · {folders} {}",
                    plural(leaves, "item", "items"),
                    plural(folders, "folder", "folders"),
                ));
                widgets.status.remove_css_class("error");
            }
        }
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
fn execute(core: &Core, sender: &ComponentSender<App>, command: Command) {
    let (core, input) = (core.clone(), sender.input_sender().clone());
    runtime().spawn(async move {
        if let Err(e) = core.execute(command).await {
            let _ = input.send(AppMsg::Failed(e.to_string()));
        }
    });
}

/// Loads the full node list, problems, addons, sessions and autostart
/// tunnels.
fn load(core: &Core, sender: &ComponentSender<App>) {
    let (core, input) = (core.clone(), sender.input_sender().clone());
    runtime().spawn(async move {
        let result = async {
            let nodes = match core.execute(Command::ListNodes).await? {
                Response::Nodes { nodes } => nodes,
                _ => Vec::new(),
            };
            let problems = match core.execute(Command::ListProblems).await? {
                Response::Problems { problems } => problems,
                _ => Vec::new(),
            };
            let addons = match core.execute(Command::ListAddons).await? {
                Response::Addons { addons } => addons,
                _ => Vec::new(),
            };
            let sessions = match core.execute(Command::ListSessions).await? {
                Response::Sessions { sessions } => sessions,
                _ => Vec::new(),
            };
            let autostart = match core.execute(Command::ListAutostart).await? {
                Response::Autostart { nodes } => nodes,
                _ => Vec::new(),
            };
            Ok::<_, burrow_core::Error>(AppMsg::Loaded {
                nodes,
                problems,
                addons,
                sessions,
                autostart,
            })
        }
        .await;
        let _ = input.send(result.unwrap_or_else(|e| AppMsg::Failed(e.to_string())));
    });
}

/// Looks up every running session, for [`AppMsg::SessionsListed`].
fn list_sessions(core: &Core, sender: &ComponentSender<App>) {
    let (core, input) = (core.clone(), sender.input_sender().clone());
    runtime().spawn(async move {
        let sessions = match core.execute(Command::ListSessions).await {
            Ok(Response::Sessions { sessions }) => sessions,
            _ => Vec::new(),
        };
        let _ = input.send(AppMsg::SessionsListed(sessions));
    });
}

/// Forwards core events to the window until it closes.
fn listen(core: &Core, sender: &ComponentSender<App>) {
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
                break; // The window is gone.
            }
        }
    });
}

fn selected(widgets: &Widgets) -> Option<NodeId> {
    let row = widgets.list.selected_row()?;
    widgets.row_ids.get(row.index() as usize).copied()
}

fn close_page(notebook: &gtk::Notebook, page: &gtk::Widget) {
    if let Some(index) = notebook.page_num(page) {
        notebook.remove_page(Some(index));
    }
}

/// Long titles (a shell's working folder, say) are cut short with "…".
/// Notebooks give tab labels only their minimum width, and an ellipsized
/// label's minimum is just the "…", so the minimum is set to fit the text.
fn set_tab_title(label: &gtk::Label, title: &str) {
    const MAX_CHARS: usize = 24;
    label.set_label(title);
    label.set_width_chars(title.chars().count().min(MAX_CHARS) as i32);
}

/// The title of an SSH host's file panel tab.
fn files_title(host: &str) -> String {
    format!("{host} files")
}

fn plural<'a>(n: usize, one: &'a str, many: &'a str) -> &'a str {
    if n == 1 { one } else { many }
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
        button.connect_clicked({
            let (popover, sender) = (popover.clone(), sender.clone());
            move |_| {
                popover.popdown();
                sender.input(message.clone());
            }
        });
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
            ("Settings    Ctrl+,", AppMsg::ShowSettings),
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
    let list = gtk::ListBox::new();
    list.add_css_class("navigation-sidebar");
    list.set_selection_mode(gtk::SelectionMode::Single);
    list.connect_row_activated({
        let sender = sender.clone();
        move |_, row| sender.input(AppMsg::RowActivated(row.index()))
    });
    let right_click = gtk::GestureClick::new();
    right_click.set_button(gdk::BUTTON_SECONDARY);
    right_click.connect_pressed({
        let (sender, list) = (sender.clone(), list.clone());
        move |_, _, x, y| {
            let row = list.row_at_y(y as i32).map(|r| r.index());
            sender.input(AppMsg::ContextMenu { row, x, y });
        }
    });
    list.add_controller(right_click);
    // Delete and F2 only apply while the sidebar has focus, so they never
    // eat a keypress meant for a text field or a terminal.
    let keys = gtk::EventControllerKey::new();
    keys.connect_key_pressed({
        let sender = sender.clone();
        move |_, key, _, _| {
            let message = match key {
                gdk::Key::Delete => AppMsg::DeleteSelected,
                gdk::Key::F2 => AppMsg::RenameSelected,
                _ => return glib::Propagation::Proceed,
            };
            sender.input(message);
            glib::Propagation::Stop
        }
    });
    list.add_controller(keys);

    let scroller = gtk::ScrolledWindow::builder()
        .child(&list)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    let sidebar_pane = gtk::Box::new(gtk::Orientation::Vertical, 0);
    sidebar_pane.add_css_class("sidebar");
    sidebar_pane.set_size_request(240, -1);
    sidebar_pane.append(&scroller);

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
    let statusbar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    statusbar.add_css_class("statusbar");
    statusbar.append(&status);
    statusbar.append(&problems_button);

    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.append(&paned);
    root.append(&statusbar);
    window.set_child(Some(&root));

    Widgets {
        sidebar_pane,
        list,
        row_ids: Vec::new(),
        content,
        notebook,
        new_button,
        status,
        problems_button,
        problems_list,
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
        ("Ctrl+Shift+W", "Close tab"),
        ("F2", "Rename the selected item"),
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
        ("<Control><Shift>w", AppMsg::CloseCurrentTab),
        ("<Control><Shift>r", AppMsg::ReloadConfig),
        ("<Control>comma", AppMsg::ShowSettings),
        ("<Control>Page_Down", AppMsg::SwitchTab(1)),
        ("<Control>Page_Up", AppMsg::SwitchTab(-1)),
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
