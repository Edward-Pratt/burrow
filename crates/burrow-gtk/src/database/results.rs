//! Showing a query's result: the grid, its summary and errors, and notes.

use std::time::Duration;

use burrow_core::{QueryError, QueryResult, StatementResult};
use relm4::gtk::glib;
use relm4::gtk::prelude::*;
use relm4::{ComponentSender, gtk};

use crate::app::{App, AppMsg};
use crate::files::printable;

use super::{Database, Msg, Row};

/// The most characters of a value drawn in a grid cell. Labels get slow
/// long before a value reaches the 64 KiB the core keeps.
pub(super) const CELL_CHARS: usize = 300;

/// How long "Saved 3 rows" stays in the status line before the result's
/// own summary comes back.
pub(super) const NOTE_TIME: Duration = Duration::from_secs(6);

impl Database {
    /// Shows a note in the status line for a while.
    pub(super) fn note(&mut self, text: &str, sender: &ComponentSender<App>) {
        self.status.set_label(text);
        self.note_id += 1;
        let (id, node, sender) = (self.note_id, self.node, sender.clone());
        glib::timeout_add_local_once(NOTE_TIME, move || {
            let msg = Msg::ClearNote(id);
            sender.input(AppMsg::Database { node, msg });
        });
    }

    pub(super) fn show_result(
        &mut self,
        result: &QueryResult,
        offset: i32,
        browsing: Option<&str>,
    ) {
        self.note_id += 1;
        self.summary = summary(result, browsing);
        self.status.set_label(&self.summary);
        match &result.error {
            Some(error) => {
                self.show_error(Some(&error_text(error)));
                // Put the cursor where the server says the error is.
                if browsing.is_none()
                    && let Some(position) = error.position
                {
                    let at = offset + i32::try_from(position).unwrap_or(i32::MAX - offset) - 1;
                    let iter = self.buffer.iter_at_offset(at);
                    self.buffer.place_cursor(&iter);
                    self.editor
                        .scroll_to_iter(&mut iter.clone(), 0.1, false, 0.0, 0.0);
                }
            }
            None => self.show_error(None),
        }
        // The last statement that returned rows, or the last one.
        let shown = result
            .statements
            .iter()
            .rev()
            .find(|s| !s.columns.is_empty())
            .or(result.statements.last());
        match shown {
            Some(statement) => self.fill_grid(statement),
            None if result.error.is_none() => self.fill_grid(&StatementResult::default()),
            // A failed query leaves the last rows in place.
            None => {}
        }
    }

    pub(super) fn fill_grid(&self, statement: &StatementResult) {
        while let Some(column) = self
            .grid
            .columns()
            .item(0)
            .and_downcast::<gtk::ColumnViewColumn>()
        {
            self.grid.remove_column(&column);
        }
        self.rows.remove_all();
        for (index, name) in statement.columns.iter().enumerate() {
            let factory = gtk::SignalListItemFactory::new();
            factory.connect_setup(|_, item| {
                let label = gtk::Label::new(None);
                label.set_xalign(0.0);
                label.set_ellipsize(gtk::pango::EllipsizeMode::End);
                label.set_max_width_chars(40);
                label.set_single_line_mode(true);
                if let Some(item) = item.downcast_ref::<gtk::ListItem>() {
                    item.set_child(Some(&label));
                }
            });
            factory.connect_bind(move |_, item| {
                let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
                    return;
                };
                let (Some(label), Some(row)) = (
                    item.child().and_downcast::<gtk::Label>(),
                    item.item().and_downcast::<glib::BoxedAnyObject>(),
                ) else {
                    return;
                };
                let row = row.borrow::<Row>();
                match row.get(index).and_then(Option::as_deref) {
                    Some(value) => {
                        label.set_label(&cell_text(value));
                        label.remove_css_class("null");
                    }
                    None => {
                        label.set_label("NULL");
                        label.add_css_class("null");
                    }
                }
            });
            let column = gtk::ColumnViewColumn::new(Some(&printable(name)), Some(factory));
            column.set_resizable(true);
            self.grid.append_column(&column);
        }
        let rows: Vec<glib::BoxedAnyObject> = statement
            .rows
            .iter()
            .map(|row| glib::BoxedAnyObject::new(row.clone()))
            .collect();
        self.rows.extend_from_slice(&rows);
    }
}

/// "12 rows · 4 ms", and what else the user should know about the result.
pub(super) fn summary(result: &QueryResult, browsing: Option<&str>) -> String {
    let mut parts = Vec::new();
    if let Some(sql) = browsing {
        parts.push(printable(sql));
    }
    if result.statements.len() > 1 {
        parts.push(format!("{} statements", result.statements.len()));
    }
    let shown = result
        .statements
        .iter()
        .rev()
        .find(|s| !s.columns.is_empty())
        .or(result.statements.last());
    if let Some(statement) = shown {
        let rows = |n: usize| {
            if n == 1 {
                "1 row".into()
            } else {
                format!("{n} rows")
            }
        };
        parts.push(if statement.truncated {
            format!("first {} shown", rows(statement.rows.len()))
        } else if statement.columns.is_empty() {
            match statement.count {
                1 => "1 row changed".into(),
                n => format!("{n} rows changed"),
            }
        } else {
            rows(statement.rows.len())
        });
    }
    if result.stopped {
        parts.push("stopped there, and the rest of the query did not run".into());
    }
    if result.error.is_some() {
        parts.push("failed".into());
    }
    parts.push(format!("{} ms", result.elapsed_ms));
    parts.join(" · ")
}

/// The server's error with its detail and hint, each on its own line.
pub(super) fn error_text(error: &QueryError) -> String {
    let mut lines = vec![printable(&error.message)];
    if let Some(detail) = &error.detail {
        lines.push(printable(detail));
    }
    if let Some(hint) = &error.hint {
        lines.push(format!("Hint: {}", printable(hint)));
    }
    lines.join("\n")
}

/// A value for a grid cell: on one line, without control characters, and
/// cut short when long.
pub(super) fn cell_text(value: &str) -> String {
    let mut text: String = value
        .chars()
        .take(CELL_CHARS)
        .map(|c| match c {
            '\n' => '↵',
            '\t' => ' ',
            c if c.is_control() => '\u{fffd}',
            c => c,
        })
        .collect();
    if value.chars().nth(CELL_CHARS).is_some() {
        text.push('…');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cells_are_drawn_on_one_line_without_control_characters() {
        assert_eq!(cell_text("a\nb\tc\u{1b}[2J"), "a↵b c\u{fffd}[2J");
        let long = "x".repeat(CELL_CHARS + 5);
        let cut = cell_text(&long);
        assert_eq!(cut.chars().count(), CELL_CHARS + 1);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn the_summary_says_when_rows_were_left_out() {
        let result = QueryResult {
            statements: vec![StatementResult {
                columns: vec!["n".into()],
                rows: vec![vec![Some("1".into())]; 3],
                count: 0,
                truncated: true,
            }],
            error: None,
            stopped: true,
            elapsed_ms: 7,
        };
        assert_eq!(
            summary(&result, None),
            "first 3 rows shown · stopped there, and the rest of the query did not run · 7 ms"
        );
        let update = QueryResult {
            statements: vec![StatementResult {
                count: 2,
                ..StatementResult::default()
            }],
            elapsed_ms: 1,
            ..QueryResult::default()
        };
        assert_eq!(summary(&update, None), "2 rows changed · 1 ms");
    }
}
