//! SSH tunnel tabs, and the dialog that creates and edits tunnels.
//!
//! A tunnel's state comes from its session's events, so a tunnel started
//! by autostart or by another client shows up the same as one started
//! here.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use burrow_core::{Node, NodeId, SessionId, SessionState};
use relm4::gtk;
use relm4::gtk::glib;
use relm4::gtk::prelude::*;

use crate::dialogs;

/// What a tunnel tab and its sidebar row show.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Status {
    /// The tunnel's latest session and its state. `None` until it has
    /// run.
    pub session: Option<(SessionId, SessionState)>,
    /// Started with settings that have changed since.
    pub stale: bool,
}

impl Status {
    /// The session to close to stop the tunnel, if it is running.
    pub fn running(&self) -> Option<SessionId> {
        match &self.session {
            Some((session, SessionState::Connecting { .. } | SessionState::Connected)) => {
                Some(*session)
            }
            _ => None,
        }
    }

    /// Connected and forwarding.
    pub fn is_up(&self) -> bool {
        matches!(self.session, Some((_, SessionState::Connected)))
    }
}

/// What the buttons of a tunnel tab do.
pub struct Actions {
    pub start: Rc<dyn Fn()>,
    pub stop: Rc<dyn Fn()>,
    pub restart: Rc<dyn Fn()>,
    pub edit: Rc<dyn Fn()>,
    pub set_autostart: Rc<dyn Fn(bool)>,
}

/// The settings a tunnel dialog edits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub via: NodeId,
    pub port: u16,
    /// `None` for a SOCKS proxy.
    pub target: Option<(String, u16)>,
}

impl Settings {
    /// Reads what it can from a tunnel's props. Missing or broken values
    /// are left to the dialog's defaults.
    fn from_props(props: &toml::Table) -> (Option<NodeId>, Option<u16>, Option<(String, u16)>) {
        let port = |key: &str| {
            props
                .get(key)
                .and_then(|v| v.as_integer())
                .and_then(|n| u16::try_from(n).ok())
                .filter(|&n| n != 0)
        };
        let via = props
            .get("via")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse().ok())
            .map(NodeId);
        let target = match props.get("type").and_then(|v| v.as_str()) {
            Some("dynamic") => None,
            _ => {
                let host = props.get("target_host").and_then(|v| v.as_str());
                Some((
                    host.unwrap_or_default().to_string(),
                    port("target_port").unwrap_or(0),
                ))
            }
        };
        (via, port("port"), target)
    }

    /// `base` with these settings written over it. Other keys, such as
    /// `bind`, are kept; a SOCKS proxy drops the target keys.
    pub fn apply(&self, base: &toml::Table) -> toml::Table {
        let mut props = base.clone();
        props.insert("via".into(), self.via.to_string().into());
        props.insert("port".into(), i64::from(self.port).into());
        match &self.target {
            Some((host, port)) => {
                props.insert("type".into(), "local".into());
                props.insert("target_host".into(), host.clone().into());
                props.insert("target_port".into(), i64::from(*port).into());
            }
            None => {
                props.insert("type".into(), "dynamic".into());
                props.remove("target_host");
                props.remove("target_port");
            }
        }
        props
    }
}

/// "localhost:5433 → db.internal:5432 through web", in parts.
fn describe(node: &Node, nodes: &HashMap<NodeId, Node>) -> Vec<(&'static str, String)> {
    let (via, port, target) = Settings::from_props(&node.props);
    let bind = node
        .props
        .get("bind")
        .and_then(|v| v.as_str())
        .unwrap_or("127.0.0.1");
    let listen = port.map_or_else(|| "not set".into(), |port| format!("{bind}:{port}"));
    let forward = match target {
        Some((host, port)) if !host.is_empty() && port != 0 => format!("{host}:{port}"),
        Some(_) => "not set".into(),
        None => "Anywhere, as a SOCKS5 proxy".into(),
    };
    let host = match via.map(|id| nodes.get(&id)) {
        Some(Some(host)) => host.name.clone(),
        Some(None) => "a host that no longer exists".into(),
        None => "not set".into(),
    };
    vec![
        ("Listens on", listen),
        ("Forwards to", forward),
        ("Through", host),
    ]
}

/// Words for the state of a tunnel.
pub fn status_text(status: &Status) -> (String, Option<&'static str>) {
    match &status.session {
        None | Some((_, SessionState::Closed { error: None })) => ("Stopped".into(), None),
        Some((_, SessionState::Connecting { step })) => (format!("{step}…"), None),
        Some((_, SessionState::Connected)) => ("Running".into(), Some("running")),
        Some((_, SessionState::Closed { error: Some(error) })) => {
            (format!("Stopped: {error}"), Some("error-text"))
        }
    }
}

/// Shows the tunnel's settings, state and buttons in `page`. The widgets
/// are built on the first call for a page and updated on later ones, so
/// focus and selected text survive a change of state.
pub fn fill(
    page: &gtk::Box,
    node: &Node,
    nodes: &HashMap<NodeId, Node>,
    status: &Status,
    autostart: bool,
    actions: &Actions,
) {
    let tab = PAGES.with_borrow(|pages| {
        pages
            .get(&node.id)
            .filter(|tab| tab.page.upgrade().as_ref() == Some(page))
            .cloned()
    });
    let tab = tab.unwrap_or_else(|| TunnelPage::build(page, node.id));
    tab.update(node, nodes, status, autostart, actions);
}

thread_local! {
    /// The widgets of each open tunnel tab. GTK runs on one thread, and
    /// `fill` is only given the page, so this is where it finds them again.
    static PAGES: RefCell<HashMap<NodeId, Rc<TunnelPage>>> = RefCell::default();
}

/// The widgets of one tunnel tab, and the actions its buttons run.
struct TunnelPage {
    page: glib::WeakRef<gtk::Box>,
    title: gtk::Label,
    /// The values next to "Listens on", "Forwards to", "Through" and
    /// "State".
    values: Vec<gtk::Label>,
    stale: gtk::Label,
    /// Start, or Stop while the tunnel runs. One button, so it keeps the
    /// focus when it is pressed from the keyboard.
    toggle: gtk::Button,
    restart: gtk::Button,
    autostart: gtk::CheckButton,
    /// Set while the checkbox is changed to match the core, so that isn't
    /// taken for the user's choice.
    quiet: Cell<bool>,
    running: Cell<bool>,
    actions: RefCell<Option<Actions>>,
}

impl TunnelPage {
    fn build(page: &gtk::Box, id: NodeId) -> Rc<TunnelPage> {
        while let Some(child) = page.first_child() {
            page.remove(&child);
        }

        let title = gtk::Label::new(None);
        title.set_xalign(0.0);
        title.add_css_class("page-title");
        page.append(&title);

        let grid = gtk::Grid::builder()
            .row_spacing(4)
            .column_spacing(16)
            .margin_top(8)
            .build();
        let mut values = Vec::new();
        for (row, name) in ["Listens on", "Forwards to", "Through", "State"]
            .into_iter()
            .enumerate()
        {
            let label = gtk::Label::new(Some(name));
            label.set_xalign(0.0);
            label.set_yalign(0.0);
            label.add_css_class("dim");
            let value = gtk::Label::new(None);
            value.set_xalign(0.0);
            value.set_wrap(true);
            value.set_selectable(true);
            grid.attach(&label, 0, row as i32, 1, 1);
            grid.attach(&value, 1, row as i32, 1, 1);
            values.push(value);
        }
        page.append(&grid);

        let stale = gtk::Label::new(Some(
            "The settings changed while the tunnel was running. Restart it to use them.",
        ));
        stale.set_xalign(0.0);
        stale.set_wrap(true);
        stale.add_css_class("warning-text");
        stale.set_visible(false);
        page.append(&stale);

        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        buttons.set_margin_top(12);
        let toggle = gtk::Button::new();
        let restart = gtk::Button::with_label("Restart");
        restart.add_css_class("suggested-action");
        let edit = gtk::Button::with_label("Edit…");
        buttons.append(&toggle);
        buttons.append(&restart);
        buttons.append(&edit);
        page.append(&buttons);

        let autostart = gtk::CheckButton::with_label("Start with Burrow on this computer");
        autostart.set_margin_top(6);
        page.append(&autostart);

        let tab = Rc::new(TunnelPage {
            page: page.downgrade(),
            title,
            values,
            stale,
            toggle: toggle.clone(),
            restart: restart.clone(),
            autostart: autostart.clone(),
            quiet: Cell::new(false),
            running: Cell::new(false),
            actions: RefCell::new(None),
        });

        // The handlers hold the page weakly: it holds them through its
        // buttons.
        let weak = Rc::downgrade(&tab);
        let run = move |pick: fn(&TunnelPage, &Actions) -> Rc<dyn Fn()>| {
            let weak = weak.clone();
            move |_: &gtk::Button| {
                let Some(tab) = weak.upgrade() else { return };
                // Taken out of the borrow first, as the action may draw
                // the page again.
                let action = tab.actions.borrow().as_ref().map(|a| pick(&tab, a));
                if let Some(action) = action {
                    action();
                }
            }
        };
        toggle.connect_clicked(run(|tab, actions| {
            if tab.running.get() {
                actions.stop.clone()
            } else {
                actions.start.clone()
            }
        }));
        restart.connect_clicked(run(|_, actions| actions.restart.clone()));
        edit.connect_clicked(run(|_, actions| actions.edit.clone()));
        autostart.connect_toggled({
            let weak = Rc::downgrade(&tab);
            move |check| {
                let Some(tab) = weak.upgrade() else { return };
                if tab.quiet.get() {
                    return;
                }
                let action = tab
                    .actions
                    .borrow()
                    .as_ref()
                    .map(|a| a.set_autostart.clone());
                if let Some(action) = action {
                    action(check.is_active());
                }
            }
        });

        page.connect_destroy(move |page| {
            PAGES.with_borrow_mut(|pages| {
                let ours = pages
                    .get(&id)
                    .is_some_and(|tab| tab.page.upgrade().is_none_or(|shown| shown == *page));
                if ours {
                    pages.remove(&id);
                }
            });
        });
        PAGES.with_borrow_mut(|pages| pages.insert(id, tab.clone()));
        tab
    }

    fn update(
        &self,
        node: &Node,
        nodes: &HashMap<NodeId, Node>,
        status: &Status,
        autostart: bool,
        actions: &Actions,
    ) {
        *self.actions.borrow_mut() = Some(Actions {
            start: actions.start.clone(),
            stop: actions.stop.clone(),
            restart: actions.restart.clone(),
            edit: actions.edit.clone(),
            set_autostart: actions.set_autostart.clone(),
        });

        set_text(&self.title, &node.name);
        let (state, class) = status_text(status);
        let mut texts: Vec<String> = describe(node, nodes).into_iter().map(|(_, v)| v).collect();
        texts.push(state);
        for (value, text) in self.values.iter().zip(&texts) {
            set_text(value, text);
        }
        let state = &self.values[3];
        for old in ["running", "error-text"] {
            if Some(old) != class {
                state.remove_css_class(old);
            }
        }
        if let Some(class) = class {
            state.add_css_class(class);
        }

        let running = status.running().is_some();
        self.running.set(running);
        self.stale.set_visible(status.stale && running);
        self.toggle
            .set_label(if running { "Stop" } else { "Start" });
        if running {
            self.toggle.remove_css_class("suggested-action");
        } else {
            self.toggle.add_css_class("suggested-action");
        }
        let restart = running && status.stale;
        if !restart && self.restart.has_focus() {
            self.toggle.grab_focus();
        }
        self.restart.set_visible(restart);

        if self.autostart.is_active() != autostart {
            self.quiet.set(true);
            self.autostart.set_active(autostart);
            self.quiet.set(false);
        }
    }
}

/// Sets a label's text only when it changed, as setting it clears the
/// selection in it.
fn set_text(label: &gtk::Label, text: &str) {
    if label.label() != text {
        label.set_label(text);
    }
}

/// Asks for a tunnel's name and settings, starting from `node` when
/// editing one. `hosts` are the SSH hosts it can go through. `done` gets
/// the name and the new props.
pub fn edit(
    parent: &impl IsA<gtk::Window>,
    node: Option<&Node>,
    hosts: Vec<(NodeId, String)>,
    done: impl Fn(String, toml::Table) + 'static,
) {
    let window = gtk::Window::builder()
        .title(if node.is_some() {
            "Edit SSH tunnel"
        } else {
            "New SSH tunnel"
        })
        .modal(true)
        .transient_for(parent)
        .resizable(false)
        .default_width(420)
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_margin_top(18);
    content.set_margin_bottom(18);
    content.set_margin_start(18);
    content.set_margin_end(18);

    let base = node.map(|n| n.props.clone()).unwrap_or_default();
    let (via, port, target) = Settings::from_props(&base);

    let grid = gtk::Grid::builder()
        .row_spacing(8)
        .column_spacing(12)
        .build();
    let label = |text: &str| {
        let label = gtk::Label::new(Some(text));
        label.set_xalign(0.0);
        label
    };

    let name = gtk::Entry::new();
    name.set_hexpand(true);
    name.set_text(node.map_or("", |n| n.name.as_str()));
    grid.attach(&label("Name"), 0, 0, 1, 1);
    grid.attach(&name, 1, 0, 1, 1);

    let names: Vec<&str> = hosts.iter().map(|(_, name)| name.as_str()).collect();
    let host = gtk::DropDown::from_strings(&names);
    if let Some(index) = via.and_then(|via| hosts.iter().position(|(id, _)| *id == via)) {
        host.set_selected(index as u32);
    }
    grid.attach(&label("Through"), 0, 1, 1, 1);
    grid.attach(&host, 1, 1, 1, 1);

    let kind = gtk::DropDown::from_strings(&["Forward a port", "SOCKS proxy"]);
    kind.set_selected(if target.is_some() { 0 } else { 1 });
    grid.attach(&label("Type"), 0, 2, 1, 1);
    grid.attach(&kind, 1, 2, 1, 1);

    let port_entry = port_button(port.unwrap_or(1080));
    grid.attach(&label("Local port"), 0, 3, 1, 1);
    grid.attach(&port_entry, 1, 3, 1, 1);

    let (target_host, target_port) = target.unwrap_or_default();
    let target_entry = gtk::Entry::new();
    target_entry.set_text(&target_host);
    target_entry.set_placeholder_text(Some("As the SSH host sees it, such as localhost"));
    let target_label = label("Destination");
    grid.attach(&target_label, 0, 4, 1, 1);
    grid.attach(&target_entry, 1, 4, 1, 1);
    let target_port_entry = port_button(if target_port == 0 { 80 } else { target_port });
    let target_port_label = label("Destination port");
    grid.attach(&target_port_label, 0, 5, 1, 1);
    grid.attach(&target_port_entry, 1, 5, 1, 1);
    content.append(&grid);

    let show_target = {
        let widgets: Vec<gtk::Widget> = vec![
            target_label.upcast(),
            target_entry.clone().upcast(),
            target_port_label.upcast(),
            target_port_entry.clone().upcast(),
        ];
        move |kind: &gtk::DropDown| {
            for widget in &widgets {
                widget.set_visible(kind.selected() == 0);
            }
        }
    };
    show_target(&kind);
    kind.connect_selected_notify(show_target);

    // A port forward's local port follows the destination port until the
    // user picks one, and a proxy's is 1080. Editing a tunnel keeps the
    // port it has.
    let local_picked = Rc::new(Cell::new(port.is_some()));
    let following = Rc::new(Cell::new(false));
    let follow = Rc::new(glib::clone!(
        #[weak]
        kind,
        #[weak]
        port_entry,
        #[weak]
        target_port_entry,
        #[strong]
        local_picked,
        #[strong]
        following,
        move || {
            if local_picked.get() {
                return;
            }
            let local = if kind.selected() == 0 {
                local_port_for(target_port_entry.value() as u16)
            } else {
                1080
            };
            following.set(true);
            port_entry.set_value(f64::from(local));
            following.set(false);
        }
    ));
    port_entry.connect_value_changed(move |_| {
        if !following.get() {
            local_picked.set(true);
        }
    });
    target_port_entry.connect_value_changed({
        let follow = follow.clone();
        move |_| follow()
    });
    kind.connect_selected_notify(move |_| follow());

    let note = gtk::Label::new(Some(
        "The tunnel listens on this computer only. Programs connect to the local port, \
         and the SSH host makes the onward connection.",
    ));
    note.set_xalign(0.0);
    note.set_wrap(true);
    note.set_max_width_chars(52);
    note.add_css_class("dim");
    content.append(&note);

    let error = gtk::Label::new(None);
    error.set_xalign(0.0);
    error.set_wrap(true);
    error.add_css_class("error-text");
    error.set_visible(false);
    content.append(&error);

    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::End);
    buttons.set_margin_top(6);
    let cancel = gtk::Button::with_label("Cancel");
    let ok = gtk::Button::with_label(if node.is_some() { "Save" } else { "Create" });
    ok.add_css_class("suggested-action");
    buttons.append(&cancel);
    buttons.append(&ok);
    content.append(&buttons);
    window.set_child(Some(&content));

    if hosts.is_empty() {
        error.set_label("Add an SSH host first: a tunnel goes through one.");
        error.set_visible(true);
        ok.set_sensitive(false);
    }

    // The window is held weakly by everything inside it, or it would keep
    // itself alive after closing.
    let submit = Rc::new({
        let (window, name, error) = (window.downgrade(), name.clone(), error.clone());
        let (target_entry, port_entry, target_port_entry) = (
            target_entry.clone(),
            port_entry.clone(),
            target_port_entry.clone(),
        );
        move || {
            let Some(window) = window.upgrade() else {
                return;
            };
            let fail = |message: &str| {
                error.set_label(message);
                error.set_visible(true);
            };
            // Commit text typed into the ports but not yet taken.
            port_entry.update();
            target_port_entry.update();
            let title = name.text().trim().to_string();
            if title.is_empty() {
                return fail("Give the tunnel a name.");
            }
            let Some(&(via, _)) = hosts.get(host.selected() as usize) else {
                return fail("Pick the SSH host to go through.");
            };
            let target = if kind.selected() == 0 {
                let host = target_entry.text().trim().to_string();
                if host.is_empty() {
                    return fail("Say where to forward to, such as localhost.");
                }
                Some((host, target_port_entry.value() as u16))
            } else {
                None
            };
            let settings = Settings {
                via,
                port: port_entry.value() as u16,
                target,
            };
            window.close();
            done(title, settings.apply(&base));
        }
    });
    ok.connect_clicked({
        let submit = submit.clone();
        move |_| submit()
    });
    for entry in [&name, &target_entry] {
        let submit = submit.clone();
        entry.connect_activate(move |_| submit());
    }
    for entry in [&port_entry, &target_port_entry] {
        let submit = submit.clone();
        entry.connect_activate(move |_| submit());
    }
    dialogs::close_on_cancel(&window, &cancel);
    dialogs::close_on_escape(&window);

    window.present();
    name.grab_focus();
}

/// The local port to suggest for forwarding `target`: the same port, so
/// programs find the service where they expect it, except below 1024,
/// where only root may listen, so 80 becomes 8080.
fn local_port_for(target: u16) -> u16 {
    if target < 1024 { target + 8000 } else { target }
}

fn port_button(value: u16) -> gtk::SpinButton {
    let button = gtk::SpinButton::with_range(1.0, 65535.0, 1.0);
    button.set_value(f64::from(value));
    button.set_numeric(true);
    button
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_keep_other_props_and_drop_old_targets() {
        let via = NodeId::new();
        let mut base = toml::Table::new();
        base.insert("bind".into(), "::1".into());
        base.insert("target_host".into(), "db".into());
        base.insert("target_port".into(), 5432.into());
        let socks = Settings {
            via,
            port: 1080,
            target: None,
        };
        let props = socks.apply(&base);
        assert_eq!(props["bind"].as_str(), Some("::1"));
        assert_eq!(props["type"].as_str(), Some("dynamic"));
        assert_eq!(props["port"].as_integer(), Some(1080));
        assert_eq!(props["via"].as_str(), Some(via.to_string().as_str()));
        assert!(!props.contains_key("target_host"));
        assert!(!props.contains_key("target_port"));
    }

    #[test]
    fn settings_read_back_what_they_wrote() {
        let via = NodeId::new();
        let settings = Settings {
            via,
            port: 5433,
            target: Some(("db.internal".into(), 5432)),
        };
        let props = settings.apply(&toml::Table::new());
        assert_eq!(
            Settings::from_props(&props),
            (Some(via), Some(5433), Some(("db.internal".into(), 5432)))
        );
    }

    #[test]
    fn forwards_suggest_the_destination_port_where_one_may_listen() {
        assert_eq!(local_port_for(5432), 5432);
        assert_eq!(local_port_for(1024), 1024);
        assert_eq!(local_port_for(80), 8080);
        assert_eq!(local_port_for(443), 8443);
    }

    #[test]
    fn running_means_connecting_or_connected() {
        let mut status = Status::default();
        assert_eq!(status.running(), None);
        status.session = Some((
            SessionId(1),
            SessionState::Connecting {
                step: "Listening".into(),
            },
        ));
        assert_eq!(status.running(), Some(SessionId(1)));
        status.session = Some((SessionId(1), SessionState::Closed { error: None }));
        assert_eq!(status.running(), None);
    }
}
