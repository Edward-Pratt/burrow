//! The dialog that adds or edits a VNC desktop.

use std::rc::Rc;

use burrow_core::{Node, NodeId};
use relm4::gtk;
use relm4::gtk::prelude::*;

/// The desktop dialog: new when `node` is `None`. `hosts` are the SSH hosts
/// a desktop can be reached through. `done` gets the name and props.
pub fn edit(
    parent: &impl IsA<gtk::Window>,
    node: Option<&Node>,
    hosts: Vec<(NodeId, String)>,
    done: impl Fn(String, toml::Table) + 'static,
) {
    let window = gtk::Window::builder()
        .title(if node.is_some() {
            "Edit desktop"
        } else {
            "New VNC desktop"
        })
        .modal(true)
        .transient_for(parent)
        .resizable(false)
        .default_width(440)
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_margin_top(18);
    content.set_margin_bottom(18);
    content.set_margin_start(18);
    content.set_margin_end(18);
    let props = node.map(|n| n.props.clone()).unwrap_or_default();
    let text = |key: &str| props.get(key).and_then(|v| v.as_str()).map(str::to_string);

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
    let through = gtk::DropDown::from_strings(&names);
    let via = text("via").and_then(|via| via.parse().ok()).map(NodeId);
    if let Some(index) = hosts.iter().position(|(id, _)| Some(*id) == via) {
        through.set_selected(index as u32);
    }
    grid.attach(&label("Through"), 0, 1, 1, 1);
    grid.attach(&through, 1, 1, 1, 1);

    let host = gtk::Entry::new();
    host.set_text(&text("host").unwrap_or_default());
    host.set_placeholder_text(Some("localhost, as the SSH host sees it"));
    grid.attach(&label("Host"), 0, 2, 1, 1);
    grid.attach(&host, 1, 2, 1, 1);

    let port = gtk::SpinButton::with_range(1.0, 65535.0, 1.0);
    let saved_port = props.get("port").and_then(|p| p.as_integer());
    port.set_value(saved_port.unwrap_or(5900) as f64);
    port.set_numeric(true);
    grid.attach(&label("Port"), 0, 3, 1, 1);
    grid.attach(&port, 1, 3, 1, 1);
    content.append(&grid);

    let note = gtk::Label::new(Some(
        "Burrow reaches the desktop over the SSH host's connection, so the screen \
         never crosses the network in the clear and nothing listens on this \
         computer. It asks for the VNC password when the desktop wants one.",
    ));
    note.set_xalign(0.0);
    note.set_wrap(true);
    note.set_max_width_chars(54);
    note.add_css_class("dim");
    content.append(&note);

    let error = gtk::Label::new(None);
    error.set_xalign(0.0);
    error.set_wrap(true);
    error.add_css_class("error-text");
    error.set_visible(hosts.is_empty());
    error.set_label("Add an SSH host first: a desktop is reached through one.");
    content.append(&error);

    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::End);
    buttons.set_margin_top(6);
    let cancel = gtk::Button::with_label("Cancel");
    let ok = gtk::Button::with_label(if node.is_some() { "Save" } else { "Create" });
    ok.add_css_class("suggested-action");
    ok.set_sensitive(!hosts.is_empty());
    buttons.append(&cancel);
    buttons.append(&ok);
    content.append(&buttons);
    window.set_child(Some(&content));

    // The window is held weakly by everything inside it, or it would keep
    // itself alive after closing.
    let submit = Rc::new({
        let window = window.downgrade();
        let (name, host, port, error) = (name.clone(), host.clone(), port.clone(), error.clone());
        move || {
            let Some(window) = window.upgrade() else {
                return;
            };
            port.update();
            let title = name.text().trim().to_string();
            if title.is_empty() {
                error.set_label("Give the desktop a name.");
                error.set_visible(true);
                return;
            }
            let Some((via, _)) = hosts.get(through.selected() as usize) else {
                return;
            };
            let mut props = props.clone();
            props.insert("via".into(), via.0.to_string().into());
            let host = host.text().trim().to_string();
            if host.is_empty() {
                props.remove("host");
            } else {
                props.insert("host".into(), host.into());
            }
            props.insert("port".into(), (port.value() as i64).into());
            done(title, props);
            window.close();
        }
    });
    ok.connect_clicked({
        let submit = submit.clone();
        move |_| submit()
    });
    for entry in [&name, &host] {
        let submit = submit.clone();
        entry.connect_activate(move |_| submit());
    }
    let weak = window.downgrade();
    cancel.connect_clicked(move |_| {
        if let Some(window) = weak.upgrade() {
            window.close();
        }
    });
    crate::dialogs::close_on_escape(&window);
    window.present();
}
