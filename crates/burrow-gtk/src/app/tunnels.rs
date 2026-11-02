//! Tunnel state, by node, whether or not a tunnel's tab is open.

use std::collections::HashSet;
use std::rc::Rc;

use burrow_core::{Command, NodeId, Prompt, SessionId, SessionState};
use relm4::ComponentSender;

use crate::tunnels;

use super::App;
use super::daemon::execute;
use super::message::AppMsg;
use super::tabs::{Panel, TabKey};

use super::daemon::run;
use super::editors::shown;
use crate::runtime::runtime;
impl App {
    /// The nodes the sidebar marks as running: tunnels that are up.
    pub(super) fn running(&self) -> HashSet<NodeId> {
        self.tunnels
            .iter()
            .filter(|(_, status)| status.is_up())
            .map(|(id, _)| *id)
            .collect()
    }

    pub(super) fn tunnel_of(&self, session: SessionId) -> Option<NodeId> {
        self.tunnels
            .iter()
            .find(|(_, status)| status.running() == Some(session))
            .map(|(id, _)| *id)
    }

    /// A tunnel can ask while its tab is closed, or when it started with
    /// Burrow, so its questions say which tunnel is asking.
    pub(super) fn for_tunnel(&self, node: NodeId, prompt: &Prompt) -> Prompt {
        let name = self
            .nodes
            .get(&node)
            .map_or("a tunnel", |n| n.name.as_str());
        Prompt {
            message: format!("For the tunnel “{name}”.\n\n{}", prompt.message),
            ..prompt.clone()
        }
    }

    pub(super) fn tunnel_changed(
        &mut self,
        node: NodeId,
        session: SessionId,
        state: SessionState,
        sender: &ComponentSender<Self>,
    ) {
        let status = self.tunnels.entry(node).or_default();
        match &status.session {
            // A late event from a session that has been replaced.
            Some((current, _)) if session < *current => return,
            Some((current, _)) if session == *current => {}
            _ => status.stale = false,
        }
        let closed = matches!(state, SessionState::Closed { .. });
        status.session = Some((session, state));
        if closed && self.restarting.remove(&node) {
            execute(&self.core, sender, Command::StartTunnel { node });
        }
        self.refresh_tunnel(node, sender);
    }

    pub(super) fn refresh_tunnels(&self, sender: &ComponentSender<Self>) {
        for key in self.tabs.keys() {
            if let TabKey::Node(id) = key {
                self.refresh_tunnel(*id, sender);
            }
        }
    }

    /// Draws a tunnel's tab again, if it is open.
    pub(super) fn refresh_tunnel(&self, id: NodeId, sender: &ComponentSender<Self>) {
        let panel = self.tabs.get(&TabKey::Node(id)).map(|tab| &tab.panel);
        let (Some(Panel::Tunnel(tunnel)), Some(node)) = (panel, self.nodes.get(&id)) else {
            return;
        };
        let send = |message: AppMsg| -> Rc<dyn Fn()> {
            let sender = sender.clone();
            Rc::new(move || sender.input(message.clone()))
        };
        let actions = tunnels::Actions {
            start: send(AppMsg::StartTunnel(id)),
            stop: send(AppMsg::StopTunnel(id)),
            restart: send(AppMsg::RestartTunnel(id)),
            edit: send(AppMsg::EditTunnel(id)),
            set_autostart: {
                let sender = sender.clone();
                Rc::new(move |enabled| sender.input(AppMsg::SetAutostart { node: id, enabled }))
            },
        };
        let status = self.tunnels.get(&id).cloned().unwrap_or_default();
        let autostart = self.autostart.contains(&id);
        tunnel.fill(node, &self.nodes, &status, autostart, &actions);
    }

    pub(super) fn stop_tunnel(&mut self, node: NodeId, sender: &ComponentSender<Self>) {
        self.restarting.remove(&node);
        if let Some(session) = self.tunnels.get(&node).and_then(|s| s.running()) {
            execute(&self.core, sender, Command::CloseSession { session });
        }
    }

    /// Stops the tunnel and starts it again once it has closed.
    pub(super) fn restart_tunnel(&mut self, node: NodeId, sender: &ComponentSender<Self>) {
        if let Some(session) = self.tunnels.get(&node).and_then(|s| s.running()) {
            self.restarting.insert(node);
            execute(&self.core, sender, Command::CloseSession { session });
        }
    }

    /// Starts the tunnel with Burrow from now on, or stops doing so. A
    /// failure arrives as [`AppMsg::AutostartFailed`].
    pub(super) fn set_autostart(
        &self,
        node: NodeId,
        enabled: bool,
        sender: &ComponentSender<Self>,
    ) {
        // What the tab shows: it is redrawn on every change.
        let expect = self
            .nodes
            .get(&node)
            .and_then(|n| shown(&self.nodes, &n.props));
        let (core, input) = (self.core.clone(), sender.input_sender().clone());
        runtime().spawn(async move {
            let command = Command::SetAutostart {
                node,
                enabled,
                expect,
            };
            if let Err(error) = run(&core, command).await {
                let _ = input.send(AppMsg::AutostartFailed { node, error });
            }
        });
    }
}
