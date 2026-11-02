//! Opening a node's tab, a file panel or a local terminal.

use std::collections::HashMap;
use std::path::Path;

use burrow_core::{Node, NodeId, POSTGRES_KIND, SSH_HOST_KIND, SSH_TUNNEL_KIND, VNC_KIND};
use relm4::gtk::prelude::*;
use relm4::{ComponentSender, gtk};

use crate::addons;
use crate::database::{self, Database};
use crate::desktop::{self, Desktop};
use crate::files::{self, Files};
use crate::sidebar;
use crate::terminal;
use crate::tunnels;

use super::message::AppMsg;
use super::tabs::{Panel, TabKey, files_title};
use super::{App, Widgets};

impl App {
    pub(super) fn activate(
        &mut self,
        id: NodeId,
        widgets: &Widgets,
        sender: &ComponentSender<Self>,
    ) {
        let Some(node) = self.nodes.get(&id) else {
            return;
        };
        if node.is_folder() {
            widgets.sidebar.toggle(id);
        } else {
            self.open_tab_now(id, widgets, sender);
        }
    }

    pub(super) fn open_tab(
        &mut self,
        id: NodeId,
        widgets: &Widgets,
        sender: &ComponentSender<Self>,
    ) {
        // Reveal the node in the sidebar, then open it.
        widgets.sidebar.reveal(id);
        if self.nodes.get(&id).is_some_and(|n| !n.is_folder()) {
            self.open_tab_now(id, widgets, sender);
        }
    }

    pub(super) fn open_tab_now(
        &mut self,
        id: NodeId,
        widgets: &Widgets,
        sender: &ComponentSender<Self>,
    ) {
        let key = TabKey::Node(id);
        if self.show_tab(key, widgets) {
            return;
        }
        let Some(node) = self.nodes.get(&id) else {
            return;
        };
        if node.kind == SSH_HOST_KIND {
            // The shell opened first, not a tab following a log.
            let shell = (self.tabs.iter())
                .filter_map(|(key, tab)| match (key, &tab.panel) {
                    (TabKey::Shell(node, n), Panel::Shell(remote))
                        if *node == id && remote.run.is_none() =>
                    {
                        Some(*n)
                    }
                    _ => None,
                })
                .min();
            if !shell.is_some_and(|n| self.show_tab(TabKey::Shell(id, n), widgets)) {
                self.new_shell(id, None, widgets, sender);
            }
            return;
        }
        let icon = sidebar::icon_name(node, false, &self.icons()).to_string();
        let (icon, name) = (icon.as_str(), node.name.clone());
        if node.kind == POSTGRES_KIND {
            let panel = Database::new(id, sender);
            let page = panel.page.clone().upcast();
            database::start(&self.core, id, sender);
            let panel = Panel::Database(panel);
            self.add_tab(key, page, panel, icon, &name, widgets, sender);
            return;
        }
        if node.kind == VNC_KIND {
            let panel = Desktop::new(id, sender);
            let page = panel.page.clone().upcast();
            desktop::start(&self.core, id, sender);
            let panel = Panel::Desktop(Box::new(panel));
            self.add_tab(key, page, panel, icon, &name, widgets, sender);
            return;
        }
        if node.kind == SSH_TUNNEL_KIND {
            let tunnel = tunnels::TunnelPage::new();
            let page = tunnel.page.clone().upcast();
            let panel = Panel::Tunnel(tunnel);
            self.add_tab(key, page, panel, icon, &name, widgets, sender);
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
            let panel = Panel::View(page);
            self.add_tab(key, scroller.upcast(), panel, icon, &name, widgets, sender);
            self.load_view(id, sender);
            return;
        }
        let page = gtk::Box::new(gtk::Orientation::Vertical, 8);
        fill_page(&page, node, &self.kind_label(&node.kind), &self.nodes);
        let panel = Panel::Placeholder(page.clone());
        self.add_tab(key, page.upcast(), panel, icon, &name, widgets, sender);
    }

    pub(super) fn open_files(
        &mut self,
        id: NodeId,
        widgets: &Widgets,
        sender: &ComponentSender<Self>,
    ) {
        let key = TabKey::Files(id);
        if self.show_tab(key, widgets) {
            return;
        }
        let Some(node) = self.nodes.get(&id).filter(|n| n.kind == SSH_HOST_KIND) else {
            return;
        };
        let title = files_title(&node.name);
        let panel = Files::new(id, sender);
        let page = panel.page.clone().upcast();
        files::start(&self.core, id, sender);
        let (panel, icon) = (Panel::Files(panel), "folder-remote-symbolic");
        self.add_tab(key, page, panel, icon, &title, widgets, sender);
    }

    pub(super) fn open_terminal(
        &mut self,
        shell: Option<String>,
        folder: Option<&Path>,
        widgets: &Widgets,
        sender: &ComponentSender<Self>,
    ) {
        let id = self.next_tab;
        self.next_tab += 1;
        let shell = shell.unwrap_or_else(terminal::default_shell);
        let term = terminal::new(
            &shell,
            folder,
            {
                let sender = sender.clone();
                move |title| sender.input(AppMsg::TerminalTitle { id, title })
            },
            {
                // Closing the window ends the shell after the app has
                // stopped taking messages, which `input` panics on.
                let input = sender.input_sender().clone();
                move || {
                    let _ = input.send(AppMsg::TerminalExited(id));
                }
            },
        );
        terminal::set_font(&term, self.settings.terminal_font.as_deref());
        let page = terminal::page(&term);
        let key = TabKey::Terminal(id);
        let name = terminal::shell_name(&shell).to_string();
        let icon = "utilities-terminal-symbolic";
        let panel = Panel::Terminal { shell };
        self.add_tab(key, page.upcast(), panel, icon, &name, widgets, sender);
        term.grab_focus();
    }
}

/// Placeholder page for a node until its addon provides a real view.
/// `kind` names the node's kind, as [`App::kind_label`] does.
pub(super) fn fill_page(page: &gtk::Box, node: &Node, kind: &str, nodes: &HashMap<NodeId, Node>) {
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
        kind.to_string()
    } else {
        format!("{kind} · {location}")
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
