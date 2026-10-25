//! Monitor tabs: an SSH host's CPU, memory, disks, network and systemd
//! services, as burrow-agent reports them through the core.
//!
//! The core already cleaned what the agent sent, but names are passed
//! through [`printable`] anyway, as everything from a server is.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use burrow_core::ipc::Client;
use burrow_core::monitor::{Sample, Unit};
use burrow_core::{Command, NodeId, Response, SessionId, SessionState};
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

#[derive(Clone, Debug)]
pub enum Msg {
    /// The core accepted `open_monitor`.
    Started(SessionId),
    /// `open_monitor` itself failed.
    StartFailed(String),
    /// Whether the server runs systemd, from the monitor's history.
    Systemd(bool),
    /// The service filter or "failed only" changed.
    Filter,
    Reconnect,
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

        let content = gtk::Box::new(gtk::Orientation::Vertical, 18);
        let summary = label("");
        summary.add_css_class("dim");
        content.append(&summary);

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
        }
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

    /// Asks whether systemd runs there, which only the history says.
    fn fetch_history(&self, core: &Client, sender: &ComponentSender<App>) {
        let State::Running(session) = self.state else {
            return;
        };
        let (core, node, input) = (core.clone(), self.node, sender.input_sender().clone());
        runtime().spawn(async move {
            if let Ok(Response::Monitor { history }) =
                core.execute(Command::MonitorHistory { session }).await
            {
                let msg = Msg::Systemd(history.systemd);
                let _ = input.send(AppMsg::Monitor { node, msg });
            }
        });
    }

    pub fn sample(&mut self, sample: &Sample) {
        self.stack.set_visible_child_name("content");
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
        self.cpu_label
            .set_label(&format!("CPU {:.0}%", sample.cpu.percent));

        let rx: f64 = sample.net.iter().map(|i| i.rx).sum();
        let tx: f64 = sample.net.iter().map(|i| i.tx).sum();
        self.net.push(&[rx, tx]);
        // Rounded up to a size that reads well, so the scale doesn't creep.
        let peak = self.net.peak().max(1024.0);
        let scale = 2f64.powf(peak.log2().ceil());
        self.net.set_max(scale);
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
            Msg::Systemd(systemd) => {
                self.systemd = systemd;
                self.fill_units();
            }
            Msg::Filter => self.fill_units(),
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
    colors: Vec<gtk::gdk::RGBA>,
    max: f64,
}

impl Chart {
    fn new(colors: &[&str]) -> Chart {
        let data = Rc::new(RefCell::new(ChartData {
            series: vec![VecDeque::new(); colors.len()],
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

    /// The highest value shown.
    fn peak(&self) -> f64 {
        let data = self.data.borrow();
        data.series.iter().flatten().copied().fold(0.0, f64::max)
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

    let step = width / (CHART_POINTS - 1) as f64;
    for (series, color) in data.series.iter().zip(&data.colors) {
        // The newest point is at the right edge.
        let offset = CHART_POINTS - series.len();
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
