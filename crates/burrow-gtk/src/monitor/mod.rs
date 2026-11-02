//! Monitor tabs: an SSH host's CPU, memory, disks, network and systemd
//! services, as burrow-agent reports them through the core.
//!
//! The core already cleaned what the agent sent, but names are passed
//! through [`printable`] anyway, as everything from a server is.
//!
//! A host monitored in the background also shows its alerts, and its
//! charts can show the history the core keeps on disk instead of the last
//! ten minutes. The daemon notifies about alerts, so the tab only lists
//! them.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Instant;

use burrow_core::ipc::Client;
use burrow_core::monitor::{Sample, Unit};
use burrow_core::{
    Alert, AlertKind, Command, MonitorHistory, NodeId, Response, SessionState, TrendRange,
};
use relm4::gtk::prelude::*;
use relm4::{ComponentSender, gtk};

use crate::app::{App, AppMsg};
use crate::files::printable;
use crate::runtime::runtime;
use crate::session::{self, SessionPage};

mod apps;
mod charts;
mod entries;
mod page;
mod samples;
mod service_menu;
mod services;

use apps::AppsSection;
use charts::{CHART_POINTS, Chart, Meter};
use page::label;
use service_menu::Menu;
use services::UnitRow;

pub use service_menu::confirms;

#[derive(Clone, Debug)]
pub enum Msg {
    /// `open_monitor` and what follows.
    Session(session::Msg),
    /// What the monitor had seen before the tab joined it, which a
    /// background monitor has: whether systemd runs there, the latest
    /// services, and samples for the charts.
    History(MonitorHistory),
    /// The service filter or "failed only" changed.
    Filter,
    /// The user turned "Monitor in the background" on or off. The window
    /// asks first, since it knows the host.
    SetBackground(bool),
    /// The user picked what the charts show.
    Range(Option<TrendRange>),
    /// The history on disk for `range`, from `monitor_trend`.
    Trend(TrendRange, Vec<Sample>),
}

/// One SSH host's monitor tab.
pub struct Monitor {
    node: NodeId,
    pub page: gtk::Box,
    pub session: SessionPage,
    summary: gtk::Label,
    cpu: Chart,
    cpu_label: gtk::Label,
    net: Chart,
    net_label: gtk::Label,
    memory: Meter,
    swap: Meter,
    disks: gtk::Grid,
    /// The mounts in `disks`, in order, so it is only rebuilt when they
    /// change.
    disk_rows: Vec<(String, Meter)>,
    units_label: gtk::Label,
    filter: gtk::SearchEntry,
    failed_only: gtk::CheckButton,
    units_list: gtk::ListBox,
    /// The rows in `units_list`, in order, so a new list only changes the
    /// rows that differ.
    unit_rows: Vec<UnitRow>,
    systemd: bool,
    units: Option<Vec<Unit>>,
    containers: AppsSection,
    pm2: AppsSection,
    /// Opens the menu of a service's actions at a row's button.
    menu: Menu,
    background: gtk::CheckButton,
    /// Set while the checkbox is changed to match the core, so that isn't
    /// taken for the user's choice.
    quiet: Rc<Cell<bool>>,
    ranges: gtk::Box,
    range_buttons: Vec<gtk::ToggleButton>,
    range: Option<TrendRange>,
    trend_note: gtk::Label,
    trend_read: Option<Instant>,
    alerts: gtk::Box,
}

impl Monitor {
    /// Shows whether the host is monitored in the background, as the core
    /// says. Its history is only there if it is.
    pub fn set_background(&mut self, on: bool) {
        self.quiet.set(true);
        self.background.set_active(on);
        self.quiet.set(false);
        self.ranges.set_visible(on);
        if !on && self.range.is_some() {
            // Goes back to the live charts through `Msg::Range(None)`.
            self.range_buttons[0].set_active(true);
        }
    }

    /// Lists the host's alerts above the charts.
    pub fn set_alerts(&self, alerts: &[&Alert]) {
        while let Some(child) = self.alerts.first_child() {
            self.alerts.remove(&child);
        }
        for alert in alerts {
            let text = label(&alert_text(&alert.kind));
            text.add_css_class("warning-text");
            text.set_wrap(true);
            self.alerts.append(&text);
        }
        self.alerts.set_visible(!alerts.is_empty());
    }

    pub fn session_changed(
        &mut self,
        state: &SessionState,
        core: &Client,
        sender: &ComponentSender<App>,
    ) {
        let ended = |error: Option<&str>| session::ended("The monitor stopped", error);
        if self.session.session_changed(state, ended) {
            self.session
                .show_message("Waiting for the first sample…", false);
            self.fetch_history(core, sender);
        }
    }

    /// Asks what the monitor has seen. Whether systemd runs there is only
    /// in the history, and so are the services of a monitor that was
    /// running before the tab joined it.
    pub fn fetch_history(&self, core: &Client, sender: &ComponentSender<App>) {
        let Some(session) = self.session.running() else {
            return;
        };
        let (core, node, input) = (core.clone(), self.node, sender.input_sender().clone());
        runtime().spawn(async move {
            if let Ok(Response::Monitor { history }) =
                core.execute(Command::MonitorHistory { session }).await
            {
                let msg = Msg::History(history);
                let _ = input.send(AppMsg::Monitor { node, msg });
            }
        });
    }

    pub fn update(&mut self, msg: Msg, core: &Client, sender: &ComponentSender<App>) {
        match msg {
            // Handled by the window, which knows the other sessions.
            Msg::Session(session::Msg::Started(_)) => {}
            Msg::Session(session::Msg::StartFailed(error)) => self.session.start_failed(&error),
            Msg::Session(session::Msg::Reconnect) => {
                if self.session.reconnect() {
                    start(core, self.node, sender);
                }
            }
            Msg::History(history) => {
                if let Some(apps) = &history.apps {
                    self.set_apps(apps);
                }
                self.systemd = history.systemd;
                if history.units.is_some() {
                    self.units = history.units;
                }
                self.fill_units();
                let recent = history.samples.len().saturating_sub(CHART_POINTS);
                for sample in &history.samples[recent..] {
                    self.cpu.push(&[sample.cpu.percent]);
                    let rx = sample.net.iter().map(|i| i.rx).sum();
                    let tx = sample.net.iter().map(|i| i.tx).sum();
                    self.net.push(&[rx, tx]);
                }
                self.scale_net();
            }
            Msg::Filter => self.fill_units(),
            // Handled by the window, which asks first.
            Msg::SetBackground(_) => {}
            Msg::Range(range) => {
                self.range = range;
                match range {
                    Some(range) => self.read_trend(range, core, sender),
                    None => {
                        self.cpu.show(None);
                        self.net.show(None);
                        self.scale_net();
                        self.trend_note.set_visible(false);
                    }
                }
            }
            Msg::Trend(range, points) => {
                if self.range == Some(range) {
                    self.show_trend(range, &points);
                }
            }
        }
    }
}

/// Asks the core for a monitor on `node`.
pub fn start(core: &Client, node: NodeId, sender: &ComponentSender<App>) {
    let wrap = move |msg| AppMsg::Monitor {
        node,
        msg: Msg::Session(msg),
    };
    session::start(core, Command::OpenMonitor { node }, sender, wrap);
}

/// One line about an alert. Names come from the server.
fn alert_text(kind: &AlertKind) -> String {
    match kind {
        AlertKind::UnitFailed { unit } => format!("{} failed", printable(unit)),
        AlertKind::DiskFull { mount, percent } => {
            format!("{} is {percent:.0}% full", printable(mount))
        }
        AlertKind::MemoryLow { percent } => {
            format!("Only {percent:.0}% of memory is available")
        }
        AlertKind::Unreachable { error } => format!("Not answering: {}", printable(error)),
        AlertKind::ContainerFailed { container, status } => {
            format!("Container {}: {}", printable(container), printable(status))
        }
        AlertKind::AppFailed { app } => format!("PM2 app {} errored", printable(app)),
    }
}
