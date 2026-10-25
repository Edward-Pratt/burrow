//! Database tabs: an SQL editor, the result of the last query below it,
//! and the database's tables beside them. Also the dialog that creates and
//! edits database nodes.
//!
//! Everything the server sends (values, column, table and schema names,
//! errors) is cleaned up before it is shown, and drawn on one line in the
//! grid. Copying a row copies the values as they came.

use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use burrow_core::ipc::Client;
use burrow_core::{
    Command, DbSchema, DbTable, DbTableKind, HistoryEntry, Node, NodeId, QueryError, QueryResult,
    Response, SessionId, SessionState, StatementResult,
};
use relm4::gtk::prelude::*;
use relm4::gtk::{gio, glib};
use relm4::{ComponentSender, gtk};
use sourceview5::prelude::*;

use crate::app::{App, AppMsg};
use crate::dialogs;
use crate::remote::State;
use crate::runtime::runtime;

/// The most characters of a value drawn in a grid cell. Labels get slow
/// long before a value reaches the 64 KiB the core keeps.
const CELL_CHARS: usize = 300;

/// How long "Saved 3 rows" stays in the status line before the result's
/// own summary comes back.
const NOTE_TIME: Duration = Duration::from_secs(6);

#[derive(Clone, Debug)]
pub enum Msg {
    /// The core accepted `open_database`.
    Started(SessionId),
    /// `open_database` itself failed.
    StartFailed(String),
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
    Reconnect,
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
    pub state: State,
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
    stack: gtk::Stack,
    message: gtk::Label,
    reconnect: gtk::Button,
}

impl Database {
    pub fn new(node: NodeId, sender: &ComponentSender<App>) -> Database {
        let send = |msg: Msg| {
            let sender = sender.clone();
            move || {
                sender.input(AppMsg::Database {
                    node,
                    msg: msg.clone(),
                })
            }
        };
        let button = |label: &str, msg: Msg| {
            let button = gtk::Button::with_label(label);
            let send = send(msg);
            button.connect_clicked(move |_| send());
            button
        };

        let page = gtk::Box::new(gtk::Orientation::Vertical, 8);
        page.add_css_class("database-view");

        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let run = button("Run", Msg::Run);
        run.add_css_class("suggested-action");
        run.set_tooltip_text(Some("Run the selection, or everything (Ctrl+Enter)"));
        let cancel = button("Stop", Msg::Cancel);
        cancel.set_sensitive(false);
        let read_only_check = gtk::CheckButton::with_label("Read-only");
        read_only_check.set_active(true);
        read_only_check.set_tooltip_text(Some(
            "Transactions can't change anything. A query can still turn this off with SET.",
        ));
        read_only_check.connect_toggled({
            let sender = sender.clone();
            move |check| {
                let msg = Msg::ReadOnly(check.is_active());
                sender.input(AppMsg::Database { node, msg });
            }
        });
        let writable_note = gtk::Label::new(Some("Changes allowed"));
        writable_note.add_css_class("warning-text");
        writable_note.set_visible(false);
        bar.append(&run);
        bar.append(&cancel);
        bar.append(&read_only_check);
        bar.append(&writable_note);
        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        bar.append(&spacer);
        let history_button = button("History…", Msg::History);
        bar.append(&history_button);
        bar.append(&button("Export CSV…", Msg::Export));

        let buffer = sourceview5::Buffer::new(None::<&gtk::TextTagTable>);
        if let Some(sql) = sourceview5::LanguageManager::default().language("sql") {
            buffer.set_language(Some(&sql));
        }
        let editor = sourceview5::View::with_buffer(&buffer);
        editor.set_monospace(true);
        editor.set_show_line_numbers(true);
        editor.set_highlight_current_line(true);
        editor.set_auto_indent(true);
        editor.set_insert_spaces_instead_of_tabs(true);
        editor.set_tab_width(4);
        editor.add_css_class("sql-editor");
        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed({
            let send = send(Msg::Run);
            move |_, key, _, modifiers| {
                let enter = matches!(key, gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter);
                if enter && modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK) {
                    send();
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            }
        });
        editor.add_controller(keys);
        follow_theme(&buffer, &editor);
        let editor_scroller = gtk::ScrolledWindow::builder()
            .child(&editor)
            .vexpand(true)
            .min_content_height(120)
            .build();

        let status = gtk::Label::new(None);
        status.set_xalign(0.0);
        status.set_ellipsize(gtk::pango::EllipsizeMode::End);
        status.add_css_class("dim");
        let error = gtk::Label::new(None);
        error.set_xalign(0.0);
        error.set_wrap(true);
        error.set_selectable(true);
        error.add_css_class("error-text");
        error.set_visible(false);

        let rows = gio::ListStore::new::<glib::BoxedAnyObject>();
        let selection = gtk::SingleSelection::new(Some(rows.clone()));
        selection.set_autoselect(false);
        selection.set_can_unselect(true);
        let grid = gtk::ColumnView::new(Some(selection.clone()));
        grid.add_css_class("result-grid");
        grid.set_show_column_separators(true);
        grid.set_reorderable(false);
        let copy = gtk::EventControllerKey::new();
        copy.connect_key_pressed({
            let send = send(Msg::CopyRow);
            move |_, key, _, modifiers| {
                if key == gtk::gdk::Key::c
                    && modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK)
                {
                    send();
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            }
        });
        grid.add_controller(copy);
        let grid_scroller = gtk::ScrolledWindow::builder()
            .child(&grid)
            .vexpand(true)
            .build();
        let results = gtk::Box::new(gtk::Orientation::Vertical, 6);
        results.append(&status);
        results.append(&error);
        results.append(&grid_scroller);

        let work = gtk::Paned::new(gtk::Orientation::Vertical);
        work.set_start_child(Some(&editor_scroller));
        work.set_end_child(Some(&results));
        work.set_resize_start_child(false);
        work.set_shrink_start_child(false);
        work.set_position(180);

        let (tree, schemas) = schema_tree(node, sender);
        let tree_header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let tree_title = gtk::Label::new(Some("Tables"));
        tree_title.set_xalign(0.0);
        tree_title.set_hexpand(true);
        tree_title.add_css_class("heading");
        let refresh = gtk::Button::from_icon_name("view-refresh-symbolic");
        refresh.add_css_class("flat");
        refresh.set_tooltip_text(Some("List the tables again"));
        let send_refresh = send(Msg::RefreshSchema);
        refresh.connect_clicked(move |_| send_refresh());
        tree_header.append(&tree_title);
        tree_header.append(&refresh);
        let tree_scroller = gtk::ScrolledWindow::builder()
            .child(&tree)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();
        let side = gtk::Box::new(gtk::Orientation::Vertical, 6);
        side.add_css_class("schema-tree");
        side.append(&tree_header);
        side.append(&tree_scroller);

        let split = gtk::Paned::new(gtk::Orientation::Horizontal);
        split.set_start_child(Some(&side));
        split.set_end_child(Some(&work));
        split.set_shrink_start_child(false);
        split.set_resize_start_child(false);
        split.set_position(220);

        let main = gtk::Box::new(gtk::Orientation::Vertical, 8);
        main.append(&bar);
        main.append(&split);
        split.set_vexpand(true);

        let message = gtk::Label::new(Some("Connecting…"));
        message.set_wrap(true);
        message.set_max_width_chars(60);
        message.set_selectable(true);
        let reconnect = button("Reconnect", Msg::Reconnect);
        reconnect.set_halign(gtk::Align::Center);
        reconnect.set_visible(false);
        let message_page = gtk::Box::new(gtk::Orientation::Vertical, 12);
        message_page.set_valign(gtk::Align::Center);
        message_page.append(&message);
        message_page.append(&reconnect);

        let stack = gtk::Stack::new();
        stack.add_named(&main, Some("main"));
        stack.add_named(&message_page, Some("message"));
        stack.set_visible_child_name("message");
        stack.set_vexpand(true);
        page.append(&stack);

        Database {
            node,
            page,
            state: State::Starting,
            read_only: true,
            running: false,
            buffer,
            editor,
            run,
            cancel,
            read_only_check,
            writable_note,
            history_button,
            status,
            summary: String::new(),
            note_id: 0,
            error,
            grid,
            rows,
            selection,
            schemas,
            export_sql: None,
            stack,
            message,
            reconnect,
        }
    }

    fn show_message(&self, text: &str, can_reconnect: bool) {
        self.message.set_label(text);
        self.reconnect.set_visible(can_reconnect);
        self.stack.set_visible_child_name("message");
        // Otherwise the selectable message takes focus, all selected.
        if can_reconnect {
            self.reconnect.grab_focus();
        }
    }

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
        match state {
            SessionState::Connecting { step } => self.show_message(&format!("{step}…"), false),
            SessionState::Connected => {
                // A new session starts read-only, whatever the last one was.
                self.set_read_only(true);
                self.set_running(false);
                self.stack.set_visible_child_name("main");
                self.editor.grab_focus();
                self.load_schema(core, sender);
            }
            SessionState::Closed { error } => {
                self.state = State::Closed;
                self.set_running(false);
                // The editor only shows once connected.
                let was_connected = self.stack.visible_child_name().as_deref() == Some("main");
                let text = match (error, was_connected) {
                    (Some(error), true) => format!("The connection ended: {}", printable(error)),
                    (Some(error), false) => format!("Couldn't connect: {}", printable(error)),
                    (None, _) => "The connection ended.".into(),
                };
                self.show_message(&text, true);
            }
        }
    }

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
            Msg::Started(_) => {}
            Msg::StartFailed(error) => {
                self.state = State::Closed;
                self.show_message(&printable(&error), true);
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
                let State::Running(session) = self.state else {
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
                let State::Running(session) = self.state else {
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
                let (State::Running(session), Some(sql)) = (self.state, self.export_sql.take())
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
            Msg::Reconnect => {
                if self.state == State::Closed {
                    self.state = State::Starting;
                    self.show_error(None);
                    self.show_message("Connecting…", false);
                    start(core, node, sender);
                }
            }
        }
    }

    /// Runs `sql`, which starts `offset` characters into the editor.
    fn query(
        &mut self,
        sql: String,
        offset: i32,
        browsing: Option<String>,
        core: &Client,
        sender: &ComponentSender<App>,
    ) {
        let State::Running(session) = self.state else {
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

    fn load_schema(&self, core: &Client, sender: &ComponentSender<App>) {
        let State::Running(session) = self.state else {
            return;
        };
        self.request(
            Command::ListSchema { session },
            core,
            sender,
            |response| match response {
                Response::Schema { schemas } => Some(Msg::Schema(schemas)),
                _ => None,
            },
        );
    }

    /// Shows a note in the status line for a while.
    fn note(&mut self, text: &str, sender: &ComponentSender<App>) {
        self.status.set_label(text);
        self.note_id += 1;
        let (id, node, sender) = (self.note_id, self.node, sender.clone());
        glib::timeout_add_local_once(NOTE_TIME, move || {
            let msg = Msg::ClearNote(id);
            sender.input(AppMsg::Database { node, msg });
        });
    }

    fn show_result(&mut self, result: &QueryResult, offset: i32, browsing: Option<&str>) {
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

    fn fill_grid(&self, statement: &StatementResult) {
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

    fn show_history(&self, entries: Vec<HistoryEntry>, sender: &ComponentSender<App>) {
        let node = self.node;
        let popover = gtk::Popover::new();
        popover.set_parent(&self.history_button);
        popover.connect_closed(|popover| {
            let popover = popover.clone();
            glib::idle_add_local_once(move || popover.unparent());
        });
        let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
        if entries.is_empty() {
            let empty = gtk::Label::new(Some("No queries yet."));
            empty.add_css_class("dim");
            content.append(&empty);
        } else {
            let list = gtk::ListBox::new();
            list.add_css_class("history-list");
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let newest_first: Vec<HistoryEntry> = entries.into_iter().rev().collect();
            for entry in &newest_first {
                let line = gtk::Box::new(gtk::Orientation::Horizontal, 10);
                let sql = gtk::Label::new(Some(&one_line(&entry.sql)));
                sql.set_xalign(0.0);
                sql.set_hexpand(true);
                sql.set_ellipsize(gtk::pango::EllipsizeMode::End);
                // An ellipsized label asks for no width of its own.
                sql.set_width_chars(40);
                sql.set_max_width_chars(60);
                sql.add_css_class("monospace");
                let when = gtk::Label::new(Some(&ago(now.saturating_sub(entry.at))));
                when.add_css_class("dim");
                line.append(&sql);
                if !entry.ok {
                    let failed = gtk::Label::new(Some("failed"));
                    failed.add_css_class("error-text");
                    line.append(&failed);
                }
                line.append(&when);
                list.append(&line);
            }
            list.connect_row_activated({
                let (sender, popover) = (sender.clone(), popover.clone());
                move |_, row| {
                    if let Some(entry) = newest_first.get(row.index() as usize) {
                        let msg = Msg::UseQuery(entry.sql.clone());
                        sender.input(AppMsg::Database { node, msg });
                    }
                    popover.popdown();
                }
            });
            let scroller = gtk::ScrolledWindow::builder()
                .child(&list)
                .hscrollbar_policy(gtk::PolicyType::Never)
                .propagate_natural_height(true)
                .max_content_height(360)
                .min_content_width(420)
                .build();
            content.append(&scroller);
            let clear = gtk::Button::with_label("Clear history");
            clear.set_halign(gtk::Align::End);
            clear.connect_clicked({
                let (sender, popover) = (sender.clone(), popover.clone());
                move |_| {
                    sender.input(AppMsg::Database {
                        node,
                        msg: Msg::ClearHistory,
                    });
                    popover.popdown();
                }
            });
            content.append(&clear);
        }
        popover.set_child(Some(&content));
        popover.popup();
    }
}

/// Asks the core to connect to `node`. The reply arrives as
/// [`Msg::Started`] or [`Msg::StartFailed`].
pub fn start(core: &Client, node: NodeId, sender: &ComponentSender<App>) {
    let (core, input) = (core.clone(), sender.input_sender().clone());
    runtime().spawn(async move {
        let msg = match core.execute(Command::OpenDatabase { node }).await {
            Ok(Response::Session { session }) => Msg::Started(session),
            Ok(other) => Msg::StartFailed(format!("unexpected reply {other:?}")),
            Err(e) => Msg::StartFailed(e.to_string()),
        };
        let _ = input.send(AppMsg::Database { node, msg });
    });
}

/// The schemas, their tables and their columns, as a tree. Activating a
/// table shows its first rows.
fn schema_tree(node: NodeId, sender: &ComponentSender<App>) -> (gtk::ListView, gio::ListStore) {
    let schemas = gio::ListStore::new::<glib::BoxedAnyObject>();
    let tree = gtk::TreeListModel::new(schemas.clone(), false, false, |item| {
        let item = item.downcast_ref::<glib::BoxedAnyObject>()?;
        let children = gio::ListStore::new::<glib::BoxedAnyObject>();
        match &*item.borrow::<Item>() {
            Item::Schema(schema) => {
                for table in &schema.tables {
                    children.append(&glib::BoxedAnyObject::new(Item::Table {
                        schema: schema.name.clone(),
                        table: table.clone(),
                    }));
                }
            }
            Item::Table { table, .. } if !table.columns.is_empty() => {
                for column in &table.columns {
                    let data_type = if column.nullable {
                        column.data_type.clone()
                    } else {
                        format!("{} not null", column.data_type)
                    };
                    children.append(&glib::BoxedAnyObject::new(Item::Column {
                        name: column.name.clone(),
                        data_type,
                    }));
                }
            }
            _ => return None,
        }
        Some(children.upcast())
    });
    let selection = gtk::SingleSelection::new(Some(tree));
    selection.set_autoselect(false);
    selection.set_can_unselect(true);

    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let line = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        line.append(&gtk::Image::new());
        let name = gtk::Label::new(None);
        name.set_xalign(0.0);
        name.set_ellipsize(gtk::pango::EllipsizeMode::End);
        line.append(&name);
        let detail = gtk::Label::new(None);
        detail.set_xalign(0.0);
        detail.set_hexpand(true);
        detail.set_ellipsize(gtk::pango::EllipsizeMode::End);
        detail.add_css_class("dim");
        line.append(&detail);
        let expander = gtk::TreeExpander::new();
        expander.set_child(Some(&line));
        if let Some(item) = item.downcast_ref::<gtk::ListItem>() {
            item.set_child(Some(&expander));
        }
    });
    factory.connect_bind(|_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let (Some(expander), Some(row)) = (
            item.child().and_downcast::<gtk::TreeExpander>(),
            item.item().and_downcast::<gtk::TreeListRow>(),
        ) else {
            return;
        };
        expander.set_list_row(Some(&row));
        let Some(line) = expander.child().and_downcast::<gtk::Box>() else {
            return;
        };
        let icon = line.first_child().and_downcast::<gtk::Image>();
        let name = icon
            .as_ref()
            .and_then(|i| i.next_sibling())
            .and_downcast::<gtk::Label>();
        let detail = name
            .as_ref()
            .and_then(|n| n.next_sibling())
            .and_downcast::<gtk::Label>();
        let (Some(icon), Some(name), Some(detail), Some(object)) = (
            icon,
            name,
            detail,
            row.item().and_downcast::<glib::BoxedAnyObject>(),
        ) else {
            return;
        };
        let (icon_name, text, extra) = match &*object.borrow::<Item>() {
            Item::Schema(schema) => (
                "folder-symbolic",
                schema.name.clone(),
                tables_count(schema.tables.len()),
            ),
            Item::Table { table, .. } => {
                (table_icon(table.kind), table.name.clone(), String::new())
            }
            Item::Column { name, data_type } => ("", name.clone(), data_type.clone()),
        };
        icon.set_icon_name(Some(icon_name).filter(|i| !i.is_empty()));
        icon.set_visible(!icon_name.is_empty());
        name.set_label(&printable(&text));
        detail.set_label(&printable(&extra));
    });

    let view = gtk::ListView::new(Some(selection.clone()), Some(factory));
    view.set_single_click_activate(false);
    view.connect_activate({
        let sender = sender.clone();
        move |_, position| {
            let Some(row) = selection.item(position).and_downcast::<gtk::TreeListRow>() else {
                return;
            };
            let Some(object) = row.item().and_downcast::<glib::BoxedAnyObject>() else {
                return;
            };
            let msg = match &*object.borrow::<Item>() {
                Item::Table { schema, table } => Msg::Browse {
                    schema: schema.clone(),
                    table: table.name.clone(),
                },
                _ => {
                    row.set_expanded(!row.is_expanded());
                    return;
                }
            };
            sender.input(AppMsg::Database { node, msg });
        }
    });
    (view, schemas)
}

fn table_icon(kind: DbTableKind) -> &'static str {
    match kind {
        DbTableKind::View | DbTableKind::MaterializedView => "view-reveal-symbolic",
        _ => "x-office-spreadsheet-symbolic",
    }
}

fn tables_count(count: usize) -> String {
    match count {
        0 => "empty".into(),
        1 => "1 table".into(),
        n => format!("{n} tables"),
    }
}

/// Picks the editor's colours to match the window's, light or dark, now and
/// whenever they change. Without libadwaita nothing does it for us.
fn follow_theme(buffer: &sourceview5::Buffer, editor: &sourceview5::View) {
    let apply = {
        let buffer = buffer.clone();
        move |widget: &gtk::Widget| {
            // Light text means a dark theme.
            let color = widget.color();
            let dark = color.red() + color.green() + color.blue() > 1.5;
            let name = if dark { "Adwaita-dark" } else { "Adwaita" };
            if let Some(scheme) = sourceview5::StyleSchemeManager::default().scheme(name) {
                buffer.set_style_scheme(Some(&scheme));
            }
        }
    };
    let apply = Rc::new(apply);
    editor.connect_map({
        let apply = apply.clone();
        move |editor| apply(editor.upcast_ref())
    });
    if let Some(settings) = gtk::Settings::default() {
        let editor = editor.downgrade();
        let follow = move |_: &gtk::Settings| {
            if let Some(editor) = editor.upgrade() {
                // The new colours apply once the style is recomputed.
                let apply = apply.clone();
                glib::idle_add_local_once(move || apply(editor.upcast_ref()));
            }
        };
        settings.connect_gtk_interface_color_scheme_notify(follow.clone());
        settings.connect_gtk_theme_name_notify(follow);
    }
}

/// "12 rows · 4 ms", and what else the user should know about the result.
fn summary(result: &QueryResult, browsing: Option<&str>) -> String {
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
fn error_text(error: &QueryError) -> String {
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
fn cell_text(value: &str) -> String {
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

/// A query on one line, for the history list.
fn one_line(sql: &str) -> String {
    let words: Vec<&str> = sql.split_whitespace().collect();
    printable(&words.join(" "))
}

fn ago(seconds: u64) -> String {
    match seconds {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", seconds / 60),
        3600..86400 => format!("{} h ago", seconds / 3600),
        _ => format!("{} days ago", seconds / 86400),
    }
}

/// An SQL identifier, quoted so any name a server sends is taken as a
/// name.
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Text from the server, with control characters replaced so they can't
/// change what is around them.
fn printable(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { '\u{fffd}' } else { c })
        .collect()
}

/// What the database dialog edits. Other props, such as `sslrootcert` when
/// TLS is off, are kept as they were.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub via: Option<NodeId>,
    /// A TCP host, or `None` for the server's socket.
    pub host: Option<String>,
    /// The socket's folder, when it isn't the default.
    pub socket: Option<String>,
    pub port: u16,
    pub user: String,
    /// `None` uses the user's name.
    pub database: Option<String>,
    /// `None` leaves it to the default for the route.
    pub sslmode: Option<String>,
    pub sslrootcert: Option<String>,
}

/// The `sslmode` choices offered, with what each one is called.
const SSLMODES: [(Option<&str>, &str); 5] = [
    (None, "Default for the route"),
    (Some("disable"), "Off"),
    (Some("require"), "Encrypt, but don't check the server"),
    (
        Some("verify-ca"),
        "Check the CA only (needs a root certificate)",
    ),
    (Some("verify-full"), "Check the server"),
];

impl Settings {
    /// Reads what it can from a database's props, falling back to the
    /// dialog's defaults.
    pub fn from_props(props: &toml::Table) -> Settings {
        let string = |key: &str| {
            props
                .get(key)
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .filter(|s| !s.is_empty())
        };
        Settings {
            via: string("via").and_then(|s| s.parse().ok()).map(NodeId),
            host: string("host"),
            socket: string("socket"),
            port: props
                .get("port")
                .and_then(|v| v.as_integer())
                .and_then(|n| u16::try_from(n).ok())
                .filter(|&n| n != 0)
                .unwrap_or(5432),
            user: string("user").unwrap_or_default(),
            database: string("database"),
            sslmode: string("sslmode"),
            sslrootcert: string("sslrootcert"),
        }
    }

    /// `base` with these settings written over it.
    pub fn apply(&self, base: &toml::Table) -> toml::Table {
        let mut props = base.clone();
        let mut set = |key: &str, value: Option<toml::Value>| match value {
            Some(value) => {
                props.insert(key.into(), value);
            }
            None => {
                props.remove(key);
            }
        };
        set("via", self.via.map(|id| id.to_string().into()));
        set("host", self.host.clone().map(Into::into));
        set("socket", self.socket.clone().map(Into::into));
        set("port", Some(i64::from(self.port).into()));
        set("user", Some(self.user.clone().into()));
        set("database", self.database.clone().map(Into::into));
        set("sslmode", self.sslmode.clone().map(Into::into));
        set("sslrootcert", self.sslrootcert.clone().map(Into::into));
        props
    }
}

/// Asks for a database's name and settings, starting from `node` when
/// editing one. `hosts` are the SSH hosts it can go through. `done` gets
/// the name and the new props.
pub fn edit(
    parent: &impl IsA<gtk::Window>,
    node: Option<&Node>,
    hosts: Vec<(NodeId, String)>,
    done: impl Fn(String, toml::Table) + 'static,
) {
    let window = gtk::Window::builder()
        .title(if node.is_some() {
            "Edit database"
        } else {
            "New PostgreSQL database"
        })
        .modal(true)
        .transient_for(parent)
        .resizable(false)
        .default_width(460)
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_margin_top(18);
    content.set_margin_bottom(18);
    content.set_margin_start(18);
    content.set_margin_end(18);

    let base = node.map(|n| n.props.clone()).unwrap_or_default();
    let settings = Settings::from_props(&base);

    let grid = gtk::Grid::builder()
        .row_spacing(8)
        .column_spacing(12)
        .build();
    let label = |text: &str| {
        let label = gtk::Label::new(Some(text));
        label.set_xalign(0.0);
        label
    };
    let entry = |text: &str, placeholder: &str| {
        let entry = gtk::Entry::new();
        entry.set_hexpand(true);
        entry.set_text(text);
        entry.set_placeholder_text(Some(placeholder));
        entry
    };
    let mut row = 0;
    let mut attach = |name: &gtk::Label, widget: &gtk::Widget| {
        grid.attach(name, 0, row, 1, 1);
        grid.attach(widget, 1, row, 1, 1);
        row += 1;
    };

    let name = entry(node.map_or("", |n| n.name.as_str()), "");
    attach(&label("Name"), name.upcast_ref());

    let mut through: Vec<String> = vec!["This computer".into()];
    through.extend(hosts.iter().map(|(_, name)| name.clone()));
    let through: Vec<&str> = through.iter().map(String::as_str).collect();
    let host_choice = gtk::DropDown::from_strings(&through);
    if let Some(index) = settings
        .via
        .and_then(|via| hosts.iter().position(|(id, _)| *id == via))
    {
        host_choice.set_selected(index as u32 + 1);
    }
    attach(&label("Through"), host_choice.upcast_ref());

    let route = gtk::DropDown::from_strings(&["Network", "Socket"]);
    route.set_selected(if settings.host.is_some() || node.is_none() {
        0
    } else {
        1
    });
    attach(&label("Connect by"), route.upcast_ref());

    let host_label = label("Host");
    let host = entry(
        settings.host.as_deref().unwrap_or_default(),
        "As the host above sees it, such as localhost",
    );
    attach(&host_label, host.upcast_ref());
    let socket_label = label("Socket folder");
    let socket = entry(
        settings.socket.as_deref().unwrap_or_default(),
        "/run/postgresql",
    );
    attach(&socket_label, socket.upcast_ref());

    let port = gtk::SpinButton::with_range(1.0, 65535.0, 1.0);
    port.set_value(f64::from(settings.port));
    port.set_numeric(true);
    attach(&label("Port"), port.upcast_ref());

    let user_default = std::env::var("USER").unwrap_or_default();
    let user = entry(
        if settings.user.is_empty() {
            &user_default
        } else {
            &settings.user
        },
        "",
    );
    attach(&label("User"), user.upcast_ref());
    let database = entry(
        settings.database.as_deref().unwrap_or_default(),
        "Same as the user",
    );
    attach(&label("Database"), database.upcast_ref());

    let tls_label = label("TLS");
    let names: Vec<&str> = SSLMODES.iter().map(|(_, name)| *name).collect();
    let tls = gtk::DropDown::from_strings(&names);
    if let Some(index) = SSLMODES
        .iter()
        .position(|(mode, _)| *mode == settings.sslmode.as_deref())
    {
        tls.set_selected(index as u32);
    }
    attach(&tls_label, tls.upcast_ref());
    let root_label = label("Root certificate");
    let root = entry(
        settings.sslrootcert.as_deref().unwrap_or_default(),
        "The system's trusted CAs",
    );
    attach(&root_label, root.upcast_ref());
    content.append(&grid);

    let note = gtk::Label::new(None);
    note.set_xalign(0.0);
    note.set_wrap(true);
    note.set_max_width_chars(56);
    note.add_css_class("dim");
    content.append(&note);

    let show_route = {
        let network: Vec<gtk::Widget> = vec![
            host_label.upcast(),
            host.clone().upcast(),
            tls_label.upcast(),
            tls.clone().upcast(),
            root_label.upcast(),
            root.clone().upcast(),
        ];
        let local: Vec<gtk::Widget> = vec![socket_label.upcast(), socket.clone().upcast()];
        let (host_choice, note) = (host_choice.clone(), note.clone());
        move |route: &gtk::DropDown| {
            let by_network = route.selected() == 0;
            for widget in &network {
                widget.set_visible(by_network);
            }
            for widget in &local {
                widget.set_visible(!by_network);
            }
            let through_host = host_choice.selected() > 0;
            note.set_label(match (through_host, by_network) {
                (true, _) => {
                    "Burrow opens a channel on the SSH host's connection, so nothing \
                     listens on this computer. TLS is off by default: SSH already \
                     encrypts it. Through a socket, you sign in as the SSH user."
                }
                (false, true) => {
                    "By default, the server's certificate must be valid for its \
                     name. New sessions are read-only until you allow changes."
                }
                (false, false) => {
                    "A socket on this computer. New sessions are read-only until \
                     you allow changes."
                }
            });
        }
    };
    show_route(&route);
    route.connect_selected_notify(show_route.clone());
    host_choice.connect_selected_notify({
        let route = route.clone();
        move |_| show_route(&route)
    });

    let error = gtk::Label::new(None);
    error.set_xalign(0.0);
    error.set_wrap(true);
    error.add_css_class("error-text");
    error.set_visible(false);
    content.append(&error);

    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::End);
    buttons.set_margin_top(6);
    let cancel = gtk::Button::with_label("Cancel");
    let ok = gtk::Button::with_label(if node.is_some() { "Save" } else { "Create" });
    ok.add_css_class("suggested-action");
    buttons.append(&cancel);
    buttons.append(&ok);
    content.append(&buttons);
    window.set_child(Some(&content));

    let submit = Rc::new({
        let (window, name, error) = (window.clone(), name.clone(), error.clone());
        move || {
            let fail = |message: &str| {
                error.set_label(message);
                error.set_visible(true);
            };
            port.update();
            let title = name.text().trim().to_string();
            if title.is_empty() {
                return fail("Give the database a name.");
            }
            let text = |entry: &gtk::Entry| {
                Some(entry.text().trim().to_string()).filter(|s| !s.is_empty())
            };
            let by_network = route.selected() == 0;
            let host = if by_network {
                match text(&host) {
                    Some(host) => Some(host),
                    None => return fail("Say where the server is, such as localhost."),
                }
            } else {
                None
            };
            let Some(user) = text(&user) else {
                return fail("Say which user to sign in as.");
            };
            let (sslmode, sslrootcert) = if by_network {
                let mode = SSLMODES
                    .get(tls.selected() as usize)
                    .and_then(|(mode, _)| *mode);
                let root = text(&root);
                if root.as_deref().is_some_and(|p| !p.starts_with('/')) {
                    return fail("Give the root certificate's full path.");
                }
                if mode == Some("verify-ca") && root.is_none() {
                    return fail("Checking the CA only needs the root certificate to trust.");
                }
                (mode.map(str::to_string), root)
            } else {
                (None, None)
            };
            let via = (host_choice.selected() as usize)
                .checked_sub(1)
                .and_then(|index| hosts.get(index))
                .map(|(id, _)| *id);
            let settings = Settings {
                via,
                host,
                socket: if by_network { None } else { text(&socket) },
                port: port.value() as u16,
                user,
                database: text(&database),
                sslmode,
                sslrootcert,
            };
            window.close();
            done(title, settings.apply(&base));
        }
    });
    ok.connect_clicked({
        let submit = submit.clone();
        move |_| submit()
    });
    name.connect_activate(move |_| submit());
    cancel.connect_clicked({
        let window = window.clone();
        move |_| window.close()
    });
    dialogs::close_on_escape(&window);

    window.present();
    name.grab_focus();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_read_back_what_they_wrote_and_keep_other_props() {
        let mut base = toml::Table::new();
        base.insert("extra".into(), "kept".into());
        base.insert("host".into(), "old".into());
        let settings = Settings {
            via: Some(NodeId::new()),
            host: None,
            socket: Some("/tmp/pg".into()),
            port: 5433,
            user: "app".into(),
            database: None,
            sslmode: None,
            sslrootcert: None,
        };
        let props = settings.apply(&base);
        assert_eq!(props["extra"].as_str(), Some("kept"));
        assert!(!props.contains_key("host"), "a socket drops the old host");
        assert!(!props.contains_key("database"));
        assert_eq!(Settings::from_props(&props), settings);
    }

    #[test]
    fn cells_are_drawn_on_one_line_without_control_characters() {
        assert_eq!(cell_text("a\nb\tc\u{1b}[2J"), "a↵b c\u{fffd}[2J");
        let long = "x".repeat(CELL_CHARS + 5);
        let cut = cell_text(&long);
        assert_eq!(cut.chars().count(), CELL_CHARS + 1);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn identifiers_are_quoted_whatever_the_server_calls_them() {
        assert_eq!(quote_ident("items"), "\"items\"");
        assert_eq!(
            quote_ident("a\"; drop table x; --"),
            "\"a\"\"; drop table x; --\""
        );
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
