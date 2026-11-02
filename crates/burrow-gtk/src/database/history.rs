//! The query history list.

use std::time::{SystemTime, UNIX_EPOCH};

use burrow_core::HistoryEntry;
use relm4::gtk::glib;
use relm4::gtk::prelude::*;
use relm4::{ComponentSender, gtk};

use crate::app::{App, AppMsg};
use crate::files::printable;

use super::{Database, Msg};

impl Database {
    pub(super) fn show_history(&self, entries: Vec<HistoryEntry>, sender: &ComponentSender<App>) {
        let node = self.node;
        let popover = gtk::Popover::new();
        popover.set_parent(&self.history_button);
        popover.connect_closed(|popover| {
            let popover = popover.clone();
            glib::idle_add_local_once(move || popover.unparent());
        });
        let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
        if entries.is_empty() {
            let empty = gtk::Label::new(Some("No queries yet."));
            empty.add_css_class("dim");
            content.append(&empty);
        } else {
            let list = gtk::ListBox::new();
            list.add_css_class("history-list");
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let newest_first: Vec<HistoryEntry> = entries.into_iter().rev().collect();
            for entry in &newest_first {
                let line = gtk::Box::new(gtk::Orientation::Horizontal, 10);
                let sql = gtk::Label::new(Some(&one_line(&entry.sql)));
                sql.set_xalign(0.0);
                sql.set_hexpand(true);
                sql.set_ellipsize(gtk::pango::EllipsizeMode::End);
                // An ellipsized label asks for no width of its own.
                sql.set_width_chars(40);
                sql.set_max_width_chars(60);
                sql.add_css_class("monospace");
                let when = gtk::Label::new(Some(&ago(now.saturating_sub(entry.at))));
                when.add_css_class("dim");
                line.append(&sql);
                if !entry.ok {
                    let failed = gtk::Label::new(Some("failed"));
                    failed.add_css_class("error-text");
                    line.append(&failed);
                }
                line.append(&when);
                list.append(&line);
            }
            // Weak: the list is inside the popover.
            list.connect_row_activated(glib::clone!(
                #[weak]
                popover,
                #[strong]
                sender,
                move |_, row| {
                    if let Some(entry) = newest_first.get(row.index() as usize) {
                        let msg = Msg::UseQuery(entry.sql.clone());
                        sender.input(AppMsg::Database { node, msg });
                    }
                    popover.popdown();
                }
            ));
            let scroller = gtk::ScrolledWindow::builder()
                .child(&list)
                .hscrollbar_policy(gtk::PolicyType::Never)
                .propagate_natural_height(true)
                .max_content_height(360)
                .min_content_width(420)
                .build();
            content.append(&scroller);
            let clear = gtk::Button::with_label("Clear history");
            clear.set_halign(gtk::Align::End);
            clear.connect_clicked(glib::clone!(
                #[weak]
                popover,
                #[strong]
                sender,
                move |_| {
                    sender.input(AppMsg::Database {
                        node,
                        msg: Msg::ClearHistory,
                    });
                    popover.popdown();
                }
            ));
            content.append(&clear);
        }
        popover.set_child(Some(&content));
        popover.popup();
    }
}

/// A query on one line, for the history list.
pub(super) fn one_line(sql: &str) -> String {
    let words: Vec<&str> = sql.split_whitespace().collect();
    printable(&words.join(" "))
}

pub(super) fn ago(seconds: u64) -> String {
    match seconds {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", seconds / 60),
        3600..86400 => format!("{} h ago", seconds / 3600),
        _ => format!("{} days ago", seconds / 86400),
    }
}
