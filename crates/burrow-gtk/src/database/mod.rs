//! Database tabs: an SQL editor, the result of the last query below it,
//! and the database's tables beside them. Also the dialog that creates and
//! edits database nodes.
//!
//! Everything the server sends (values, column, table and schema names,
//! errors) is cleaned up before it is shown, and drawn on one line in the
//! grid. Copying a row copies the values as they came.

use std::path::PathBuf;

use burrow_core::ipc::Client;
use burrow_core::{
    Command, DbSchema, DbTable, HistoryEntry, NodeId, QueryResult, Response, SessionState,
};
use relm4::gtk::gio;
use relm4::gtk::prelude::*;
use relm4::{ComponentSender, gtk};

use crate::app::{App, AppMsg};
use crate::runtime::runtime;
use crate::session::{self, SessionPage};

mod edit;
mod history;
mod page;
mod results;
mod schema;
mod settings;
mod update;

pub use edit::edit;
pub use page::follow_theme;

#[derive(Clone, Debug)]
pub enum Msg {
    /// `open_database` and what follows.
    Session(session::Msg),
    Run,
    Cancel,
    Ran {
        result: QueryResult,
        /// Where the SQL that ran starts in the editor, in characters, so
        /// an error's position can be found there.
        offset: i32,
        /// What ran, when it isn't the editor's text.
        browsing: Option<String>,
    },
    /// A command failed. Shown above the grid.
    Failed(String),
    /// `run_query` itself failed, so no result will come.
    NotRun(String),
    /// The read-only box was ticked or cleared.
    ReadOnly(bool),
    /// `set_read_only` took effect, or failed and left it as it was.
    ReadOnlySet(bool),
    RefreshSchema,
    Schema(Vec<DbSchema>),
    /// Shows the first rows of a table, without touching the editor.
    Browse {
        schema: String,
        table: String,
    },
    Export,
    ExportTo(PathBuf),
    Exported {
        rows: u64,
        path: PathBuf,
    },
    ClearNote(u32),
    History,
    HistoryListed(Vec<HistoryEntry>),
    UseQuery(String),
    ClearHistory,
    CopyRow,
}

/// One row of the result grid.
type Row = Vec<Option<String>>;

/// What a row of the schema tree stands for.
enum Item {
    Schema(DbSchema),
    Table { schema: String, table: DbTable },
    Column { name: String, data_type: String },
}

/// One database node's tab.
pub struct Database {
    node: NodeId,
    pub page: gtk::Box,
    pub session: SessionPage,
    /// As the core last confirmed it. Sessions start read-only.
    read_only: bool,
    running: bool,
    buffer: sourceview5::Buffer,
    editor: sourceview5::View,
    run: gtk::Button,
    cancel: gtk::Button,
    read_only_check: gtk::CheckButton,
    writable_note: gtk::Label,
    history_button: gtk::Button,
    status: gtk::Label,
    /// The status line's text when no note covers it.
    summary: String,
    note_id: u32,
    error: gtk::Label,
    grid: gtk::ColumnView,
    rows: gio::ListStore,
    selection: gtk::SingleSelection,
    schemas: gio::ListStore,
    /// The SQL to export, while the user picks where to.
    export_sql: Option<String>,
}

impl Database {
    fn show_error(&self, error: Option<&str>) {
        self.error.set_label(error.unwrap_or_default());
        self.error.set_visible(error.is_some());
    }

    fn set_running(&mut self, running: bool) {
        self.running = running;
        self.run.set_sensitive(!running);
        self.cancel.set_sensitive(running);
    }

    /// Shows the core's read-only setting, without sending it back.
    fn set_read_only(&mut self, read_only: bool) {
        self.read_only = read_only;
        self.read_only_check.set_active(read_only);
        self.writable_note.set_visible(!read_only);
    }

    pub fn session_changed(
        &mut self,
        state: &SessionState,
        core: &Client,
        sender: &ComponentSender<App>,
    ) {
        // The editor only shows once connected. Read before the session
        // page swaps it for the message.
        let was_connected = self.session.showing_content();
        let ended = |error: Option<&str>| match (error, was_connected) {
            (Some(_), false) => session::ended("Couldn't connect", error),
            _ => session::ended("The connection ended", error),
        };
        if matches!(state, SessionState::Closed { .. }) {
            self.set_running(false);
        }
        if self.session.session_changed(state, ended) {
            // A new session starts read-only, whatever the last one was.
            self.set_read_only(true);
            self.set_running(false);
            self.session.show_content();
            self.editor.grab_focus();
            self.load_schema(core, sender);
        }
    }

    /// Sends `command`, and turns its reply into a message with `reply`.
    /// A failure becomes [`Msg::Failed`].
    fn request(
        &self,
        command: Command,
        core: &Client,
        sender: &ComponentSender<App>,
        reply: impl FnOnce(Response) -> Option<Msg> + Send + 'static,
    ) {
        self.send(command, core, sender, reply, Msg::Failed);
    }

    /// As [`Database::request`], with `failed` making the failure's message.
    fn send(
        &self,
        command: Command,
        core: &Client,
        sender: &ComponentSender<App>,
        reply: impl FnOnce(Response) -> Option<Msg> + Send + 'static,
        failed: fn(String) -> Msg,
    ) {
        let (core, input, node) = (core.clone(), sender.input_sender().clone(), self.node);
        runtime().spawn(async move {
            let msg = match core.execute(command).await {
                Ok(response) => match reply(response) {
                    Some(msg) => msg,
                    None => return,
                },
                Err(e) => failed(e.to_string()),
            };
            let _ = input.send(AppMsg::Database { node, msg });
        });
    }
}

/// Asks the core to connect to `node`.
pub fn start(core: &Client, node: NodeId, sender: &ComponentSender<App>) {
    let wrap = move |msg| AppMsg::Database {
        node,
        msg: Msg::Session(msg),
    };
    session::start(core, Command::OpenDatabase { node }, sender, wrap);
}
