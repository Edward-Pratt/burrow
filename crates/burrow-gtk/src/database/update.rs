//! The page's messages: running queries, browsing tables and everything the user does on the page.

use burrow_core::ipc::Client;
use burrow_core::{Command, Response};
use relm4::gtk::prelude::*;
use relm4::gtk::{gio, glib};
use relm4::{ComponentSender, gtk};

use crate::app::{App, AppMsg};
use crate::files::printable;
use crate::runtime::runtime;
use crate::session::{self};

use super::schema::quote_ident;
use super::{Database, Item, Msg, Row, start};

impl Database {
    pub fn update(
        &mut self,
        msg: Msg,
        core: &Client,
        window: &gtk::ApplicationWindow,
        sender: &ComponentSender<App>,
    ) {
        let node = self.node;
        match msg {
            // Handled by the window, which knows the other sessions.
            Msg::Session(session::Msg::Started(_)) => {}
            Msg::Session(session::Msg::StartFailed(error)) => self.session.start_failed(&error),
            Msg::Session(session::Msg::Reconnect) => {
                if self.session.reconnect() {
                    self.show_error(None);
                    start(core, node, sender);
                }
            }
            Msg::Run => {
                if self.running {
                    return;
                }
                let (start, end) = self
                    .buffer
                    .selection_bounds()
                    .unwrap_or_else(|| self.buffer.bounds());
                let sql = self.buffer.text(&start, &end, false).to_string();
                if sql.trim().is_empty() {
                    return;
                }
                self.query(sql, start.offset(), None, core, sender);
            }
            Msg::Browse { schema, table } => {
                if self.running {
                    return;
                }
                let sql = format!(
                    "SELECT * FROM {}.{} LIMIT 100",
                    quote_ident(&schema),
                    quote_ident(&table)
                );
                self.query(sql.clone(), 0, Some(sql), core, sender);
            }
            Msg::Cancel => {
                let Some(session) = self.session.running() else {
                    return;
                };
                self.request(Command::CancelQuery { session }, core, sender, |_| None);
            }
            Msg::Ran {
                result,
                offset,
                browsing,
            } => {
                self.set_running(false);
                self.show_result(&result, offset, browsing.as_deref());
                // A statement that returns no rows may have changed the
                // tables. Reloading after every SELECT would fold the tree
                // up each time.
                if result.statements.iter().any(|s| s.columns.is_empty()) {
                    self.load_schema(core, sender);
                }
            }
            Msg::NotRun(error) => {
                self.set_running(false);
                self.show_error(Some(&printable(&error)));
            }
            Msg::Failed(error) => self.show_error(Some(&printable(&error))),
            Msg::ReadOnly(read_only) => {
                // Set by the code, or already what the core has.
                if read_only == self.read_only {
                    return;
                }
                let Some(session) = self.session.running() else {
                    return self.set_read_only(self.read_only);
                };
                let command = Command::SetReadOnly { session, read_only };
                let (core, input) = (core.clone(), sender.input_sender().clone());
                let was = self.read_only;
                runtime().spawn(async move {
                    let msgs = match core.execute(command).await {
                        Ok(_) => vec![Msg::ReadOnlySet(read_only)],
                        Err(e) => vec![Msg::ReadOnlySet(was), Msg::Failed(e.to_string())],
                    };
                    for msg in msgs {
                        let _ = input.send(AppMsg::Database { node, msg });
                    }
                });
            }
            Msg::ReadOnlySet(read_only) => self.set_read_only(read_only),
            Msg::RefreshSchema => self.load_schema(core, sender),
            Msg::Schema(schemas) => {
                self.schemas.remove_all();
                for schema in schemas {
                    self.schemas
                        .append(&glib::BoxedAnyObject::new(Item::Schema(schema)));
                }
            }
            Msg::Export => {
                let (start, end) = self
                    .buffer
                    .selection_bounds()
                    .unwrap_or_else(|| self.buffer.bounds());
                let sql = self.buffer.text(&start, &end, false).to_string();
                if sql.trim().is_empty() {
                    return self.show_error(Some("Write the query to export first."));
                }
                let dialog = gtk::FileDialog::builder()
                    .title("Export the rows as CSV")
                    .accept_label("Export")
                    .initial_name("export.csv")
                    .modal(true)
                    .build();
                let sender = sender.clone();
                dialog.save(Some(window), None::<&gio::Cancellable>, move |result| {
                    let Ok(file) = result else { return };
                    let msg = match file.path() {
                        Some(path) => Msg::ExportTo(path),
                        None => Msg::Failed("Pick a file on this computer.".into()),
                    };
                    sender.input(AppMsg::Database { node, msg });
                });
                self.export_sql = Some(sql);
            }
            Msg::ExportTo(path) => {
                let (Some(session), Some(sql)) = (self.session.running(), self.export_sql.take())
                else {
                    return;
                };
                self.show_error(None);
                self.note("Exporting…", sender);
                let command = Command::ExportQuery {
                    session,
                    sql,
                    path: path.clone(),
                };
                self.request(command, core, sender, move |response| match response {
                    Response::Exported { rows } => Some(Msg::Exported { rows, path }),
                    _ => None,
                });
            }
            Msg::Exported { rows, path } => {
                let name = path
                    .file_name()
                    .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
                let rows = if rows == 1 {
                    "1 row".into()
                } else {
                    format!("{rows} rows")
                };
                self.note(&format!("Saved {rows} to {}", printable(&name)), sender);
            }
            Msg::ClearNote(id) => {
                if id == self.note_id {
                    self.status.set_label(&self.summary);
                }
            }
            Msg::History => {
                let command = Command::ListHistory { node };
                self.request(command, core, sender, |response| match response {
                    Response::History { entries } => Some(Msg::HistoryListed(entries)),
                    _ => None,
                });
            }
            Msg::HistoryListed(entries) => self.show_history(entries, sender),
            Msg::UseQuery(sql) => {
                self.buffer.set_text(&sql);
                self.editor.grab_focus();
            }
            Msg::ClearHistory => {
                self.request(Command::ClearHistory { node }, core, sender, |_| None);
            }
            Msg::CopyRow => {
                let Some(row) = self
                    .selection
                    .selected_item()
                    .and_downcast::<glib::BoxedAnyObject>()
                else {
                    return;
                };
                let text = row
                    .borrow::<Row>()
                    .iter()
                    .map(|value| value.as_deref().unwrap_or_default())
                    .collect::<Vec<_>>()
                    .join("\t");
                self.grid.clipboard().set_text(&text);
                self.note("Copied the row", sender);
            }
        }
    }

    /// Runs `sql`, which starts `offset` characters into the editor.
    pub(super) fn query(
        &mut self,
        sql: String,
        offset: i32,
        browsing: Option<String>,
        core: &Client,
        sender: &ComponentSender<App>,
    ) {
        let Some(session) = self.session.running() else {
            return self.show_error(Some("Not connected."));
        };
        self.set_running(true);
        self.show_error(None);
        self.status.set_label("Running…");
        let command = Command::RunQuery {
            session,
            sql,
            limit: None,
        };
        let reply = move |response| match response {
            Response::Query { result } => Some(Msg::Ran {
                result,
                offset,
                browsing,
            }),
            _ => None,
        };
        self.send(command, core, sender, reply, Msg::NotRun);
    }
}
