//! The containers and PM2 apps section.

use std::cell::RefCell;

use burrow_core::Service;
use burrow_core::monitor::Apps;
use relm4::gtk;
use relm4::gtk::prelude::*;

use crate::files::{printable, size};

use super::Monitor;
use super::entries::MAX_ROWS;
use super::page::{heading, label};
use super::service_menu::{Menu, menu_button};

impl Monitor {
    /// Shows the containers and PM2 apps. A section stays hidden while the
    /// server has neither Docker nor Podman, or no PM2 running.
    pub fn set_apps(&self, apps: &Apps) {
        match &apps.containers {
            Some(containers) => {
                let rows: Vec<AppRow> = (containers.list.iter())
                    .map(|c| AppRow {
                        service: Service::Container {
                            runtime: containers.runtime.clone(),
                            name: c.name.clone(),
                        },
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
                let (error, empty) = (containers.error.as_deref(), "No containers.");
                self.containers
                    .show(&heading, &rows, error, empty, &self.menu);
            }
            None => self.containers.section.set_visible(false),
        }
        match &apps.pm2 {
            Some(pm2) => {
                let rows: Vec<AppRow> = (pm2.apps.iter())
                    .map(|app| AppRow {
                        service: Service::Pm2App {
                            pm2: pm2.path.clone(),
                            name: app.name.clone(),
                        },
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
                let (error, empty) = (pm2.error.as_deref(), "PM2 runs no apps.");
                self.pm2.show("PM2 apps", &rows, error, empty, &self.menu);
            }
            None => self.pm2.section.set_visible(false),
        }
    }
}

/// A list of containers or PM2 apps. They are few and their labels can't
/// take the focus, so the list is drawn again when it changes, failed ones
/// first.
pub(super) struct AppsSection {
    pub(super) section: gtk::Box,
    pub(super) heading: gtk::Label,
    pub(super) grid: gtk::Grid,
    /// What is drawn, so an unchanged list isn't drawn again.
    pub(super) shown: RefCell<Option<(Vec<AppRow>, Option<String>)>>,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct AppRow {
    pub(super) service: Service,
    pub(super) name: String,
    pub(super) state: String,
    pub(super) detail: String,
    pub(super) failed: bool,
    pub(super) running: bool,
}

impl AppsSection {
    pub(super) fn new() -> AppsSection {
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

    pub(super) fn show(
        &self,
        title: &str,
        rows: &[AppRow],
        error: Option<&str>,
        empty: &str,
        menu: &Menu,
    ) {
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
            self.grid.attach(&text, 0, line, 4, 1);
            line += 1;
        } else if rows.is_empty() {
            let text = label(empty);
            text.add_css_class("dim");
            self.grid.attach(&text, 0, line, 4, 1);
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
            let button = menu_button(menu, row.service.clone());
            self.grid.attach(&button, 3, line, 1, 1);
            line += 1;
        }
        if rows.len() > MAX_ROWS {
            let more = label(&format!("{} more.", rows.len() - MAX_ROWS));
            more.add_css_class("dim");
            self.grid.attach(&more, 0, line, 4, 1);
        }
        self.shown.replace(now);
    }
}
