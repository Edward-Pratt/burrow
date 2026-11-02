//! The main window: header bar, sidebar tree, tabs and status bar.
//!
//! State flows one way. User actions become core [`Command`]s; the core
//! answers with [`Event`]s; events update `App` and the view is redrawn from
//! it. The UI never edits its node list directly, so it always matches the
//! core, including after edits made outside Burrow.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Instant;

use burrow_core::ipc::Client;
use burrow_core::{AddonInfo, Alert, Node, NodeId, Problem, SessionId};
use relm4::gtk::glib;
use relm4::gtk::prelude::*;
use relm4::{Component, ComponentParts, ComponentSender, gtk};

use crate::dialogs;
use crate::layout::Layout;
use crate::remote::Unclaimed;
use crate::settings::UiSettings;
use crate::sidebar;

mod chrome;
mod daemon;
mod dispatch;
mod editors;
mod events;
mod grants;
mod menus;
mod message;
mod monitors;
mod nodes;
mod open;
mod panels;
mod sessions;
mod settings_dialog;
mod shells;
mod status;
mod tabs;
mod tunnels;
mod views;
mod window;

pub(crate) use menus::{menu_popover, sentence_case};
pub use message::AppMsg;
pub use tabs::TabKey;

use chrome::{add_shortcuts, build};
use daemon::{listen, load, stale_daemon};
use status::ErrorLog;
use tabs::Tab;

pub struct Init {
    pub core: Client,
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
}

pub struct App {
    core: Client,
    config_dir: PathBuf,
    state_dir: PathBuf,
    settings: UiSettings,
    /// The tabs to open again, until the first list of nodes arrives. The
    /// file isn't written until then, so it keeps them meanwhile.
    restore: Option<Layout>,
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
    /// The number the next shell or terminal tab gets.
    next_tab: u32,
    unclaimed: Unclaimed,
    /// Whether a `list_sessions` to place an unknown session's prompt is
    /// on its way.
    looking_up: bool,
    /// Every tunnel that has run, by node, whether or not its tab is open.
    tunnels: HashMap<NodeId, crate::tunnels::Status>,
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
        let Init {
            core,
            config_dir,
            state_dir,
        } = init;
        let settings = UiSettings::load(&config_dir);
        let restore = Some(Layout::load(&state_dir));
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
            state_dir,
            settings,
            restore,
            nodes: HashMap::new(),
            problems: Vec::new(),
            addons: Vec::new(),
            error: None,
            errors: ErrorLog::default(),
            reconnecting: false,
            notice: None,
            notice_id: 0,
            tabs: HashMap::new(),
            next_tab: 1,
            unclaimed: Unclaimed::default(),
            looking_up: false,
            tunnels: HashMap::new(),
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
        self.dispatch(widgets, message, sender, window);

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
        self.refresh_tabs();
        self.refresh_status(widgets);
    }
}
