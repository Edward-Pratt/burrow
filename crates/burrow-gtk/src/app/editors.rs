//! The dialogs that create or edit a tunnel, a database or a desktop.

use std::collections::HashMap;

use burrow_core::{
    Command, Node, NodeId, NodePatch, POSTGRES_KIND, Response, SSH_HOST_KIND, SSH_TUNNEL_KIND,
    TunnelShown, VNC_KIND,
};
use relm4::{ComponentSender, gtk};

use crate::database::{self};
use crate::desktop::{self};
use crate::runtime::runtime;
use crate::tunnels;

use super::App;
use super::daemon::{execute, run};
use super::message::AppMsg;

impl App {
    /// Creates a tunnel in `parent`, or edits the tunnel `id`.
    pub(super) fn edit_tunnel(
        &self,
        id: Option<NodeId>,
        parent: Option<NodeId>,
        window: &gtk::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        let node = id.and_then(|id| self.nodes.get(&id));
        if id.is_some() && node.is_none() {
            return;
        }
        let mut hosts: Vec<(NodeId, String)> = self
            .nodes
            .values()
            .filter(|n| n.kind == SSH_HOST_KIND)
            .map(|n| (n.id, n.name.clone()))
            .collect();
        hosts.sort_by_key(|(id, name)| (name.to_lowercase(), *id));
        // Starting with Burrow is pinned to the settings, so a change
        // withdraws it. A change made here is the user's own, so it is
        // asked for again.
        let autostart = id.is_some_and(|id| self.autostart.contains(&id));
        // The hosts as they were when the dialog opened.
        let host_nodes: HashMap<NodeId, Node> = self
            .nodes
            .iter()
            .filter(|(_, n)| n.kind == SSH_HOST_KIND)
            .map(|(id, n)| (*id, n.clone()))
            .collect();
        let (core, input) = (self.core.clone(), sender.input_sender().clone());
        tunnels::edit(window, node, hosts, move |name, props| {
            let saved = shown(&host_nodes, &props);
            let command = match id {
                Some(id) => Command::UpdateNode {
                    id,
                    patch: NodePatch {
                        name: Some(name),
                        props: Some(props),
                        ..Default::default()
                    },
                },
                None => Command::CreateNode {
                    parent,
                    kind: SSH_TUNNEL_KIND.into(),
                    name,
                    color: None,
                    props,
                },
            };
            let (core, input) = (core.clone(), input.clone());
            runtime().spawn(async move {
                let result = match run(&core, command).await {
                    // Only the settings saved here: a file that changed
                    // meanwhile makes this fail rather than be approved.
                    Ok(Response::Node { node }) if autostart => {
                        let command = Command::SetAutostart {
                            node: node.id,
                            enabled: true,
                            expect: saved,
                        };
                        run(&core, command).await
                    }
                    other => other,
                };
                if let Err(error) = result {
                    let _ = input.send(AppMsg::Failed(error));
                }
            });
        });
    }

    /// Creates a database in `parent`, or edits the database `id`.
    pub(super) fn edit_database(
        &self,
        id: Option<NodeId>,
        parent: Option<NodeId>,
        window: &gtk::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        let node = id.and_then(|id| self.nodes.get(&id));
        if id.is_some() && node.is_none() {
            return;
        }
        let mut hosts: Vec<(NodeId, String)> = self
            .nodes
            .values()
            .filter(|n| n.kind == SSH_HOST_KIND)
            .map(|n| (n.id, n.name.clone()))
            .collect();
        hosts.sort_by_key(|(id, name)| (name.to_lowercase(), *id));
        let (core, sender) = (self.core.clone(), sender.clone());
        database::edit(window, node, hosts, move |name, props| {
            let command = match id {
                Some(id) => Command::UpdateNode {
                    id,
                    patch: NodePatch {
                        name: Some(name),
                        props: Some(props),
                        ..Default::default()
                    },
                },
                None => Command::CreateNode {
                    parent,
                    kind: POSTGRES_KIND.into(),
                    name,
                    color: None,
                    props,
                },
            };
            execute(&core, &sender, command);
        });
    }

    pub(super) fn edit_desktop(
        &self,
        id: Option<NodeId>,
        parent: Option<NodeId>,
        window: &gtk::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        let node = id.and_then(|id| self.nodes.get(&id));
        if id.is_some() && node.is_none() {
            return;
        }
        let mut hosts: Vec<(NodeId, String)> = (self.nodes.values())
            .filter(|n| n.kind == SSH_HOST_KIND)
            .map(|n| (n.id, n.name.clone()))
            .collect();
        hosts.sort_by_key(|(id, name)| (name.to_lowercase(), *id));
        let (core, sender) = (self.core.clone(), sender.clone());
        desktop::edit(window, node, hosts, move |name, props| {
            let command = match id {
                Some(id) => Command::UpdateNode {
                    id,
                    patch: NodePatch {
                        name: Some(name),
                        props: Some(props),
                        ..Default::default()
                    },
                },
                None => Command::CreateNode {
                    parent,
                    kind: VNC_KIND.into(),
                    name,
                    color: None,
                    props,
                },
            };
            execute(&core, &sender, command);
        });
    }
}

/// A tunnel's settings as the window shows them, for `set_autostart`:
/// `props` and the host they go through, as found in `nodes`.
pub(super) fn shown(nodes: &HashMap<NodeId, Node>, props: &toml::Table) -> Option<TunnelShown> {
    let via = props.get("via")?.as_str()?.parse().ok().map(NodeId)?;
    let host = nodes.get(&via)?;
    Some(TunnelShown {
        props: props.clone(),
        host_name: host.name.clone(),
        host_props: host.props.clone(),
    })
}
