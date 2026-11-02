//! Building the file panel: the path bar, the toolbar, the list and the transfers below it.

use std::collections::HashMap;

use burrow_core::{FileEntry, FileKind, NodeId};
use relm4::gtk::glib;
use relm4::gtk::prelude::*;
use relm4::{ComponentSender, gtk};

use crate::app::{App, AppMsg};
use crate::session::{self, SessionPage};

use super::names::{mode, printable, size};
use super::{Files, Msg};

impl Files {
    pub fn new(node: NodeId, sender: &ComponentSender<App>) -> Files {
        let send = |msg: Msg| {
            let sender = sender.clone();
            move || {
                sender.input(AppMsg::Files {
                    node,
                    msg: msg.clone(),
                })
            }
        };
        let icon_button = |icon: &str, tooltip: &str, msg: Msg| {
            let button = gtk::Button::from_icon_name(icon);
            button.set_tooltip_text(Some(tooltip));
            let send = send(msg);
            button.connect_clicked(move |_| send());
            button
        };
        let button = |label: &str, msg: Msg| {
            let button = gtk::Button::with_label(label);
            let send = send(msg);
            button.connect_clicked(move |_| send());
            button
        };

        let page = gtk::Box::new(gtk::Orientation::Vertical, 8);
        page.add_css_class("files-view");

        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        bar.append(&icon_button("go-up-symbolic", "Up (Backspace)", Msg::Up));
        bar.append(&icon_button("go-home-symbolic", "Home folder", Msg::Home));
        bar.append(&icon_button(
            "view-refresh-symbolic",
            "Refresh",
            Msg::Refresh,
        ));
        let path_entry = gtk::Entry::new();
        path_entry.set_hexpand(true);
        path_entry.set_placeholder_text(Some("Folder"));
        path_entry.connect_activate({
            let send = send(Msg::GoTyped);
            move |_| send()
        });
        bar.append(&path_entry);
        page.append(&bar);

        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        actions.append(&button("New folder…", Msg::NewFolder));
        actions.append(&button("Upload files…", Msg::Upload { folder: false }));
        actions.append(&button("Upload folder…", Msg::Upload { folder: true }));
        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        actions.append(&spacer);
        let on_selection = vec![
            button("Download…", Msg::Download),
            button("Rename…", Msg::Rename),
            button("Delete…", Msg::Delete),
        ];
        for button in &on_selection {
            button.set_sensitive(false);
            actions.append(button);
        }
        page.append(&actions);

        let error = gtk::Label::new(None);
        error.set_xalign(0.0);
        error.set_wrap(true);
        error.set_selectable(true);
        error.add_css_class("error-text");
        error.set_visible(false);
        page.append(&error);

        let list = gtk::ListBox::new();
        list.add_css_class("file-list");
        list.set_selection_mode(gtk::SelectionMode::Single);
        // A click selects, so the buttons can act on it; a double click
        // opens.
        list.set_activate_on_single_click(false);
        list.connect_row_activated({
            let sender = sender.clone();
            move |_, row| {
                let msg = Msg::Activate(row.index());
                sender.input(AppMsg::Files { node, msg });
            }
        });
        list.connect_selected_rows_changed({
            let send = send(Msg::SelectionChanged);
            move |_| send()
        });
        // Like the sidebar, these keys only apply while the list has focus.
        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed({
            let sender = sender.clone();
            move |_, key, _, _| {
                let msg = match key {
                    gtk::gdk::Key::Delete => Msg::Delete,
                    gtk::gdk::Key::F2 => Msg::Rename,
                    gtk::gdk::Key::BackSpace => Msg::Up,
                    _ => return glib::Propagation::Proceed,
                };
                sender.input(AppMsg::Files { node, msg });
                glib::Propagation::Stop
            }
        });
        list.add_controller(keys);
        let more = gtk::Label::new(None);
        more.add_css_class("dim");
        let more_row = gtk::ListBoxRow::new();
        more_row.set_child(Some(&more));
        more_row.set_activatable(false);
        more_row.set_selectable(false);
        more_row.set_visible(false);
        list.append(&more_row);
        let scroller = gtk::ScrolledWindow::builder()
            .child(&list)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();

        let session = SessionPage::new(&scroller, send(Msg::Session(session::Msg::Reconnect)));
        page.append(session.widget());

        let transfers_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
        page.append(&transfers_box);

        Files {
            node,
            page,
            session,
            path: String::new(),
            entries: Vec::new(),
            path_entry,
            scroller,
            list,
            more_row,
            more,
            error,
            on_selection,
            transfers_box,
            transfers: HashMap::new(),
        }
    }
}

pub(super) fn row_content(entry: &FileEntry) -> gtk::Box {
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    let icon = match entry.kind {
        FileKind::Dir => "folder-symbolic",
        FileKind::Symlink => "emblem-symbolic-link-symbolic",
        FileKind::File | FileKind::Other => "text-x-generic-symbolic",
    };
    content.append(&gtk::Image::from_icon_name(icon));
    let name = gtk::Label::new(Some(&printable(&entry.name)));
    name.set_xalign(0.0);
    name.set_hexpand(true);
    name.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    content.append(&name);
    let column = |text: String, chars: i32| {
        let label = gtk::Label::new(Some(&text));
        label.set_xalign(1.0);
        label.set_width_chars(chars);
        label.add_css_class("dim");
        label
    };
    let size = match entry.kind {
        FileKind::File => size(entry.size),
        _ => String::new(),
    };
    content.append(&column(size, 10));
    content.append(&column(entry.permissions.map(mode).unwrap_or_default(), 10));
    let modified = entry
        .modified
        .and_then(|secs| glib::DateTime::from_unix_local(secs as i64).ok())
        .and_then(|time| time.format("%Y-%m-%d %H:%M").ok())
        .map(String::from)
        .unwrap_or_default();
    content.append(&column(modified, 16));
    content
}
