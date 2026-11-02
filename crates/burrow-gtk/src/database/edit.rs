//! The dialog that creates or edits a database node.

use std::rc::Rc;

use burrow_core::{Node, NodeId};
use relm4::gtk;
use relm4::gtk::prelude::*;

use crate::dialogs;

use super::settings::{SSLMODES, Settings};

/// Asks for a database's name and settings, starting from `node` when
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
            "Edit database"
        } else {
            "New PostgreSQL database"
        })
        .modal(true)
        .transient_for(parent)
        .resizable(false)
        .default_width(460)
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_margin_top(18);
    content.set_margin_bottom(18);
    content.set_margin_start(18);
    content.set_margin_end(18);

    let base = node.map(|n| n.props.clone()).unwrap_or_default();
    let settings = Settings::from_props(&base);

    let grid = gtk::Grid::builder()
        .row_spacing(8)
        .column_spacing(12)
        .build();
    let label = |text: &str| {
        let label = gtk::Label::new(Some(text));
        label.set_xalign(0.0);
        label
    };
    let entry = |text: &str, placeholder: &str| {
        let entry = gtk::Entry::new();
        entry.set_hexpand(true);
        entry.set_text(text);
        entry.set_placeholder_text(Some(placeholder));
        entry
    };
    let mut row = 0;
    let mut attach = |name: &gtk::Label, widget: &gtk::Widget| {
        grid.attach(name, 0, row, 1, 1);
        grid.attach(widget, 1, row, 1, 1);
        row += 1;
    };

    let name = entry(node.map_or("", |n| n.name.as_str()), "");
    attach(&label("Name"), name.upcast_ref());

    let mut through: Vec<String> = vec!["This computer".into()];
    through.extend(hosts.iter().map(|(_, name)| name.clone()));
    let through: Vec<&str> = through.iter().map(String::as_str).collect();
    let host_choice = gtk::DropDown::from_strings(&through);
    if let Some(index) = settings
        .via
        .and_then(|via| hosts.iter().position(|(id, _)| *id == via))
    {
        host_choice.set_selected(index as u32 + 1);
    }
    attach(&label("Through"), host_choice.upcast_ref());

    let route = gtk::DropDown::from_strings(&["Network", "Socket"]);
    route.set_selected(if settings.host.is_some() || node.is_none() {
        0
    } else {
        1
    });
    attach(&label("Connect by"), route.upcast_ref());

    let host_label = label("Host");
    let host = entry(
        settings.host.as_deref().unwrap_or_default(),
        "As the host above sees it, such as localhost",
    );
    attach(&host_label, host.upcast_ref());
    let socket_label = label("Socket folder");
    let socket = entry(
        settings.socket.as_deref().unwrap_or_default(),
        "/run/postgresql",
    );
    attach(&socket_label, socket.upcast_ref());

    let port = gtk::SpinButton::with_range(1.0, 65535.0, 1.0);
    port.set_value(f64::from(settings.port));
    port.set_numeric(true);
    attach(&label("Port"), port.upcast_ref());

    let user_default = std::env::var("USER").unwrap_or_default();
    let user = entry(
        if settings.user.is_empty() {
            &user_default
        } else {
            &settings.user
        },
        "",
    );
    attach(&label("User"), user.upcast_ref());
    let database = entry(
        settings.database.as_deref().unwrap_or_default(),
        "Same as the user",
    );
    attach(&label("Database"), database.upcast_ref());

    let tls_label = label("TLS");
    let names: Vec<&str> = SSLMODES.iter().map(|(_, name)| *name).collect();
    let tls = gtk::DropDown::from_strings(&names);
    if let Some(index) = SSLMODES
        .iter()
        .position(|(mode, _)| *mode == settings.sslmode.as_deref())
    {
        tls.set_selected(index as u32);
    }
    attach(&tls_label, tls.upcast_ref());
    let root_label = label("Root certificate");
    let root = entry(
        settings.sslrootcert.as_deref().unwrap_or_default(),
        "The system's trusted CAs",
    );
    attach(&root_label, root.upcast_ref());
    content.append(&grid);

    let note = gtk::Label::new(None);
    note.set_xalign(0.0);
    note.set_wrap(true);
    note.set_max_width_chars(56);
    note.add_css_class("dim");
    content.append(&note);

    let show_route = {
        let network: Vec<gtk::Widget> = vec![
            host_label.upcast(),
            host.clone().upcast(),
            tls_label.upcast(),
            tls.clone().upcast(),
            root_label.upcast(),
            root.clone().upcast(),
        ];
        let local: Vec<gtk::Widget> = vec![socket_label.upcast(), socket.clone().upcast()];
        let (host_choice, note) = (host_choice.clone(), note.clone());
        move |route: &gtk::DropDown| {
            let by_network = route.selected() == 0;
            for widget in &network {
                widget.set_visible(by_network);
            }
            for widget in &local {
                widget.set_visible(!by_network);
            }
            let through_host = host_choice.selected() > 0;
            note.set_label(match (through_host, by_network) {
                (true, _) => {
                    "Burrow opens a channel on the SSH host's connection, so nothing \
                     listens on this computer. TLS is off by default: SSH already \
                     encrypts it. Through a socket, you sign in as the SSH user."
                }
                (false, true) => {
                    "By default, the server's certificate must be valid for its \
                     name. New sessions are read-only until you allow changes."
                }
                (false, false) => {
                    "A socket on this computer. New sessions are read-only until \
                     you allow changes."
                }
            });
        }
    };
    show_route(&route);
    route.connect_selected_notify(show_route.clone());
    host_choice.connect_selected_notify({
        let route = route.clone();
        move |_| show_route(&route)
    });

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

    // The window is held weakly by everything inside it, or it would keep
    // itself alive after closing.
    let submit = Rc::new({
        let (window, name, error) = (window.downgrade(), name.clone(), error.clone());
        let (host, socket, user, database, root, port) = (
            host.clone(),
            socket.clone(),
            user.clone(),
            database.clone(),
            root.clone(),
            port.clone(),
        );
        move || {
            let Some(window) = window.upgrade() else {
                return;
            };
            let fail = |message: &str| {
                error.set_label(message);
                error.set_visible(true);
            };
            port.update();
            let title = name.text().trim().to_string();
            if title.is_empty() {
                return fail("Give the database a name.");
            }
            let text = |entry: &gtk::Entry| {
                Some(entry.text().trim().to_string()).filter(|s| !s.is_empty())
            };
            let by_network = route.selected() == 0;
            let host = if by_network {
                match text(&host) {
                    Some(host) => Some(host),
                    None => return fail("Say where the server is, such as localhost."),
                }
            } else {
                None
            };
            let Some(user) = text(&user) else {
                return fail("Say which user to sign in as.");
            };
            let (sslmode, sslrootcert) = if by_network {
                let mode = SSLMODES
                    .get(tls.selected() as usize)
                    .and_then(|(mode, _)| *mode);
                let root = text(&root);
                if root.as_deref().is_some_and(|p| !p.starts_with('/')) {
                    return fail("Give the root certificate's full path.");
                }
                if mode == Some("verify-ca") && root.is_none() {
                    return fail("Checking the CA only needs the root certificate to trust.");
                }
                (mode.map(str::to_string), root)
            } else {
                (None, None)
            };
            let via = (host_choice.selected() as usize)
                .checked_sub(1)
                .and_then(|index| hosts.get(index))
                .map(|(id, _)| *id);
            let settings = Settings {
                via,
                host,
                socket: if by_network { None } else { text(&socket) },
                port: port.value() as u16,
                user,
                database: text(&database),
                sslmode,
                sslrootcert,
            };
            window.close();
            done(title, settings.apply(&base));
        }
    });
    ok.connect_clicked({
        let submit = submit.clone();
        move |_| submit()
    });
    for entry in [&name, &host, &socket, &user, &database, &root] {
        let submit = submit.clone();
        entry.connect_activate(move |_| submit());
    }
    port.connect_activate(move |_| submit());
    dialogs::close_on_cancel(&window, &cancel);
    dialogs::close_on_escape(&window);

    window.present();
    name.grab_focus();
}
