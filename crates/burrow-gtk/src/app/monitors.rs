//! Monitor tabs, background monitoring and service actions.

use burrow_core::{
    Alert, Command, GrantShown, Node, NodeId, Response, SSH_HOST_KIND, Service, ServiceAction,
};
use relm4::gtk::prelude::*;
use relm4::{ComponentSender, gtk};

use crate::dialogs;
use crate::files::{self};
use crate::monitor::{self, Monitor};
use crate::runtime::runtime;

use super::daemon::run;
use super::message::AppMsg;
use super::tabs::{Panel, TabKey, monitor_title};
use super::{App, Widgets};

use super::menus::sentence_case;
use burrow_core::TerminalRun;
impl App {
    /// Opens a new shell on the SSH host `id`.
    /// Runs `action` on `service`. How it went arrives as
    /// [`AppMsg::Controlled`].
    pub(super) fn control(
        &self,
        node: NodeId,
        service: Service,
        action: ServiceAction,
        sender: &ComponentSender<Self>,
    ) {
        let (core, input) = (self.core.clone(), sender.input_sender().clone());
        runtime().spawn(async move {
            let command = Command::ControlService {
                node,
                service: service.clone(),
                action,
            };
            let result = match core.execute(command).await {
                Ok(Response::Controlled {
                    result,
                    needs_password,
                }) => Ok((result, needs_password)),
                Ok(other) => Err(format!("unexpected reply {other:?}")),
                Err(e) => Err(e.to_string()),
            };
            let _ = input.send(AppMsg::Controlled {
                node,
                service,
                action,
                result,
            });
        });
    }

    pub(super) fn open_monitor(
        &mut self,
        id: NodeId,
        widgets: &Widgets,
        sender: &ComponentSender<Self>,
    ) {
        let key = TabKey::Monitor(id);
        if self.show_tab(key, widgets) {
            return;
        }
        let Some(node) = self.nodes.get(&id).filter(|n| n.kind == SSH_HOST_KIND) else {
            return;
        };
        let title = monitor_title(&node.name);
        let panel = Monitor::new(id, sender);
        let page = panel.page.clone().upcast();
        let icon = "utilities-system-monitor-symbolic";
        monitor::start(&self.core, id, sender);
        self.add_tab(
            key,
            page,
            Panel::Monitor(Box::new(panel)),
            icon,
            &title,
            widgets,
            sender,
        );
        self.refresh_monitors();
    }

    /// Shows each monitor tab whether its host is monitored in the
    /// background, and its alerts.
    pub(super) fn refresh_monitors(&mut self) {
        for (key, tab) in &mut self.tabs {
            if let (TabKey::Monitor(node), Panel::Monitor(panel)) = (key, &mut tab.panel) {
                panel.set_background(self.monitoring.contains(node));
                let alerts: Vec<&Alert> = self.alerts.iter().filter(|a| a.node == *node).collect();
                panel.set_alerts(&alerts);
            }
        }
    }

    /// Turns background monitoring of `id` on, after saying what that
    /// means, or off.
    pub(super) fn set_monitoring(
        &mut self,
        id: NodeId,
        on: bool,
        window: &gtk::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        let Some(node) = self.nodes.get(&id).cloned() else {
            return;
        };
        let (core, input) = (self.core.clone(), sender.input_sender().clone());
        let send = move |command: Command| {
            runtime().spawn(async move {
                if let Err(error) = run(&core, command).await {
                    let _ = input.send(AppMsg::MonitoringFailed(error));
                }
            });
        };
        if !on {
            send(Command::SetMonitoring {
                node: id,
                enabled: false,
                expect: None,
            });
            return;
        }
        // The checkbox follows the core, so it stays off until the core
        // says otherwise.
        self.refresh_monitors();
        let nodes: Vec<Node> = self.nodes.values().cloned().collect();
        let expect = Some(GrantShown::of(&node, &nodes));
        let name = files::printable(&node.name);
        let detail = format!(
            "While Burrow runs, it keeps a monitor open on {name}, even with no window open, \
             keeps a week of its history on this computer, and shows a notification when a \
             service fails, a disk is nearly full, memory runs low, or {name} stops answering.\n\n\
             It installs burrow-agent there if needed, as an open monitor does. It holds only \
             while {name}'s settings stay as they are. To monitor from login, enable \
             burrowd.service (systemctl --user enable burrowd.service)."
        );
        dialogs::confirm(
            window,
            &format!("Monitor {name} in the background?"),
            &detail,
            "Monitor",
            move || {
                send(Command::SetMonitoring {
                    node: id,
                    enabled: true,
                    expect,
                })
            },
        );
    }

    /// Runs `action` on `service`, asking first when it interrupts the
    /// service.
    pub(super) fn confirm_control(
        &self,
        node: NodeId,
        service: Service,
        action: ServiceAction,
        window: &gtk::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        if !monitor::confirms(action) {
            return self.control(node, service, action, sender);
        }
        let host = self.nodes.get(&node).map(|n| n.name.as_str()).unwrap_or("");
        let (host, name) = (files::printable(host), files::printable(service.name()));
        let verb = sentence_case(action.verb());
        let detail = match action {
            ServiceAction::Stop => format!("{name} stops on {host} until it is started."),
            ServiceAction::Disable => format!("{name} won't start when {host} boots."),
            _ => format!("{name} stops on {host} and starts again."),
        };
        let sender = sender.clone();
        dialogs::confirm(
            window,
            &format!("{verb} {name}?"),
            &detail,
            &verb,
            move || {
                sender.input(AppMsg::ControlNow {
                    node,
                    service,
                    action,
                })
            },
        );
    }

    /// Takes how an action went: what it printed, and whether sudo wanted
    /// a password.
    pub(super) fn controlled(
        &mut self,
        node: NodeId,
        service: Service,
        action: ServiceAction,
        result: Result<(burrow_core::ExecResult, bool), String>,
        widgets: &Widgets,
        sender: &ComponentSender<Self>,
    ) {
        let name = files::printable(service.name());
        match result {
            // sudo asks in a terminal, where the password goes
            // straight to it.
            Ok((_, true)) => {
                let run = TerminalRun::Control { service, action };
                self.new_shell(node, Some(run), widgets, sender);
            }
            Ok((result, false)) if result.status == Some(0) => {
                let done = format!("{name}: {} done.", action.verb());
                sender.input(AppMsg::Notice(done));
            }
            Ok((result, false)) => {
                let said = [&result.stderr, &result.stdout]
                    .into_iter()
                    .flat_map(|text| text.lines())
                    .map(str::trim)
                    .find(|line| !line.is_empty())
                    .map(files::printable);
                let why = said.unwrap_or_else(|| match result.status {
                    Some(status) => format!("it exited with {status}"),
                    None => "it was stopped".into(),
                });
                self.fail(format!("Could not {} {name}: {why}", action.verb()));
            }
            Err(error) => {
                self.fail(format!("Could not {} {name}: {error}", action.verb()));
            }
        }
    }
}
