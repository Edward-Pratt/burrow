//! Creating, renaming, moving, colouring and deleting nodes.

use std::time::Duration;

use burrow_core::{
    Command, FOLDER_KIND, Node, NodeId, NodePatch, POSTGRES_KIND, SSH_HOST_KIND, SSH_TUNNEL_KIND,
    VNC_KIND,
};
use relm4::{ComponentSender, gtk};

use crate::dialogs;
use crate::hosts;
use crate::sidebar;

use super::daemon::execute;
use super::{App, Widgets};

/// What "New …" creates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NewKind {
    Folder,
    SshHost,
    SshTunnel,
    Database,
    Desktop,
    /// A kind owned by an addon.
    Addon(String),
}

/// The kinds Burrow knows itself: their id, what "New …" calls them, and
/// what it creates.
pub(super) const BUILT_IN_KINDS: [(&str, &str, NewKind); 5] = [
    (FOLDER_KIND, "folder", NewKind::Folder),
    (SSH_HOST_KIND, "SSH host", NewKind::SshHost),
    (SSH_TUNNEL_KIND, "SSH tunnel", NewKind::SshTunnel),
    (POSTGRES_KIND, "PostgreSQL database", NewKind::Database),
    (VNC_KIND, "VNC desktop", NewKind::Desktop),
];

impl NewKind {
    /// The kind of node it creates.
    pub(super) fn kind(&self) -> &str {
        match self {
            NewKind::Folder => FOLDER_KIND,
            NewKind::SshHost => SSH_HOST_KIND,
            NewKind::SshTunnel => SSH_TUNNEL_KIND,
            NewKind::Database => POSTGRES_KIND,
            NewKind::Desktop => VNC_KIND,
            NewKind::Addon(kind) => kind,
        }
    }
}

/// How long after "New …" a node of that kind, created in that folder,
/// counts as the one the user made. Its dialog can stay open a while.
pub(super) const NEW_NODE_WAIT: Duration = Duration::from_secs(600);

impl App {
    /// Whether `node` is the one "New …" in this window was asked for.
    pub(super) fn made_here(&mut self, node: &Node) -> bool {
        let Some((kind, parent, since)) = &self.creating else {
            return false;
        };
        let made = *kind == node.kind && *parent == node.parent && since.elapsed() < NEW_NODE_WAIT;
        if made {
            self.creating = None;
        }
        made
    }

    /// The folder new nodes go into: the selected folder, or the folder of
    /// the selected leaf, or the top level.
    pub(super) fn selection_folder(&self, widgets: &Widgets) -> Option<NodeId> {
        let node = self.nodes.get(&selected(widgets)?)?;
        if node.is_folder() {
            Some(node.id)
        } else {
            node.parent
        }
    }

    pub(super) fn new_node(
        &self,
        kind: NewKind,
        parent: Option<NodeId>,
        window: &gtk::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        let (title, label) = match &kind {
            NewKind::Folder => ("New folder".to_string(), "Folder name"),
            NewKind::SshHost => return hosts::new_host(window, self.core.clone(), parent),
            NewKind::SshTunnel => return self.edit_tunnel(None, parent, window, sender),
            NewKind::Database => return self.edit_database(None, parent, window, sender),
            NewKind::Desktop => return self.edit_desktop(None, parent, window, sender),
            NewKind::Addon(kind) => {
                let label = self
                    .addons
                    .iter()
                    .flat_map(|a| &a.kinds)
                    .find(|k| &k.kind == kind)
                    .map_or(kind.as_str(), |k| k.label.as_str());
                (format!("New {label}"), "Name")
            }
        };
        let (core, sender) = (self.core.clone(), sender.clone());
        dialogs::prompt(window, &title, label, "", "Create", move |name| {
            let (kind, props) = match &kind {
                NewKind::Folder => ("folder", toml::Table::new()),
                NewKind::Addon(kind) => (kind.as_str(), toml::Table::new()),
                NewKind::SshHost | NewKind::SshTunnel | NewKind::Database | NewKind::Desktop => {
                    unreachable!("hosts, tunnels, databases and desktops have their own dialogs")
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

    pub(super) fn rename(
        &self,
        id: NodeId,
        window: &gtk::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
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

    pub(super) fn delete(
        &self,
        id: NodeId,
        window: &gtk::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        let Some(node) = self.nodes.get(&id) else {
            return;
        };
        // The core closes the sessions of every node it deletes.
        let detail = if node.is_folder() {
            "Everything inside it is deleted too, along with any saved passwords. \
             Anything running on what it holds, such as a terminal or a tunnel, stops."
        } else {
            "Its saved passwords are removed from the keyring too. Anything running \
             on it, such as a terminal or a tunnel, stops."
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

    /// "Move to…", for keyboard users and long lists, where dragging is
    /// awkward.
    pub(super) fn move_to(
        &self,
        id: NodeId,
        window: &gtk::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        let Some(node) = self.nodes.get(&id) else {
            return;
        };
        let choices = sidebar::move_choices(&self.nodes, id);
        let (core, sender) = (self.core.clone(), sender.clone());
        sidebar::move_dialog(window, node, choices, move |parent| {
            execute(&core, &sender, Command::MoveNode { id, parent });
        });
    }

    pub(super) fn colour(
        &self,
        id: NodeId,
        window: &gtk::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        let Some(node) = self.nodes.get(&id) else {
            return;
        };
        let (core, sender, current) = (self.core.clone(), sender.clone(), node.color.clone());
        sidebar::colour_dialog(window, node, move |color| {
            if color != current {
                let patch = NodePatch {
                    color: Some(color),
                    ..Default::default()
                };
                execute(&core, &sender, Command::UpdateNode { id, patch });
            }
        });
    }
}

pub(super) fn selected(widgets: &Widgets) -> Option<NodeId> {
    widgets.sidebar.selected()
}
