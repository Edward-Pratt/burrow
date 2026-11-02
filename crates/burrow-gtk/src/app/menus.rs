//! The "New …" entries, the sidebar's context menu and the command palette.

use std::collections::BTreeMap;

use burrow_core::{Node, NodeId, POSTGRES_KIND, SSH_HOST_KIND, SSH_TUNNEL_KIND, VNC_KIND};
use relm4::gtk::prelude::*;
use relm4::gtk::{gdk, glib};
use relm4::{ComponentSender, gtk};

use crate::addons;
use crate::palette::{self, Item};
use crate::sidebar;
use crate::terminal;

use super::grants::capabilities;
use super::message::AppMsg;
use super::nodes::{BUILT_IN_KINDS, NewKind};
use super::{App, Widgets};

impl App {
    /// "New …" entries for folders, SSH hosts and the kinds of approved
    /// addons. `suffix` follows each label, such as " here".
    pub(super) fn new_entries(
        &self,
        parent: Option<Option<NodeId>>,
        suffix: &str,
    ) -> Vec<(String, AppMsg)> {
        let mut kinds: Vec<(String, NewKind)> = (BUILT_IN_KINDS.iter())
            .map(|(_, label, kind)| (label.to_string(), kind.clone()))
            .collect();
        for addon in self.addons.iter().filter(|a| addons::usable(a)) {
            for kind in &addon.kinds {
                kinds.push((kind.label.clone(), NewKind::Addon(kind.kind.clone())));
            }
        }
        // Each opens a dialog.
        kinds
            .into_iter()
            .map(|(label, kind)| {
                (
                    format!("New {label}{suffix}…"),
                    AppMsg::New { kind, parent },
                )
            })
            .collect()
    }

    /// What to call a node's kind, such as "SSH host": the raw kind only
    /// when no addon names it.
    pub(super) fn kind_label(&self, kind: &str) -> String {
        let built_in = BUILT_IN_KINDS.iter().find(|(id, ..)| *id == kind);
        let label = built_in.map(|(_, label, _)| *label).or_else(|| {
            (self.addons.iter().flat_map(|a| &a.kinds))
                .find(|k| k.kind == kind)
                .map(|k| k.label.as_str())
        });
        label.map_or_else(|| kind.to_string(), sentence_case)
    }

    pub(super) fn context_menu(
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
                    entries.push(("New shell".into(), AppMsg::NewShell(node.id)));
                    entries.push(("Browse files".into(), AppMsg::OpenFiles(node.id)));
                    entries.push(("Monitor".into(), AppMsg::OpenMonitor(node.id)));
                }
                if node.kind == SSH_TUNNEL_KIND {
                    let running = self.tunnels.get(&node.id).and_then(|s| s.running());
                    entries.push(match running {
                        Some(_) => ("Stop".into(), AppMsg::StopTunnel(node.id)),
                        None => ("Start".into(), AppMsg::StartTunnel(node.id)),
                    });
                    entries.push(("Edit…".into(), AppMsg::EditTunnel(node.id)));
                }
                if node.kind == POSTGRES_KIND {
                    entries.push(("Edit…".into(), AppMsg::EditDatabase(node.id)));
                }
                if node.kind == VNC_KIND {
                    entries.push(("Edit…".into(), AppMsg::EditDesktop(node.id)));
                }
                if !capabilities(&node.kind).is_empty() {
                    entries.push(("Claude access…".into(), AppMsg::EditGrant(node.id)));
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
            entries.push(("Move to…".into(), AppMsg::MoveTo(node.id)));
            entries.push(("Colour…".into(), AppMsg::Colour(node.id)));
            entries.push(("Delete…".into(), AppMsg::Delete(node.id)));
        }
        let popover = menu_popover(entries, sender);
        // Parented to the pane, never to the list view: the view owns its
        // children and adds and removes them as rows come and go, and a
        // popover among a list's children once hung the window when the
        // list changed while the menu was open (commit 9fa1e36).
        popover.set_parent(&widgets.sidebar_pane);
        let (x, y) = (x as i32, y as i32);
        popover.set_pointing_to(Some(&gdk::Rectangle::new(x, y, 1, 1)));
        popover.connect_closed(|popover| {
            // Unparented once the signal has returned, not inside it.
            let popover = popover.clone();
            glib::idle_add_local_once(move || popover.unparent());
        });
        popover.popup();
    }

    pub(super) fn show_palette(
        &self,
        window: &gtk::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
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
            command("Manage addons…", AppMsg::ManageAddons),
            command("Settings…", AppMsg::ShowSettings),
            command("Check for updates…", AppMsg::CheckForUpdates),
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
                .flat_map(|node| {
                    let subtitle = sidebar::location(&self.nodes, node);
                    [
                        Item {
                            title: format!("New shell: {}", node.name),
                            subtitle: subtitle.clone(),
                            message: AppMsg::NewShell(node.id),
                        },
                        Item {
                            title: format!("Browse files: {}", node.name),
                            subtitle: subtitle.clone(),
                            message: AppMsg::OpenFiles(node.id),
                        },
                        Item {
                            title: format!("Monitor: {}", node.name),
                            subtitle,
                            message: AppMsg::OpenMonitor(node.id),
                        },
                    ]
                }),
        );
        items.extend(nodes.into_iter().map(|node| {
            Item {
                title: node.name.clone(),
                subtitle: [
                    self.kind_label(&node.kind),
                    sidebar::location(&self.nodes, node),
                ]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" · "),
                message: AppMsg::Open(node.id),
            }
        }));
        let sender = sender.clone();
        palette::show(window, items, move |message| sender.input(message));
    }
}

/// `text` with its first letter capitalised, as "Folder" for "folder".
pub(crate) fn sentence_case(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

pub(crate) fn menu_popover(
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
        // Weak: the button is inside the popover.
        button.connect_clicked(glib::clone!(
            #[weak]
            popover,
            #[strong]
            sender,
            move |_| {
                popover.popdown();
                sender.input(message.clone());
            }
        ));
        column.append(&button);
    }
    popover.set_child(Some(&column));
    popover
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_labels_start_with_a_capital() {
        assert_eq!(sentence_case("folder"), "Folder");
        assert_eq!(sentence_case("SSH host"), "SSH host");
        assert_eq!(sentence_case(""), "");
    }
}
