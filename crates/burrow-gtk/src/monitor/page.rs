//! Building the monitor page: meters, charts, the services and apps lists, and the alerts.

use std::cell::Cell;
use std::rc::Rc;

use burrow_core::NodeId;
use relm4::gtk::prelude::*;
use relm4::{ComponentSender, gtk};

use crate::app::{App, AppMsg};
use crate::session::{self, SessionPage};

use super::apps::AppsSection;
use super::charts::{Chart, Meter};
use super::samples::RANGES;
use super::service_menu::menu_opener;
use super::{Monitor, Msg};

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
        let menu = menu_opener(&page, node, sender);

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
            menu,
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
}

pub(super) fn label(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label
}

pub(super) fn heading(text: &str) -> gtk::Label {
    let label = label(text);
    label.add_css_class("heading");
    label
}

pub(super) fn section(title: &gtk::Label, body: &impl IsA<gtk::Widget>) -> gtk::Box {
    let section = gtk::Box::new(gtk::Orientation::Vertical, 6);
    section.append(title);
    section.append(body);
    section
}
