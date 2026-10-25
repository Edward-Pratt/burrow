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
use std::collections::VecDeque;
use std::rc::Rc;
use std::time::{Duration, Instant};

use burrow_core::ipc::Client;
use burrow_core::monitor::{Sample, Unit};
use burrow_core::{
    Alert, AlertKind, Command, MonitorHistory, NodeId, Response, SessionId, SessionState,
    TrendRange,
};
use relm4::gtk::prelude::*;
use relm4::{ComponentSender, gtk};

use crate::app::{App, AppMsg};
use crate::files::{printable, size};
use crate::remote::State;
use crate::runtime::runtime;

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
    /// The core accepted `open_monitor`.
    Started(SessionId),
    /// `open_monitor` itself failed.
    StartFailed(String),
    /// What the monitor had seen before the tab joined it, which a
    /// background monitor has: whether systemd runs there, the latest
    /// services, and samples for the charts.
    History(MonitorHistory),
    /// The service filter or "failed only" changed.
    Filter,
    Reconnect,
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
    pub state: State,
    stack: gtk::Stack,
    message: gtk::Label,
    reconnect: gtk::Button,
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
    systemd: bool,
    units: Option<Vec<Unit>>,
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

        let scroller = gtk::ScrolledWindow::builder()
            .child(&content)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();

        let message = gtk::Label::new(Some("Connecting…"));
        message.set_wrap(true);
        message.set_max_width_chars(60);
        message.set_selectable(true);
        let reconnect = gtk::Button::with_label("Reconnect");
        reconnect.connect_clicked({
            let send = send(Msg::Reconnect);
            move |_| send()
        });
        reconnect.set_halign(gtk::Align::Center);
        reconnect.set_visible(false);
        let message_page = gtk::Box::new(gtk::Orientation::Vertical, 12);
        message_page.set_valign(gtk::Align::Center);
        message_page.append(&message);
        message_page.append(&reconnect);

        let stack = gtk::Stack::new();
        stack.add_named(&scroller, Some("content"));
        stack.add_named(&message_page, Some("message"));
        stack.set_visible_child_name("message");
        page.append(&stack);

        Monitor {
            node,
            page,
            state: State::Starting,
            stack,
            message,
            reconnect,
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
            systemd: false,
            units: None,
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

    fn show_message(&self, text: &str, can_reconnect: bool) {
        self.message.set_label(text);
        self.reconnect.set_visible(can_reconnect);
        self.stack.set_visible_child_name("message");
    }

    pub fn session_changed(
        &mut self,
        state: &SessionState,
        core: &Client,
        sender: &ComponentSender<App>,
    ) {
        match state {
            SessionState::Connecting { step } => self.show_message(&format!("{step}…"), false),
            SessionState::Connected => {
                self.show_message("Waiting for the first sample…", false);
                self.fetch_history(core, sender);
            }
            SessionState::Closed { error } => {
                self.state = State::Closed;
                let text = match error {
                    Some(error) => format!("The monitor stopped: {}", printable(error)),
                    None => "The monitor stopped.".into(),
                };
                self.show_message(&text, true);
            }
        }
    }

    /// Asks what the monitor has seen. Whether systemd runs there is only
    /// in the history, and so are the services of a monitor that was
    /// running before the tab joined it.
    pub fn fetch_history(&self, core: &Client, sender: &ComponentSender<App>) {
        let State::Running(session) = self.state else {
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
        self.stack.set_visible_child_name("content");
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

    pub fn set_units(&mut self, units: Vec<Unit>) {
        self.systemd = true;
        self.units = Some(units);
        self.fill_units();
    }

    fn fill_units(&self) {
        while let Some(row) = self.units_list.row_at_index(0) {
            self.units_list.remove(&row);
        }
        let Some(units) = &self.units else {
            self.units_label.set_label("Services");
            if !self.systemd {
                self.units_list
                    .append(&label("This server doesn't run systemd."));
            }
            return;
        };
        let failed = units.iter().filter(|u| u.active == "failed").count();
        self.units_label.set_label(&match failed {
            0 => format!("Services: {}", units.len()),
            n => format!("Services: {}, {n} failed", units.len()),
        });
        let text = self.filter.text().to_lowercase();
        let failed_only = self.failed_only.is_active();
        let mut shown: Vec<&Unit> = units
            .iter()
            .filter(|u| !failed_only || u.active == "failed")
            .filter(|u| {
                text.is_empty()
                    || u.name.to_lowercase().contains(&text)
                    || u.description.to_lowercase().contains(&text)
            })
            .collect();
        // Failed first, then running, then the rest, each by name.
        shown.sort_by_key(|u| (u.active != "failed", u.active != "active", &u.name));
        if shown.is_empty() {
            let none = label(if failed_only && text.is_empty() {
                "No service has failed."
            } else {
                "No service matches."
            });
            none.add_css_class("dim");
            self.units_list.append(&none);
        }
        for unit in shown.iter().take(MAX_ROWS) {
            self.units_list.append(&unit_row(unit));
        }
        if shown.len() > MAX_ROWS {
            let more = label(&format!(
                "{} more. Filter to see them.",
                shown.len() - MAX_ROWS
            ));
            more.add_css_class("dim");
            self.units_list.append(&more);
        }
    }

    pub fn update(&mut self, msg: Msg, core: &Client, sender: &ComponentSender<App>) {
        match msg {
            // Handled by the window, which knows the other sessions.
            Msg::Started(_) => {}
            Msg::StartFailed(error) => {
                self.state = State::Closed;
                self.show_message(&printable(&error), true);
            }
            Msg::History(history) => {
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
            Msg::Reconnect => {
                if self.state == State::Closed {
                    self.state = State::Starting;
                    self.show_message("Connecting…", false);
                    start(core, self.node, sender);
                }
            }
        }
    }
}

/// Asks the core for a monitor on `node`. The reply arrives as
/// [`Msg::Started`] or [`Msg::StartFailed`].
pub fn start(core: &Client, node: NodeId, sender: &ComponentSender<App>) {
    let (core, input) = (core.clone(), sender.input_sender().clone());
    runtime().spawn(async move {
        let msg = match core.execute(Command::OpenMonitor { node }).await {
            Ok(Response::Session { session }) => Msg::Started(session),
            Ok(other) => Msg::StartFailed(format!("unexpected reply {other:?}")),
            Err(e) => Msg::StartFailed(e.to_string()),
        };
        let _ = input.send(AppMsg::Monitor { node, msg });
    });
}

fn unit_row(unit: &Unit) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let name = label(&printable(&unit.name));
    name.set_width_chars(32);
    name.set_ellipsize(gtk::pango::EllipsizeMode::End);
    let state = label(&printable(&format!("{} ({})", unit.active, unit.sub)));
    state.set_width_chars(20);
    if unit.active == "failed" {
        name.add_css_class("error-text");
        state.add_css_class("error-text");
    } else if unit.active == "active" {
        state.add_css_class("running");
    } else {
        state.add_css_class("dim");
    }
    let description = label(&printable(&unit.description));
    description.add_css_class("dim");
    description.set_ellipsize(gtk::pango::EllipsizeMode::End);
    description.set_hexpand(true);
    row.append(&name);
    row.append(&state);
    row.append(&description);
    row
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

    #[test]
    fn durations_read_well() {
        assert_eq!(duration(59.0), "0 min");
        assert_eq!(duration(7.0 * 60.0), "7 min");
        assert_eq!(duration(5.0 * 3600.0 + 12.0 * 60.0), "5 h 12 min");
        assert_eq!(duration(86400.0 + 4.0 * 3600.0), "1 day 4 h");
        assert_eq!(duration(3.0 * 86400.0), "3 days 0 h");
    }
}
