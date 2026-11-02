//! The services list: its rows, and filling them from the units the agent reports.

use std::cell::RefCell;
use std::collections::HashMap;

use burrow_core::Service;
use burrow_core::monitor::Unit;
use relm4::gtk;
use relm4::gtk::prelude::*;

use crate::files::printable;

use super::Monitor;
use super::entries::{Entry, RowKey, entries, keep};
use super::page::label;
use super::service_menu::{Menu, menu_button};

impl Monitor {
    pub fn set_units(&mut self, units: Vec<Unit>) {
        self.systemd = true;
        self.units = Some(units);
        self.fill_units();
    }

    /// Shows the services that pass the filter. Rows that were already
    /// there are kept and updated, so the scroll position and the focus
    /// survive a new list.
    pub(super) fn fill_units(&mut self) {
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
                        .unwrap_or_else(|| UnitRow::new(key, &entry, &self.menu));
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
}

/// A row of the services list, and the labels in it.
pub(super) struct UnitRow {
    pub(super) key: RowKey,
    pub(super) row: gtk::ListBoxRow,
    pub(super) view: RowView,
}

pub(super) enum RowView {
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
    pub(super) fn new(key: RowKey, entry: &Entry, menu: &Menu) -> UnitRow {
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
                // A row stands for one name, so its menu can be made now.
                if let RowKey::Unit(name, _) = &key {
                    let service = Service::Unit { name: name.clone() };
                    line.append(&menu_button(menu, service));
                }
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
    pub(super) fn show(&self, entry: &Entry) {
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
