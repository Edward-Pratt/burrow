//! SSH tunnel tabs, and the dialog that creates and edits tunnels.
//!
//! A tunnel's state comes from its session's events, so a tunnel started
//! by autostart or by another client shows up the same as one started
//! here.

use std::collections::HashMap;
use std::rc::Rc;

use burrow_core::{Node, NodeId, SessionId, SessionState};
use relm4::gtk;
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

/// Replaces `page`'s children with the tunnel's settings, state and
/// buttons.
pub fn fill(
    page: &gtk::Box,
    node: &Node,
    nodes: &HashMap<NodeId, Node>,
    status: &Status,
    autostart: bool,
    actions: &Actions,
) {
    // Redrawing destroys the focused widget. GTK would then focus the
    // first value and select its text, so focus goes to the first button.
    let had_focus = page.focus_child().is_some();
    while let Some(child) = page.first_child() {
        page.remove(&child);
    }

    let title = gtk::Label::new(Some(&node.name));
    title.set_xalign(0.0);
    title.add_css_class("page-title");
    page.append(&title);

    let grid = gtk::Grid::builder()
        .row_spacing(4)
        .column_spacing(16)
        .margin_top(8)
        .build();
    let (state, class) = status_text(status);
    let mut rows = describe(node, nodes);
    rows.push(("State", state));
    for (row, (label, value)) in rows.into_iter().enumerate() {
        let label = gtk::Label::new(Some(label));
        label.set_xalign(0.0);
        label.set_yalign(0.0);
        label.add_css_class("dim");
        let value = gtk::Label::new(Some(&value));
        value.set_xalign(0.0);
        value.set_wrap(true);
        value.set_selectable(true);
        if let Some(class) = class.filter(|_| row == 3) {
            value.add_css_class(class);
        }
        grid.attach(&label, 0, row as i32, 1, 1);
        grid.attach(&value, 1, row as i32, 1, 1);
    }
    page.append(&grid);

    if status.stale && status.running().is_some() {
        let note = gtk::Label::new(Some(
            "The settings changed while the tunnel was running. Restart it to use them.",
        ));
        note.set_xalign(0.0);
        note.set_wrap(true);
        note.add_css_class("warning-text");
        page.append(&note);
    }

    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_margin_top(12);
    let button = |label: &str, action: &Rc<dyn Fn()>| {
        let button = gtk::Button::with_label(label);
        let action = action.clone();
        button.connect_clicked(move |_| action());
        button
    };
    if status.running().is_some() {
        buttons.append(&button("Stop", &actions.stop));
        if status.stale {
            let restart = button("Restart", &actions.restart);
            restart.add_css_class("suggested-action");
            buttons.append(&restart);
        }
    } else {
        let start = button("Start", &actions.start);
        start.add_css_class("suggested-action");
        buttons.append(&start);
    }
    buttons.append(&button("Edit…", &actions.edit));
    page.append(&buttons);

    let check = gtk::CheckButton::with_label("Start with Burrow on this computer");
    check.set_margin_top(6);
    check.set_active(autostart);
    let set_autostart = actions.set_autostart.clone();
    check.connect_toggled(move |check| set_autostart(check.is_active()));
    page.append(&check);
    if had_focus {
        buttons.child_focus(gtk::DirectionType::TabForward);
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

    let submit = Rc::new({
        let (window, name, error) = (window.clone(), name.clone(), error.clone());
        move || {
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
    name.connect_activate(move |_| submit());
    cancel.connect_clicked({
        let window = window.clone();
        move |_| window.close()
    });
    dialogs::close_on_escape(&window);

    window.present();
    name.grab_focus();
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
