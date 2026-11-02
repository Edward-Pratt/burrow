//! Taking the core's state: the first load, and every event after it.

use burrow_core::{AddonInfo, Alert, Event, Node, NodeId, Problem, SessionInfo, SessionState};
use relm4::{ComponentSender, gtk};

use crate::session;

use super::open::fill_page;
use super::tabs::{Panel, Tab, TabKey, files_title, monitor_title, set_tab_title, shell_title};
use super::{App, Widgets};

/// What the core sent on load, together, since the window takes it in
/// one go.
pub(super) struct Loaded {
    pub(super) nodes: Vec<Node>,
    pub(super) problems: Vec<Problem>,
    pub(super) addons: Vec<AddonInfo>,
    pub(super) sessions: Vec<SessionInfo>,
    pub(super) autostart: Vec<NodeId>,
    pub(super) monitoring: Vec<NodeId>,
    pub(super) alerts: Vec<Alert>,
}

impl App {
    /// Takes the core's state, on connecting and after every change that
    /// needs a full reload.
    pub(super) fn loaded(
        &mut self,
        loaded: Loaded,
        widgets: &mut Widgets,
        window: &gtk::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        let Loaded {
            nodes,
            problems,
            addons,
            sessions,
            autostart,
            monitoring,
            alerts,
        } = loaded;
        self.nodes = nodes.into_iter().map(|n| (n.id, n)).collect();
        self.problems = problems;
        self.autostart = autostart.into_iter().collect();
        self.monitoring = monitoring.into_iter().collect();
        self.alerts = alerts;
        self.refresh_monitors();
        self.set_addons(addons, widgets, sender);
        // Events may have been missed, so a tunnel that isn't running any
        // more has stopped.
        for status in self.tunnels.values_mut() {
            if let Some(session) = status.running()
                && !sessions.iter().any(|s| s.session == session)
            {
                status.session = Some((session, SessionState::Closed { error: None }));
            }
        }
        self.take_sessions(sessions, window, sender);
        let gone: Vec<TabKey> = self
            .tabs
            .keys()
            .filter(|key| key.node().is_some_and(|id| !self.nodes.contains_key(&id)))
            .copied()
            .collect();
        for key in gone {
            self.close_tab(key, widgets);
        }
        widgets.sidebar.replace(&self.nodes, &self.icons());
        self.restore_tabs(widgets, sender);
    }

    pub(super) fn apply_event(
        &mut self,
        event: Event,
        widgets: &Widgets,
        sender: &ComponentSender<Self>,
    ) {
        match event {
            Event::NodeCreated { node } => {
                widgets.sidebar.insert(&node, &self.icons());
                if self.made_here(&node) {
                    widgets.sidebar.show_new(node.id);
                } else if let Some(parent) = node.parent {
                    // Show where a node made elsewhere went, without taking
                    // the selection.
                    widgets.sidebar.expand(parent);
                }
                self.nodes.insert(node.id, node);
            }
            Event::NodeUpdated { node } => {
                let id = node.id;
                let kind = self.kind_label(&node.kind);
                for (key, tab) in self.tabs.iter().filter(|(key, _)| key.node() == Some(id)) {
                    let title = match (key, &tab.panel) {
                        (TabKey::Files(_), _) => files_title(&node.name),
                        (TabKey::Monitor(_), _) => monitor_title(&node.name),
                        (_, Panel::Shell(remote)) => {
                            shell_title(&node.name, remote.number, remote.run.as_ref())
                        }
                        _ => node.name.clone(),
                    };
                    set_tab_title(&tab.title, &title);
                    if let Panel::Placeholder(page) = &tab.panel {
                        fill_page(page, &node, &kind, &self.nodes);
                    }
                }
                // A running tunnel keeps the settings it started with.
                if let Some(status) = self.tunnels.get_mut(&id)
                    && status.running().is_some()
                    && self
                        .nodes
                        .get(&id)
                        .is_some_and(|old| old.props != node.props)
                {
                    status.stale = true;
                }
                widgets.sidebar.update(&node, &self.icons());
                self.nodes.insert(id, node);
                // Tunnel pages name the host they go through.
                self.refresh_tunnels(sender);
                // The addon draws from the node, so ask it again.
                self.load_view(id, sender);
            }
            Event::NodeDeleted { id } => {
                self.nodes.remove(&id);
                self.tunnels.remove(&id);
                self.restarting.remove(&id);
                let tabs: Vec<TabKey> = (self.tabs.keys())
                    .filter(|key| key.node() == Some(id))
                    .copied()
                    .collect();
                for key in tabs {
                    self.close_tab(key, widgets);
                }
                widgets.sidebar.remove(id);
                self.refresh_tunnels(sender);
            }
            Event::ProblemsChanged { problems } => self.problems = problems,
            // Handled by `session_event`, which needs the window.
            // The grant dialog lists grants when it opens.
            Event::SessionChanged { .. }
            | Event::PromptRequested { .. }
            | Event::GrantsChanged { .. } => {}
            Event::MonitoringChanged { nodes } => {
                self.monitoring = nodes.into_iter().collect();
                self.refresh_monitors();
            }
            Event::AlertRaised { alert } => {
                self.alerts.push(alert);
                self.refresh_monitors();
            }
            Event::AlertCleared { alert } => {
                self.alerts.retain(|a| a.id != alert.id);
                self.refresh_monitors();
            }
            Event::PromptClosed { session, prompt } => {
                if let Some(dialog) = self.prompts.get_mut().remove(&(session, prompt)) {
                    dialog.dismiss();
                }
            }
            Event::AddonsChanged { addons } => self.set_addons(addons, widgets, sender),
            Event::ViewChanged { node, view } => self.show_view(node, Ok(view), sender),
            Event::MonitorSample {
                session, sample, ..
            } => {
                let tab = self.tab_of(session).and_then(|key| self.tabs.get_mut(&key));
                if let Some(Tab {
                    panel: Panel::Monitor(panel),
                    ..
                }) = tab
                {
                    panel.sample(&sample, &self.core, sender);
                }
            }
            Event::MonitorUnits { session, units, .. } => {
                let tab = self.tab_of(session).and_then(|key| self.tabs.get_mut(&key));
                if let Some(Tab {
                    panel: Panel::Monitor(panel),
                    ..
                }) = tab
                {
                    panel.set_units(units);
                }
            }
            Event::MonitorApps { session, apps, .. } => {
                let tab = self.tab_of(session).and_then(|key| self.tabs.get(&key));
                if let Some(Tab {
                    panel: Panel::Monitor(panel),
                    ..
                }) = tab
                {
                    panel.set_apps(&apps);
                }
            }
            Event::AutostartChanged { nodes } => {
                self.autostart = nodes.into_iter().collect();
                self.refresh_tunnels(sender);
            }
            Event::TransferChanged {
                transfer,
                session,
                state,
            } => {
                // A transfer's last events can come after its session
                // ended, so the panel showing it comes first.
                let running = session::State::Running(session);
                let panel = (self.tabs.values_mut()).find_map(|tab| match &mut tab.panel {
                    Panel::Files(panel)
                        if panel.has_transfer(transfer) || panel.session.state == running =>
                    {
                        Some(panel)
                    }
                    _ => None,
                });
                if let Some(panel) = panel {
                    panel.transfer_changed(transfer, &state, &self.core, sender);
                }
            }
        }
    }
}
