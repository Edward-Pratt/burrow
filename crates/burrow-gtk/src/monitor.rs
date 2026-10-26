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

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::rc::Rc;
use std::time::{Duration, Instant};

use burrow_core::ipc::Client;
use burrow_core::monitor::{Apps, Sample, Unit};
use burrow_core::{
    Alert, AlertKind, Command, MonitorHistory, NodeId, Response, SessionState, TrendRange,
};
use relm4::gtk::prelude::*;
use relm4::{ComponentSender, gtk};

use crate::app::{App, AppMsg};
use crate::files::{printable, size};
use crate::runtime::runtime;
use crate::session::{self, SessionPage};

/// Samples the charts show: ten minutes, at one every 2 seconds.
const CHART_POINTS: usize = 300;

/// Services drawn before the rest are summed up.
const MAX_ROWS: usize = 2000;

/// What the charts can show: the live samples, or the history on disk.
const RANGES: [(&str, Option<TrendRange>); 4] = [
    ("Live", None),
    ("1 h", Some(TrendRange::Hour)),
    ("24 h", Some(TrendRange::Day)),
    ("7 d", Some(TrendRange::Week)),
];

/// How often a chart of the history on disk is read again. A point is
/// written a minute.
const TREND_REFRESH: Duration = Duration::from_secs(60);

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
    pub fn new(node: NodeId, sender: &ComponentSender<App>) -> Monitor {
        let send = |msg: Msg| {
            let sender = sender.clone();
            move || {
                sender.input(AppMsg::Monitor {
                    node,
                    msg: msg.clone(),
                })
            }
        };

        let page = gtk::Box::new(gtk::Orientation::Vertical, 8);
        page.add_css_class("monitor-view");

        let top = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        let background = gtk::CheckButton::with_label("Monitor in the background");
        background.set_tooltip_text(Some(
            "Keep monitoring this host while Burrow runs, keep a week of its history, \
             and notify when something goes wrong",
        ));
        let quiet = Rc::new(Cell::new(false));
        background.connect_toggled({
            let (quiet, sender) = (quiet.clone(), sender.clone());
            move |check| {
                if !quiet.get() {
                    let msg = Msg::SetBackground(check.is_active());
                    sender.input(AppMsg::Monitor { node, msg });
                }
            }
        });
        top.append(&background);
        let ranges = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        ranges.add_css_class("linked");
        ranges.set_hexpand(true);
        ranges.set_halign(gtk::Align::End);
        ranges.set_visible(false);
        let mut range_buttons: Vec<gtk::ToggleButton> = Vec::new();
        for (name, range) in RANGES {
            let button = gtk::ToggleButton::with_label(name);
            button.set_group(range_buttons.first());
            button.set_active(range.is_none());
            button.connect_toggled({
                let sender = sender.clone();
                move |button| {
                    if button.is_active() {
                        let msg = Msg::Range(range);
                        sender.input(AppMsg::Monitor { node, msg });
                    }
                }
            });
            ranges.append(&button);
            range_buttons.push(button);
        }
        top.append(&ranges);
        page.append(&top);

        let alerts = gtk::Box::new(gtk::Orientation::Vertical, 4);
        alerts.set_visible(false);
        page.append(&alerts);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 18);
        let summary = label("");
        summary.add_css_class("dim");
        content.append(&summary);

        let trend_note = label("");
        trend_note.add_css_class("dim");
        trend_note.set_visible(false);
        content.append(&trend_note);

        let charts = gtk::Box::new(gtk::Orientation::Horizontal, 18);
        charts.set_homogeneous(true);
        let cpu = Chart::new(&["#3584e4"]);
        let cpu_label = heading("CPU");
        charts.append(&section(&cpu_label, &cpu.area));
        let net = Chart::new(&["#26a269", "#c88800"]);
        let net_label = heading("Network");
        charts.append(&section(&net_label, &net.area));
        content.append(&charts);

        let memory = Meter::new();
        let swap = Meter::new();
        let memory_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
        memory_box.append(&heading("Memory"));
        memory_box.append(&memory.row("Memory"));
        memory_box.append(&swap.row("Swap"));
        content.append(&memory_box);

        let disks = gtk::Grid::new();
        disks.set_column_spacing(12);
        disks.set_row_spacing(6);
        let disks_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
        disks_box.append(&heading("Disks"));
        disks_box.append(&disks);
        content.append(&disks_box);

        let units_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let units_label = heading("Services");
        units_box.append(&units_label);
        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        let filter = gtk::SearchEntry::new();
        filter.set_placeholder_text(Some("Filter services"));
        filter.set_hexpand(true);
        filter.connect_search_changed({
            let send = send(Msg::Filter);
            move |_| send()
        });
        bar.append(&filter);
        let failed_only = gtk::CheckButton::with_label("Failed only");
        failed_only.connect_toggled({
            let send = send(Msg::Filter);
            move |_| send()
        });
        bar.append(&failed_only);
        units_box.append(&bar);
        let units_list = gtk::ListBox::new();
        units_list.add_css_class("unit-list");
        units_list.set_selection_mode(gtk::SelectionMode::None);
        units_box.append(&units_list);
        content.append(&units_box);

        let containers = AppsSection::new();
        content.append(&containers.section);
        let pm2 = AppsSection::new();
        content.append(&pm2.section);

        let scroller = gtk::ScrolledWindow::builder()
            .child(&content)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();

        let session = SessionPage::new(&scroller, send(Msg::Session(session::Msg::Reconnect)));
        page.append(session.widget());

        Monitor {
            node,
            page,
            session,
            summary,
            cpu,
            cpu_label,
            net,
            net_label,
            memory,
            swap,
            disks,
            disk_rows: Vec::new(),
            units_label,
            filter,
            failed_only,
            units_list,
            unit_rows: Vec::new(),
            systemd: false,
            units: None,
            containers,
            pm2,
            background,
            quiet,
            ranges,
            range_buttons,
            range: None,
            trend_note,
            trend_read: None,
            alerts,
        }
    }

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

    pub fn sample(&mut self, sample: &Sample, core: &Client, sender: &ComponentSender<App>) {
        self.session.show_content();
        if let Some(range) = self.range
            && self
                .trend_read
                .is_none_or(|read| read.elapsed() >= TREND_REFRESH)
        {
            self.read_trend(range, core, sender);
        }
        self.summary.set_label(&format!(
            "Up {} · load {:.2} {:.2} {:.2} · {} cores",
            duration(sample.uptime),
            sample.load[0],
            sample.load[1],
            sample.load[2],
            sample.cpu.cores,
        ));

        self.cpu.push(&[sample.cpu.percent]);
        self.cpu.set_max(100.0);
        self.scale_net();
        self.cpu_label
            .set_label(&format!("CPU {:.0}%", sample.cpu.percent));

        let rx: f64 = sample.net.iter().map(|i| i.rx).sum();
        let tx: f64 = sample.net.iter().map(|i| i.tx).sum();
        self.net.push(&[rx, tx]);
        let scale = self.scale_net();
        self.net_label.set_label(&format!(
            "Network: in {}/s, out {}/s (scale {}/s)",
            size(rx as u64),
            size(tx as u64),
            size(scale as u64)
        ));

        let memory = &sample.memory;
        self.memory
            .set(memory.total - memory.available, memory.total);
        self.swap
            .set(memory.swap_total - memory.swap_free, memory.swap_total);
        self.swap.row.set_visible(memory.swap_total > 0);

        let mounts: Vec<&str> = sample.disks.iter().map(|d| d.mount.as_str()).collect();
        if mounts
            != self
                .disk_rows
                .iter()
                .map(|(m, _)| m.as_str())
                .collect::<Vec<_>>()
        {
            while let Some(child) = self.disks.first_child() {
                self.disks.remove(&child);
            }
            self.disk_rows.clear();
            for (row, disk) in sample.disks.iter().enumerate() {
                let meter = Meter::new();
                let name = label(&printable(&disk.mount));
                name.set_tooltip_text(Some(&printable(&disk.fs)));
                let row = row as i32;
                self.disks.attach(&name, 0, row, 1, 1);
                self.disks.attach(&meter.bar, 1, row, 1, 1);
                self.disks.attach(&meter.text, 2, row, 1, 1);
                self.disk_rows.push((disk.mount.clone(), meter));
            }
        }
        for ((_, meter), disk) in self.disk_rows.iter().zip(&sample.disks) {
            meter.set(disk.total - disk.available, disk.total);
        }
    }

    /// Scales the network chart to what it shows, rounded up to a size
    /// that reads well so the scale doesn't creep, and returns the scale.
    fn scale_net(&self) -> f64 {
        let peak = self.net.peak().max(1024.0);
        let scale = 2f64.powf(peak.log2().ceil());
        self.net.set_max(scale);
        scale
    }

    /// Asks the core for the history on disk over `range`. The reply
    /// arrives as [`Msg::Trend`].
    fn read_trend(&mut self, range: TrendRange, core: &Client, sender: &ComponentSender<App>) {
        self.trend_read = Some(Instant::now());
        let (core, node, input) = (core.clone(), self.node, sender.input_sender().clone());
        runtime().spawn(async move {
            if let Ok(Response::Trend { points }) =
                core.execute(Command::MonitorTrend { node, range }).await
            {
                let msg = Msg::Trend(range, points);
                let _ = input.send(AppMsg::Monitor { node, msg });
            }
        });
    }

    fn show_trend(&mut self, range: TrendRange, points: &[Sample]) {
        // The range's whole length, so a short history fills in from the
        // right as the live charts do. Gaps while Burrow was off close up.
        let width = match range {
            TrendRange::Hour => 60,
            TrendRange::Day => 24 * 60,
            TrendRange::Week => 7 * 24 * 12,
        };
        let cpu = points.iter().map(|p| p.cpu.percent).collect();
        let rx = points
            .iter()
            .map(|p| p.net.iter().map(|i| i.rx).sum())
            .collect();
        let tx = points
            .iter()
            .map(|p| p.net.iter().map(|i| i.tx).sum())
            .collect();
        self.cpu.show(Some((vec![cpu], width)));
        self.net.show(Some((vec![rx, tx], width)));
        self.scale_net();
        self.trend_note.set_label(if points.len() < 2 {
            "Not enough history yet. Burrow keeps a point a minute while it monitors this host."
        } else {
            "Averaged history from this computer. The figures above are live."
        });
        self.trend_note.set_visible(true);
    }

    /// Shows the containers and PM2 apps. A section stays hidden while the
    /// server has neither Docker nor Podman, or no PM2 running.
    pub fn set_apps(&self, apps: &Apps) {
        match &apps.containers {
            Some(containers) => {
                let rows: Vec<AppRow> = (containers.list.iter())
                    .map(|c| AppRow {
                        name: c.name.clone(),
                        state: c.status.clone(),
                        detail: c.image.clone(),
                        failed: c.failed(),
                        running: c.state == "running",
                    })
                    .collect();
                let runtime = match containers.runtime.as_str() {
                    "podman" => "Podman",
                    _ => "Docker",
                };
                let heading = format!("Containers ({runtime})");
                self.containers.show(
                    &heading,
                    &rows,
                    containers.error.as_deref(),
                    "No containers.",
                );
            }
            None => self.containers.section.set_visible(false),
        }
        match &apps.pm2 {
            Some(pm2) => {
                let rows: Vec<AppRow> = (pm2.apps.iter())
                    .map(|app| AppRow {
                        name: app.name.clone(),
                        state: app.status.clone(),
                        detail: format!(
                            "{:.0}% CPU, {}, restarted {} times",
                            app.cpu,
                            size(app.memory),
                            app.restarts
                        ),
                        failed: app.failed(),
                        running: app.status == "online",
                    })
                    .collect();
                self.pm2
                    .show("PM2 apps", &rows, pm2.error.as_deref(), "PM2 runs no apps.");
            }
            None => self.pm2.section.set_visible(false),
        }
    }

    pub fn set_units(&mut self, units: Vec<Unit>) {
        self.systemd = true;
        self.units = Some(units);
        self.fill_units();
    }

    /// Shows the services that pass the filter. Rows that were already
    /// there are kept and updated, so the scroll position and the focus
    /// survive a new list.
    fn fill_units(&mut self) {
        let text = self.filter.text().to_lowercase();
        let failed_only = self.failed_only.is_active();
        self.units_label.set_label(&match &self.units {
            None => "Services".into(),
            Some(units) => {
                let failed = units.iter().filter(|u| u.active == "failed").count();
                match failed {
                    0 => format!("Services: {}", units.len()),
                    n => format!("Services: {}, {n} failed", units.len()),
                }
            }
        });
        let entries = entries(self.units.as_deref(), self.systemd, &text, failed_only);

        let old: Vec<&RowKey> = self.unit_rows.iter().map(|r| &r.key).collect();
        let new: Vec<&RowKey> = entries.iter().map(|(key, _)| key).collect();
        let keep = keep(&old, &new);
        let focused = self
            .unit_rows
            .iter()
            .position(|r| r.row.has_focus())
            .map(|i| (i, self.unit_rows[i].key.clone()));

        // Rows that move or go are taken out first. Those that stay are
        // then in the new order, and the rest go in around them.
        let mut kept: HashMap<RowKey, UnitRow> = HashMap::new();
        let mut moving: HashMap<RowKey, UnitRow> = HashMap::new();
        for (row, keep) in std::mem::take(&mut self.unit_rows).into_iter().zip(keep) {
            if keep {
                kept.insert(row.key.clone(), row);
            } else {
                self.units_list.remove(&row.row);
                moving.insert(row.key.clone(), row);
            }
        }
        for (index, (key, entry)) in entries.into_iter().enumerate() {
            let row = match kept.remove(&key) {
                Some(row) => row,
                None => {
                    let row = moving
                        .remove(&key)
                        .unwrap_or_else(|| UnitRow::new(key, &entry));
                    self.units_list.insert(&row.row, index as i32);
                    row
                }
            };
            row.show(&entry);
            self.unit_rows.push(row);
        }

        // A focused row that moved gets the focus back. One that went
        // passes it to the row that took its place.
        if let Some((index, key)) = focused {
            let row = self
                .unit_rows
                .iter()
                .find(|r| r.key == key)
                .or_else(|| self.unit_rows.get(index))
                .or(self.unit_rows.last());
            if let Some(row) = row
                && !row.row.has_focus()
            {
                row.row.grab_focus();
            }
        }
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

/// A list of containers or PM2 apps. They are few and their labels can't
/// take the focus, so the list is drawn again when it changes, failed ones
/// first.
struct AppsSection {
    section: gtk::Box,
    heading: gtk::Label,
    grid: gtk::Grid,
    /// What is drawn, so an unchanged list isn't drawn again.
    shown: RefCell<Option<(Vec<AppRow>, Option<String>)>>,
}

#[derive(Clone, Debug, PartialEq)]
struct AppRow {
    name: String,
    state: String,
    detail: String,
    failed: bool,
    running: bool,
}

impl AppsSection {
    fn new() -> AppsSection {
        let section = gtk::Box::new(gtk::Orientation::Vertical, 6);
        section.set_visible(false);
        let heading = heading("");
        let grid = gtk::Grid::new();
        grid.set_column_spacing(12);
        grid.set_row_spacing(4);
        section.append(&heading);
        section.append(&grid);
        AppsSection {
            section,
            heading,
            grid,
            shown: RefCell::new(None),
        }
    }

    fn show(&self, title: &str, rows: &[AppRow], error: Option<&str>, empty: &str) {
        self.section.set_visible(true);
        let failed = rows.iter().filter(|row| row.failed).count();
        self.heading.set_label(&match failed {
            0 => format!("{title}: {}", rows.len()),
            n => format!("{title}: {}, {n} failed", rows.len()),
        });
        let mut rows = rows.to_vec();
        rows.sort_by(|a, b| {
            (!a.failed, !a.running, &a.name).cmp(&(!b.failed, !b.running, &b.name))
        });
        let now = Some((rows, error.map(str::to_string)));
        if *self.shown.borrow() == now {
            return;
        }
        while let Some(child) = self.grid.first_child() {
            self.grid.remove(&child);
        }
        let (rows, error) = now.as_ref().unwrap();
        let mut line = 0;
        if let Some(error) = error {
            let text = label(&printable(error));
            text.add_css_class("error-text");
            text.set_wrap(true);
            self.grid.attach(&text, 0, line, 3, 1);
            line += 1;
        } else if rows.is_empty() {
            let text = label(empty);
            text.add_css_class("dim");
            self.grid.attach(&text, 0, line, 3, 1);
        }
        for row in rows.iter().take(MAX_ROWS) {
            let name = label(&printable(&row.name));
            name.set_width_chars(32);
            name.set_max_width_chars(32);
            name.set_ellipsize(gtk::pango::EllipsizeMode::End);
            let state = label(&printable(&row.state));
            state.set_width_chars(20);
            state.set_ellipsize(gtk::pango::EllipsizeMode::End);
            let detail = label(&printable(&row.detail));
            detail.add_css_class("dim");
            detail.set_hexpand(true);
            detail.set_ellipsize(gtk::pango::EllipsizeMode::End);
            if row.failed {
                name.add_css_class("error-text");
                state.add_css_class("error-text");
            } else if row.running {
                state.add_css_class("running");
            } else {
                state.add_css_class("dim");
            }
            self.grid.attach(&name, 0, line, 1, 1);
            self.grid.attach(&state, 1, line, 1, 1);
            self.grid.attach(&detail, 2, line, 1, 1);
            line += 1;
        }
        if rows.len() > MAX_ROWS {
            let more = label(&format!("{} more.", rows.len() - MAX_ROWS));
            more.add_css_class("dim");
            self.grid.attach(&more, 0, line, 3, 1);
        }
        self.shown.replace(now);
    }
}

/// What a row of the services list stands for, so a new list finds the
/// row that showed it before.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum RowKey {
    /// A service, and which of the services with that name it is. Names
    /// come from the server, so they may repeat.
    Unit(String, usize),
    NoSystemd,
    NoneShown,
    More,
}

/// What a row of the services list shows.
#[derive(Debug, PartialEq)]
enum Entry<'a> {
    Unit(&'a Unit),
    /// A note, and whether it is dimmed.
    Text(String, bool),
}

/// The rows of the services list: the services that pass the filter,
/// failed first, then running, then the rest, each by name, or a note
/// saying why there are none.
fn entries<'a>(
    units: Option<&'a [Unit]>,
    systemd: bool,
    text: &str,
    failed_only: bool,
) -> Vec<(RowKey, Entry<'a>)> {
    let Some(units) = units else {
        if systemd {
            return Vec::new();
        }
        let note = Entry::Text("This server doesn't run systemd.".into(), false);
        return vec![(RowKey::NoSystemd, note)];
    };
    let mut shown: Vec<&Unit> = units
        .iter()
        .filter(|u| !failed_only || u.active == "failed")
        .filter(|u| {
            text.is_empty()
                || u.name.to_lowercase().contains(text)
                || u.description.to_lowercase().contains(text)
        })
        .collect();
    shown.sort_by_key(|u| (u.active != "failed", u.active != "active", &u.name));
    if shown.is_empty() {
        let note = if failed_only && text.is_empty() {
            "No service has failed."
        } else {
            "No service matches."
        };
        return vec![(RowKey::NoneShown, Entry::Text(note.into(), true))];
    }
    let mut seen: HashMap<&str, usize> = HashMap::new();
    let mut rows: Vec<(RowKey, Entry)> = shown
        .iter()
        .take(MAX_ROWS)
        .map(|unit| {
            let nth = seen.entry(&unit.name).or_default();
            *nth += 1;
            (RowKey::Unit(unit.name.clone(), *nth - 1), Entry::Unit(unit))
        })
        .collect();
    if shown.len() > MAX_ROWS {
        let more = format!("{} more. Filter to see them.", shown.len() - MAX_ROWS);
        rows.push((RowKey::More, Entry::Text(more, true)));
    }
    rows
}

/// Which rows of `old` can stay where they are when the list becomes
/// `new`: as many as possible whose order doesn't change. The others are
/// taken out and put back in their new place. Keys must not repeat.
fn keep<K: Eq + Hash>(old: &[K], new: &[K]) -> Vec<bool> {
    let at: HashMap<&K, usize> = old.iter().enumerate().map(|(i, k)| (k, i)).collect();
    // The old places of the rows that are still there, in the new order.
    // The longest increasing run through them is what can stay.
    let places: Vec<usize> = new.iter().filter_map(|k| at.get(k).copied()).collect();
    // `tails[n]` is the index into `places` that ends the best run of
    // length n + 1 found so far, and `before` links each to the one
    // ahead of it.
    let mut tails: Vec<usize> = Vec::new();
    let mut before: Vec<Option<usize>> = vec![None; places.len()];
    for (i, &place) in places.iter().enumerate() {
        let n = tails.partition_point(|&t| places[t] < place);
        before[i] = n.checked_sub(1).map(|n| tails[n]);
        if n == tails.len() {
            tails.push(i);
        } else {
            tails[n] = i;
        }
    }
    let mut keep = vec![false; old.len()];
    let mut next = tails.last().copied();
    while let Some(i) = next {
        keep[places[i]] = true;
        next = before[i];
    }
    keep
}

/// A row of the services list, and the labels in it.
struct UnitRow {
    key: RowKey,
    row: gtk::ListBoxRow,
    view: RowView,
}

enum RowView {
    Unit {
        name: gtk::Label,
        state: gtk::Label,
        description: gtk::Label,
        /// What the labels show, so they are only set when it changes.
        shown: RefCell<Option<Unit>>,
    },
    Text(gtk::Label),
}

impl UnitRow {
    fn new(key: RowKey, entry: &Entry) -> UnitRow {
        let row = gtk::ListBoxRow::new();
        let view = match entry {
            Entry::Unit(_) => {
                let line = gtk::Box::new(gtk::Orientation::Horizontal, 12);
                let name = label("");
                name.set_width_chars(32);
                name.set_ellipsize(gtk::pango::EllipsizeMode::End);
                let state = label("");
                state.set_width_chars(20);
                let description = label("");
                description.add_css_class("dim");
                description.set_ellipsize(gtk::pango::EllipsizeMode::End);
                description.set_hexpand(true);
                line.append(&name);
                line.append(&state);
                line.append(&description);
                row.set_child(Some(&line));
                RowView::Unit {
                    name,
                    state,
                    description,
                    shown: RefCell::new(None),
                }
            }
            Entry::Text(..) => {
                let text = label("");
                row.set_child(Some(&text));
                RowView::Text(text)
            }
        };
        UnitRow { key, row, view }
    }

    /// Shows `entry`, which has this row's key, so is of the same kind.
    fn show(&self, entry: &Entry) {
        match (&self.view, entry) {
            (
                RowView::Unit {
                    name,
                    state,
                    description,
                    shown,
                },
                Entry::Unit(unit),
            ) => {
                if shown.borrow().as_ref() == Some(*unit) {
                    return;
                }
                name.set_label(&printable(&unit.name));
                state.set_label(&printable(&format!("{} ({})", unit.active, unit.sub)));
                description.set_label(&printable(&unit.description));
                for class in ["error-text", "running", "dim"] {
                    name.remove_css_class(class);
                    state.remove_css_class(class);
                }
                if unit.active == "failed" {
                    name.add_css_class("error-text");
                    state.add_css_class("error-text");
                } else if unit.active == "active" {
                    state.add_css_class("running");
                } else {
                    state.add_css_class("dim");
                }
                shown.replace(Some((*unit).clone()));
            }
            (RowView::Text(label), Entry::Text(text, dim)) => {
                if label.label() != *text {
                    label.set_label(text);
                }
                if *dim {
                    label.add_css_class("dim");
                } else {
                    label.remove_css_class("dim");
                }
            }
            _ => {}
        }
    }
}

fn label(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label
}

fn heading(text: &str) -> gtk::Label {
    let label = label(text);
    label.add_css_class("heading");
    label
}

fn section(title: &gtk::Label, body: &impl IsA<gtk::Widget>) -> gtk::Box {
    let section = gtk::Box::new(gtk::Orientation::Vertical, 6);
    section.append(title);
    section.append(body);
    section
}

/// A bar with "used of total" next to it.
struct Meter {
    bar: gtk::LevelBar,
    text: gtk::Label,
    /// Both, for meters that stand alone.
    row: gtk::Box,
}

impl Meter {
    fn new() -> Meter {
        let bar = gtk::LevelBar::new();
        bar.set_hexpand(true);
        bar.set_valign(gtk::Align::Center);
        // Full is the warning, so the theme's "high" colour isn't used for
        // a disk that is merely busy.
        bar.remove_offset_value(Some(gtk::LEVEL_BAR_OFFSET_HIGH));
        bar.add_offset_value(gtk::LEVEL_BAR_OFFSET_LOW, 0.9);
        bar.add_offset_value(gtk::LEVEL_BAR_OFFSET_HIGH, 0.9);
        bar.add_offset_value(gtk::LEVEL_BAR_OFFSET_FULL, 1.0);
        let text = label("");
        text.set_width_chars(24);
        Meter {
            bar,
            text,
            row: gtk::Box::new(gtk::Orientation::Horizontal, 12),
        }
    }

    /// The bar and text with `name` before them, in [`Meter::row`].
    fn row(&self, name: &str) -> gtk::Box {
        let name = label(name);
        name.set_width_chars(8);
        self.row.append(&name);
        self.row.append(&self.bar);
        self.row.append(&self.text);
        self.row.clone()
    }

    fn set(&self, used: u64, total: u64) {
        let share = if total == 0 {
            0.0
        } else {
            used as f64 / total as f64
        };
        self.bar.set_value(share);
        self.text.set_label(&format!(
            "{} of {} ({:.0}%)",
            size(used),
            size(total),
            share * 100.0
        ));
    }
}

/// A line chart of the last [`CHART_POINTS`] values of one or more series.
struct Chart {
    area: gtk::DrawingArea,
    data: Rc<RefCell<ChartData>>,
}

struct ChartData {
    series: Vec<VecDeque<f64>>,
    /// Drawn instead of `series` while set: the history on disk, and how
    /// many points the width stands for.
    fixed: Option<(Vec<Vec<f64>>, usize)>,
    colors: Vec<gtk::gdk::RGBA>,
    max: f64,
}

impl ChartData {
    /// What is drawn, and how many points the width stands for.
    fn shown(&self) -> (Vec<Vec<f64>>, usize) {
        match &self.fixed {
            Some((fixed, points)) => (fixed.clone(), (*points).max(2)),
            None => (
                self.series
                    .iter()
                    .map(|s| s.iter().copied().collect())
                    .collect(),
                CHART_POINTS,
            ),
        }
    }
}

impl Chart {
    fn new(colors: &[&str]) -> Chart {
        let data = Rc::new(RefCell::new(ChartData {
            series: vec![VecDeque::new(); colors.len()],
            fixed: None,
            colors: colors
                .iter()
                .map(|c| gtk::gdk::RGBA::parse(*c).expect("a valid colour"))
                .collect(),
            max: 1.0,
        }));
        let area = gtk::DrawingArea::new();
        area.set_content_height(120);
        area.set_hexpand(true);
        area.set_draw_func({
            let data = data.clone();
            move |area, cr, width, height| draw(area, cr, width, height, &data.borrow())
        });
        Chart { area, data }
    }

    fn push(&self, values: &[f64]) {
        let mut data = self.data.borrow_mut();
        for (series, value) in data.series.iter_mut().zip(values) {
            if series.len() == CHART_POINTS {
                series.pop_front();
            }
            series.push_back(*value);
        }
    }

    /// Draws `fixed` instead of the live values, as the newest of `points`
    /// across the width, or the live values again.
    fn show(&self, fixed: Option<(Vec<Vec<f64>>, usize)>) {
        self.data.borrow_mut().fixed = fixed;
        self.area.queue_draw();
    }

    /// The highest value shown.
    fn peak(&self) -> f64 {
        let (series, _) = self.data.borrow().shown();
        series.iter().flatten().copied().fold(0.0, f64::max)
    }

    fn set_max(&self, max: f64) {
        self.data.borrow_mut().max = max;
        self.area.queue_draw();
    }
}

fn draw(
    area: &gtk::DrawingArea,
    cr: &gtk::cairo::Context,
    width: i32,
    height: i32,
    data: &ChartData,
) {
    let (width, height) = (f64::from(width), f64::from(height));
    let ink = area.color();
    // A faint frame and quarter lines, in the text colour so they suit
    // light and dark themes.
    cr.set_source_rgba(
        ink.red().into(),
        ink.green().into(),
        ink.blue().into(),
        0.12,
    );
    cr.set_line_width(1.0);
    for quarter in 0..=4 {
        let y = (height - 1.0) * f64::from(quarter) / 4.0 + 0.5;
        cr.move_to(0.0, y);
        cr.line_to(width, y);
    }
    let _ = cr.stroke();

    let (shown, points) = data.shown();
    let step = width / (points - 1) as f64;
    for (series, color) in shown.iter().zip(&data.colors) {
        // The newest point is at the right edge.
        let offset = points.saturating_sub(series.len());
        for (i, value) in series.iter().enumerate() {
            let x = (offset + i) as f64 * step;
            let y = height - (value / data.max).clamp(0.0, 1.0) * (height - 2.0) - 1.0;
            if i == 0 {
                cr.move_to(x, y);
            } else {
                cr.line_to(x, y);
            }
        }
        cr.set_source_rgba(
            color.red().into(),
            color.green().into(),
            color.blue().into(),
            1.0,
        );
        cr.set_line_width(1.5);
        let _ = cr.stroke();
    }
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

/// "3 days 4 h", "5 h 12 min" or "7 min".
fn duration(seconds: f64) -> String {
    let minutes = (seconds / 60.0) as u64;
    let (days, hours, minutes) = (minutes / 1440, minutes / 60 % 24, minutes % 60);
    match (days, hours) {
        (0, 0) => format!("{minutes} min"),
        (0, _) => format!("{hours} h {minutes} min"),
        (1, _) => format!("1 day {hours} h"),
        _ => format!("{days} days {hours} h"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(name: &str, active: &str) -> Unit {
        Unit {
            name: name.into(),
            active: active.into(),
            ..Default::default()
        }
    }

    /// Checks `keep` against what the list ends up as: the rows kept are
    /// in the new order, and `kept` of them stay.
    fn check_keep(old: &[&str], new: &[&str], kept: usize) {
        let keep = keep(old, new);
        assert_eq!(keep.len(), old.len());
        let stay: Vec<&str> = old
            .iter()
            .zip(&keep)
            .filter(|(_, k)| **k)
            .map(|(o, _)| *o)
            .collect();
        assert_eq!(stay.len(), kept, "{old:?} to {new:?} keeps {stay:?}");
        let order: Vec<&str> = new.iter().copied().filter(|n| stay.contains(n)).collect();
        assert_eq!(stay, order);
    }

    #[test]
    fn rows_that_keep_their_order_stay() {
        check_keep(&["a", "b", "c"], &["a", "b", "c"], 3);
        check_keep(&["a", "b", "c"], &["a", "c"], 2);
        check_keep(&["a", "c"], &["a", "b", "c"], 2);
        check_keep(&[], &["a", "b"], 0);
        check_keep(&["a", "b"], &[], 0);
        // A service that fails moves to the top, and one that recovers
        // moves down. Only that row moves, not those it passes.
        check_keep(&["a", "b", "c", "d"], &["d", "a", "b", "c"], 3);
        check_keep(&["d", "a", "b", "c"], &["a", "b", "c", "d"], 3);
        check_keep(&["a", "b", "c", "d"], &["d", "c", "b", "a"], 1);
        check_keep(&["a", "b", "c", "d", "e"], &["b", "x", "a", "d", "e"], 3);
    }

    #[test]
    fn services_list_failed_then_running_then_the_rest() {
        let units = [
            unit("b.service", "inactive"),
            unit("c.service", "active"),
            unit("a.service", "active"),
            unit("d.service", "failed"),
        ];
        let keys: Vec<RowKey> = entries(Some(&units), true, "", false)
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        let key = |name: &str| RowKey::Unit(name.into(), 0);
        assert_eq!(
            keys,
            [
                key("d.service"),
                key("a.service"),
                key("c.service"),
                key("b.service")
            ]
        );
    }

    #[test]
    fn services_with_the_same_name_get_their_own_rows() {
        let units = [unit("a.service", "active"), unit("a.service", "active")];
        let keys: Vec<RowKey> = entries(Some(&units), true, "", false)
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        assert_eq!(
            keys,
            [
                RowKey::Unit("a.service".into(), 0),
                RowKey::Unit("a.service".into(), 1)
            ]
        );
    }

    #[test]
    fn services_list_says_why_it_is_empty() {
        assert_eq!(
            entries(None, false, "", false),
            [(
                RowKey::NoSystemd,
                Entry::Text("This server doesn't run systemd.".into(), false)
            )]
        );
        assert!(entries(None, true, "", false).is_empty());
        let units = [unit("a.service", "active")];
        assert_eq!(
            entries(Some(&units), true, "", true),
            [(
                RowKey::NoneShown,
                Entry::Text("No service has failed.".into(), true)
            )]
        );
        assert_eq!(
            entries(Some(&units), true, "nginx", false),
            [(
                RowKey::NoneShown,
                Entry::Text("No service matches.".into(), true)
            )]
        );
    }

    #[test]
    fn services_past_the_limit_are_counted() {
        let units: Vec<Unit> = (0..MAX_ROWS + 3)
            .map(|i| unit(&format!("u{i:05}.service"), "active"))
            .collect();
        let rows = entries(Some(&units), true, "", false);
        assert_eq!(rows.len(), MAX_ROWS + 1);
        assert_eq!(
            rows.last(),
            Some(&(
                RowKey::More,
                Entry::Text("3 more. Filter to see them.".into(), true)
            ))
        );
    }

    #[test]
    fn durations_read_well() {
        assert_eq!(duration(59.0), "0 min");
        assert_eq!(duration(7.0 * 60.0), "7 min");
        assert_eq!(duration(5.0 * 3600.0 + 12.0 * 60.0), "5 h 12 min");
        assert_eq!(duration(86400.0 + 4.0 * 3600.0), "1 day 4 h");
        assert_eq!(duration(3.0 * 86400.0), "3 days 0 h");
    }
}
