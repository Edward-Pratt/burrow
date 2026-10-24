//! The main window: header bar, sidebar tree, tabs and status bar.
//!
//! State flows one way. User actions become core [`Command`]s; the core
//! answers with [`Event`]s; events update `App` and the view is redrawn from
//! it. The UI never edits its node list directly, so it always matches the
//! core, including after edits made outside Burrow.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use burrow_core::{Command, Core, Event, Node, NodeId, NodePatch, Problem, Response, Watcher};
use relm4::gtk::prelude::*;
use relm4::gtk::{gdk, gio, glib};
use relm4::{Component, ComponentParts, ComponentSender, gtk};
use tokio::sync::broadcast::error::RecvError;

use crate::dialogs;
use crate::palette::{self, Item};
use crate::runtime::runtime;
use crate::settings::{Theme, UiSettings};
use crate::sidebar;

pub struct Init {
    pub core: Core,
    pub config_dir: PathBuf,
}

/// What "New …" creates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NewKind {
    Folder,
    SshHost,
}

#[derive(Clone, Debug)]
pub enum AppMsg {
    // From the core.
    Loaded {
        nodes: Vec<Node>,
        problems: Vec<Problem>,
    },
    CoreEvent(Event),
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
    CloseTab(NodeId),
    CloseCurrentTab,
    ShowPalette,
    ToggleSidebar,
    ShowSettings,
    SetTheme(Theme),
    ReloadConfig,
    OpenConfigFolder,
}

struct Tab {
    page: gtk::Box,
    title: gtk::Label,
}

pub struct App {
    core: Core,
    config_dir: PathBuf,
    settings: UiSettings,
    /// The platform's color scheme at startup, restored for
    /// [`Theme::System`].
    system_scheme: gtk::InterfaceColorScheme,
    nodes: HashMap<NodeId, Node>,
    expanded: HashSet<NodeId>,
    problems: Vec<Problem>,
    error: Option<String>,
    tabs: HashMap<NodeId, Tab>,
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
        let system_scheme = gtk::Settings::default()
            .map_or(gtk::InterfaceColorScheme::Default, |s| {
                s.gtk_interface_color_scheme()
            });

        // Subscribe before listing, so nothing that happens in between is
        // missed.
        listen(&core, &sender);
        load(&core, &sender);
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
            system_scheme,
            nodes: HashMap::new(),
            expanded: HashSet::new(),
            problems: Vec::new(),
            error,
            tabs: HashMap::new(),
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
            AppMsg::Loaded { nodes, problems } => {
                self.nodes = nodes.into_iter().map(|n| (n.id, n)).collect();
                self.problems = problems;
                self.tabs.retain(|id, tab| {
                    let keep = self.nodes.contains_key(id);
                    if !keep {
                        close_page(&widgets.notebook, &tab.page);
                    }
                    keep
                });
                self.sidebar_dirty = true;
            }
            AppMsg::CoreEvent(event) => self.apply_event(event, widgets),
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
            AppMsg::CloseTab(id) => self.close_tab(id, widgets),
            AppMsg::CloseCurrentTab => {
                let current = widgets.notebook.nth_page(widgets.notebook.current_page());
                let id = self
                    .tabs
                    .iter()
                    .find(|(_, tab)| current.as_ref() == Some(tab.page.upcast_ref()))
                    .map(|(id, _)| *id);
                if let Some(id) = id {
                    self.close_tab(id, widgets);
                }
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
            widgets.row_ids = sidebar::rebuild(&widgets.list, &self.nodes, &self.expanded);
            if let Some(index) =
                selection.and_then(|id| widgets.row_ids.iter().position(|&r| r == id))
            {
                widgets
                    .list
                    .select_row(widgets.list.row_at_index(index as i32).as_ref());
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
    fn apply_event(&mut self, event: Event, widgets: &Widgets) {
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
                if let Some(tab) = self.tabs.get(&node.id) {
                    tab.title.set_label(&node.name);
                    fill_page(&tab.page, &node, &self.nodes);
                }
                self.nodes.insert(node.id, node);
                self.sidebar_dirty = true;
            }
            Event::NodeDeleted { id } => {
                self.nodes.remove(&id);
                self.expanded.remove(&id);
                if let Some(tab) = self.tabs.remove(&id) {
                    close_page(&widgets.notebook, &tab.page);
                }
                self.sidebar_dirty = true;
            }
            Event::ProblemsChanged { problems } => self.problems = problems,
        }
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
        let notebook = &widgets.notebook;
        if let Some(tab) = self.tabs.get(&id) {
            if let Some(index) = notebook.page_num(&tab.page) {
                notebook.set_current_page(Some(index));
            }
            return;
        }
        let Some(node) = self.nodes.get(&id) else {
            return;
        };

        let page = gtk::Box::new(gtk::Orientation::Vertical, 8);
        fill_page(&page, node, &self.nodes);

        let title = gtk::Label::new(Some(&node.name));
        let close = gtk::Button::from_icon_name("window-close-symbolic");
        close.add_css_class("flat");
        close.add_css_class("tab-close");
        close.set_tooltip_text(Some("Close tab (Ctrl+W)"));
        let tab_label = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        tab_label.append(&gtk::Image::from_icon_name(sidebar::icon_name(node, false)));
        tab_label.append(&title);
        tab_label.append(&close);

        let index = notebook.append_page(&page, Some(&tab_label));
        notebook.set_tab_reorderable(&page, true);
        notebook.set_current_page(Some(index));

        send_on_click(&close, sender, AppMsg::CloseTab(id));
        self.tabs.insert(id, Tab { page, title });
    }

    fn close_tab(&mut self, id: NodeId, widgets: &Widgets) {
        if let Some(tab) = self.tabs.remove(&id) {
            close_page(&widgets.notebook, &tab.page);
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
        let (title, label) = match kind {
            NewKind::Folder => ("New folder", "Folder name"),
            NewKind::SshHost => ("New SSH host", "Host alias (as in ~/.ssh/config)"),
        };
        let (core, sender) = (self.core.clone(), sender.clone());
        dialogs::prompt(window, title, label, "", "Create", move |name| {
            let (kind, props) = match kind {
                NewKind::Folder => ("folder", toml::Table::new()),
                NewKind::SshHost => {
                    let mut props = toml::Table::new();
                    props.insert("alias".into(), name.clone().into());
                    ("ssh-host", props)
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
        let mut entries: Vec<(&str, AppMsg)> = Vec::new();
        match node {
            Some(node) if node.is_folder() => {
                let parent = Some(Some(node.id));
                entries.push((
                    "New folder here",
                    AppMsg::New {
                        kind: NewKind::Folder,
                        parent,
                    },
                ));
                entries.push((
                    "New SSH host here",
                    AppMsg::New {
                        kind: NewKind::SshHost,
                        parent,
                    },
                ));
            }
            Some(node) => entries.push(("Open", AppMsg::Open(node.id))),
            None => {
                let parent = Some(None);
                entries.push((
                    "New folder",
                    AppMsg::New {
                        kind: NewKind::Folder,
                        parent,
                    },
                ));
                entries.push((
                    "New SSH host",
                    AppMsg::New {
                        kind: NewKind::SshHost,
                        parent,
                    },
                ));
            }
        }
        if let Some(node) = node {
            entries.push(("Rename…", AppMsg::Rename(node.id)));
            entries.push(("Delete…", AppMsg::Delete(node.id)));
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
        let mut items = vec![
            command(
                "New folder",
                AppMsg::New {
                    kind: NewKind::Folder,
                    parent: None,
                },
            ),
            command(
                "New SSH host",
                AppMsg::New {
                    kind: NewKind::SshHost,
                    parent: None,
                },
            ),
            command("Toggle sidebar", AppMsg::ToggleSidebar),
            command("Reload config", AppMsg::ReloadConfig),
            command("Open config folder", AppMsg::OpenConfigFolder),
            command("Settings", AppMsg::ShowSettings),
        ];
        let mut nodes: Vec<&Node> = self.nodes.values().collect();
        nodes.sort_by_key(|n| (n.is_folder(), n.name.to_lowercase()));
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
        settings.set_gtk_interface_color_scheme(match self.settings.theme {
            Theme::System => self.system_scheme,
            Theme::Light => gtk::InterfaceColorScheme::Light,
            Theme::Dark => gtk::InterfaceColorScheme::Dark,
        });
    }

    fn save_settings(&mut self) {
        if let Err(e) = self.settings.save(&self.config_dir) {
            self.error = Some(format!("Could not save settings: {e}"));
        }
    }

    fn refresh_status(&self, widgets: &Widgets) {
        match &self.error {
            Some(error) => {
                widgets.status.set_label(error);
                widgets.status.add_css_class("error");
            }
            None => {
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

/// Loads the full node list and problems.
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
            Ok::<_, burrow_core::Error>(AppMsg::Loaded { nodes, problems })
        }
        .await;
        let _ = input.send(result.unwrap_or_else(|e| AppMsg::Failed(e.to_string())));
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

fn close_page(notebook: &gtk::Notebook, page: &gtk::Box) {
    if let Some(index) = notebook.page_num(page) {
        notebook.remove_page(Some(index));
    }
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

    let note = gtk::Label::new(Some(
        "Connecting arrives with the local terminal and SSH milestones.",
    ));
    note.set_xalign(0.0);
    note.set_margin_top(12);
    note.add_css_class("dim");
    page.append(&note);
}

fn menu_popover(entries: Vec<(&str, AppMsg)>, sender: &ComponentSender<App>) -> gtk::Popover {
    let popover = gtk::Popover::new();
    popover.set_has_arrow(false);
    popover.add_css_class("menu");
    let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    for (label, message) in entries {
        let button = gtk::Button::with_label(label);
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
    sidebar_button.set_tooltip_text(Some("Toggle sidebar (Ctrl+B)"));
    send_on_click(&sidebar_button, sender, AppMsg::ToggleSidebar);
    header.pack_start(&sidebar_button);

    let new_button = gtk::MenuButton::new();
    new_button.set_icon_name("list-add-symbolic");
    new_button.set_tooltip_text(Some("New"));
    new_button.set_popover(Some(&menu_popover(
        vec![
            (
                "New folder",
                AppMsg::New {
                    kind: NewKind::Folder,
                    parent: None,
                },
            ),
            (
                "New SSH host",
                AppMsg::New {
                    kind: NewKind::SshHost,
                    parent: None,
                },
            ),
        ],
        sender,
    )));
    header.pack_start(&new_button);

    let main_menu = gtk::MenuButton::new();
    main_menu.set_icon_name("open-menu-symbolic");
    main_menu.set_popover(Some(&menu_popover(
        vec![
            ("Command palette    Ctrl+P", AppMsg::ShowPalette),
            ("Reload config", AppMsg::ReloadConfig),
            ("Open config folder", AppMsg::OpenConfigFolder),
            ("Settings    Ctrl+,", AppMsg::ShowSettings),
        ],
        sender,
    )));
    header.pack_end(&main_menu);
    let palette_button = gtk::Button::from_icon_name("system-search-symbolic");
    palette_button.set_tooltip_text(Some("Command palette (Ctrl+P)"));
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
    // Delete only applies while the sidebar has focus, so it never eats a
    // keypress meant for a text field or, later, a terminal.
    let delete = gtk::EventControllerKey::new();
    delete.connect_key_pressed({
        let sender = sender.clone();
        move |_, key, _, _| {
            if key == gdk::Key::Delete {
                sender.input(AppMsg::DeleteSelected);
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        }
    });
    list.add_controller(delete);

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
        ("Ctrl+P", "Command palette"),
        ("Ctrl+B", "Toggle sidebar"),
        ("F2", "Rename the selected item"),
        ("Ctrl+W", "Close tab"),
    ] {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        let key = gtk::Label::new(Some(keys));
        key.add_css_class("kbd");
        key.set_width_chars(8);
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
    let controller = gtk::ShortcutController::new();
    controller.set_scope(gtk::ShortcutScope::Global);
    let shortcuts = [
        ("<Control>p", AppMsg::ShowPalette),
        ("<Control>k", AppMsg::ShowPalette),
        ("<Control>b", AppMsg::ToggleSidebar),
        ("<Control>w", AppMsg::CloseCurrentTab),
        ("<Control>comma", AppMsg::ShowSettings),
        ("F2", AppMsg::RenameSelected),
        ("<Control>r", AppMsg::ReloadConfig),
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
