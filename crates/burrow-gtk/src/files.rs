//! SFTP file panels: a tab that browses an SSH host's files, and copies
//! files and folders to and from this computer.
//!
//! Names come from the server, so they are cleaned up before they are
//! shown, and checked before they become part of a local path. Remote
//! paths are always built from the names as the server sent them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use burrow_core::{
    Command, Core, FileEntry, FileKind, NodeId, Response, SessionId, SessionState, TransferId,
    TransferState,
};
use relm4::gtk::prelude::*;
use relm4::gtk::{gio, glib};
use relm4::{ComponentSender, gtk};

use crate::app::{App, AppMsg};
use crate::dialogs;
use crate::remote::State;
use crate::runtime::runtime;

/// Rows drawn before the rest of a folder is summed up. A list of widgets
/// gets slow long before a folder gets this big.
const MAX_ROWS: usize = 2000;

/// How long a finished transfer stays in the list.
const DONE_TIME: Duration = Duration::from_secs(4);

#[derive(Clone, Debug)]
pub enum Msg {
    /// The core accepted `open_files`.
    Started(SessionId),
    /// `open_files` itself failed.
    StartFailed(String),
    Listed {
        path: String,
        entries: Vec<FileEntry>,
    },
    /// A command failed. Shown above the list.
    Failed(String),
    /// Lists the folder typed into the path bar.
    GoTyped,
    Up,
    Home,
    Refresh,
    Activate(i32),
    SelectionChanged,
    NewFolder,
    Rename,
    Delete,
    /// Confirmed in a dialog.
    MakeDir(String),
    RenameTo {
        from: String,
        to: String,
    },
    Remove(String),
    Download,
    Upload {
        folder: bool,
    },
    /// Where to download to or upload from, picked by the user.
    DownloadTo {
        remote: String,
        local: PathBuf,
        overwrite: bool,
    },
    UploadFrom {
        local: PathBuf,
        remote: String,
        overwrite: bool,
    },
    TransferStarted {
        transfer: TransferId,
        label: String,
    },
    /// A transfer's button: cancel while it runs, dismiss once it failed.
    TransferButton(TransferId),
    Dismiss(TransferId),
    Reconnect,
}

/// One SSH host's file panel.
pub struct Files {
    node: NodeId,
    pub page: gtk::Box,
    pub state: State,
    /// The folder shown, as the server names it. Empty until the first
    /// listing arrives.
    path: String,
    /// The entries shown, in row order.
    entries: Vec<FileEntry>,
    path_entry: gtk::Entry,
    stack: gtk::Stack,
    list: gtk::ListBox,
    message: gtk::Label,
    reconnect: gtk::Button,
    error: gtk::Label,
    /// Buttons that act on the selected entry.
    on_selection: Vec<gtk::Button>,
    transfers_box: gtk::Box,
    transfers: HashMap<TransferId, Transfer>,
}

struct Transfer {
    row: gtk::Box,
    label: gtk::Label,
    progress: gtk::ProgressBar,
    button: gtk::Button,
    ended: bool,
    /// "Downloading notes.txt", once the reply to the command arrives.
    what: Option<String>,
    /// The latest state, as words.
    status: String,
}

impl Transfer {
    fn redraw(&self) {
        let what = self.what.as_deref().unwrap_or("Copying");
        self.label.set_label(&format!("{what}: {}", self.status));
    }
}

impl Files {
    pub fn new(node: NodeId, sender: &ComponentSender<App>) -> Files {
        let send = |msg: Msg| {
            let sender = sender.clone();
            move || {
                sender.input(AppMsg::Files {
                    node,
                    msg: msg.clone(),
                })
            }
        };
        let icon_button = |icon: &str, tooltip: &str, msg: Msg| {
            let button = gtk::Button::from_icon_name(icon);
            button.set_tooltip_text(Some(tooltip));
            let send = send(msg);
            button.connect_clicked(move |_| send());
            button
        };
        let button = |label: &str, msg: Msg| {
            let button = gtk::Button::with_label(label);
            let send = send(msg);
            button.connect_clicked(move |_| send());
            button
        };

        let page = gtk::Box::new(gtk::Orientation::Vertical, 8);
        page.add_css_class("files-view");

        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        bar.append(&icon_button("go-up-symbolic", "Up (Backspace)", Msg::Up));
        bar.append(&icon_button("go-home-symbolic", "Home folder", Msg::Home));
        bar.append(&icon_button(
            "view-refresh-symbolic",
            "Refresh",
            Msg::Refresh,
        ));
        let path_entry = gtk::Entry::new();
        path_entry.set_hexpand(true);
        path_entry.set_placeholder_text(Some("Folder"));
        path_entry.connect_activate({
            let send = send(Msg::GoTyped);
            move |_| send()
        });
        bar.append(&path_entry);
        page.append(&bar);

        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        actions.append(&button("New folder…", Msg::NewFolder));
        actions.append(&button("Upload files…", Msg::Upload { folder: false }));
        actions.append(&button("Upload folder…", Msg::Upload { folder: true }));
        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        actions.append(&spacer);
        let on_selection = vec![
            button("Download…", Msg::Download),
            button("Rename…", Msg::Rename),
            button("Delete…", Msg::Delete),
        ];
        for button in &on_selection {
            button.set_sensitive(false);
            actions.append(button);
        }
        page.append(&actions);

        let error = gtk::Label::new(None);
        error.set_xalign(0.0);
        error.set_wrap(true);
        error.set_selectable(true);
        error.add_css_class("error-text");
        error.set_visible(false);
        page.append(&error);

        let list = gtk::ListBox::new();
        list.add_css_class("file-list");
        list.set_selection_mode(gtk::SelectionMode::Single);
        // A click selects, so the buttons can act on it; a double click
        // opens.
        list.set_activate_on_single_click(false);
        list.connect_row_activated({
            let sender = sender.clone();
            move |_, row| {
                let msg = Msg::Activate(row.index());
                sender.input(AppMsg::Files { node, msg });
            }
        });
        list.connect_selected_rows_changed({
            let send = send(Msg::SelectionChanged);
            move |_| send()
        });
        // Like the sidebar, these keys only apply while the list has focus.
        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed({
            let sender = sender.clone();
            move |_, key, _, _| {
                let msg = match key {
                    gtk::gdk::Key::Delete => Msg::Delete,
                    gtk::gdk::Key::F2 => Msg::Rename,
                    gtk::gdk::Key::BackSpace => Msg::Up,
                    _ => return glib::Propagation::Proceed,
                };
                sender.input(AppMsg::Files { node, msg });
                glib::Propagation::Stop
            }
        });
        list.add_controller(keys);
        let scroller = gtk::ScrolledWindow::builder()
            .child(&list)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();

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
        stack.add_named(&scroller, Some("list"));
        stack.add_named(&message_page, Some("message"));
        stack.set_visible_child_name("message");
        page.append(&stack);

        let transfers_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
        page.append(&transfers_box);

        Files {
            node,
            page,
            state: State::Starting,
            path: String::new(),
            entries: Vec::new(),
            path_entry,
            stack,
            list,
            message,
            reconnect,
            error,
            on_selection,
            transfers_box,
            transfers: HashMap::new(),
        }
    }

    /// Shows a message in place of the list.
    fn show_message(&self, text: &str, can_reconnect: bool) {
        self.message.set_label(text);
        self.reconnect.set_visible(can_reconnect);
        self.stack.set_visible_child_name("message");
    }

    fn show_error(&self, error: Option<&str>) {
        self.error.set_label(error.unwrap_or_default());
        self.error.set_visible(error.is_some());
    }

    /// The session's state changed.
    pub fn session_changed(
        &mut self,
        state: &SessionState,
        core: &Core,
        sender: &ComponentSender<App>,
    ) {
        match state {
            SessionState::Connecting { step } => self.show_message(&format!("{step}…"), false),
            SessionState::Connected => self.list_dir(self.path.clone(), core, sender),
            SessionState::Closed { error } => {
                self.state = State::Closed;
                let text = match error {
                    Some(error) => format!("The connection ended: {}", printable(error)),
                    None => "The connection ended.".into(),
                };
                self.show_message(&text, true);
            }
        }
    }

    /// Handles a message for this panel. `window` is the parent of its
    /// dialogs.
    pub fn update(
        &mut self,
        msg: Msg,
        core: &Core,
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
            Msg::Listed { path, entries } => {
                self.show_error(None);
                // Listing the same folder again keeps what was selected.
                let keep = self
                    .selected()
                    .filter(|_| path == self.path)
                    .map(|e| e.name.clone());
                self.path = path;
                self.path_entry.set_text(&printable(&self.path));
                self.entries = sorted(entries);
                self.fill_list(keep.as_deref());
                self.stack.set_visible_child_name("list");
            }
            Msg::Failed(error) => self.show_error(Some(&printable(&error))),
            Msg::GoTyped => {
                let typed = self.path_entry.text().trim().to_string();
                // Unchanged text stands for the real path, which can hold
                // characters that are shown differently.
                let path = if typed == printable(&self.path) {
                    self.path.clone()
                } else {
                    typed
                };
                self.list_dir(path, core, sender);
            }
            Msg::Up => {
                if let Some(parent) = parent(&self.path) {
                    self.list_dir(parent, core, sender);
                }
            }
            Msg::Home => self.list_dir(String::new(), core, sender),
            Msg::Refresh => self.list_dir(self.path.clone(), core, sender),
            Msg::Activate(index) => {
                let Some(entry) = self.entries.get(index as usize) else {
                    return;
                };
                match entry.kind {
                    // A link to a file fails to list, and says so.
                    FileKind::Dir | FileKind::Symlink => {
                        self.list_dir(join(&self.path, &entry.name), core, sender)
                    }
                    FileKind::File => self.update(Msg::Download, core, window, sender),
                    FileKind::Other => {}
                }
            }
            Msg::SelectionChanged => {
                let selected = self.selected().is_some();
                for button in &self.on_selection {
                    button.set_sensitive(selected);
                }
            }
            Msg::NewFolder => {
                let (dir, sender) = (self.path.clone(), sender.clone());
                dialogs::prompt(
                    window,
                    "New folder",
                    "Folder name",
                    "",
                    "Create",
                    move |name| {
                        let msg = match checked_name(&name) {
                            Ok(name) => Msg::MakeDir(join(&dir, name)),
                            Err(error) => Msg::Failed(error),
                        };
                        sender.input(AppMsg::Files { node, msg });
                    },
                );
            }
            Msg::Rename => {
                let Some(entry) = self.selected() else { return };
                let from = join(&self.path, &entry.name);
                let (dir, sender) = (self.path.clone(), sender.clone());
                dialogs::prompt(
                    window,
                    "Rename",
                    "New name",
                    &printable(&entry.name),
                    "Rename",
                    move |name| {
                        let msg = match checked_name(&name) {
                            Ok(name) => Msg::RenameTo {
                                from: from.clone(),
                                to: join(&dir, name),
                            },
                            Err(error) => Msg::Failed(error),
                        };
                        sender.input(AppMsg::Files { node, msg });
                    },
                );
            }
            Msg::Delete => {
                let Some(entry) = self.selected() else { return };
                let path = join(&self.path, &entry.name);
                let detail = if entry.kind == FileKind::Dir {
                    "Everything inside it is deleted too. This can't be undone."
                } else {
                    "This can't be undone."
                };
                let sender = sender.clone();
                dialogs::confirm(
                    window,
                    &format!("Delete “{}” from the server?", printable(&entry.name)),
                    detail,
                    "Delete",
                    move || {
                        sender.input(AppMsg::Files {
                            node,
                            msg: Msg::Remove(path),
                        })
                    },
                );
            }
            Msg::MakeDir(path) => {
                self.change(|session| Command::MakeDir { session, path }, core, sender);
            }
            Msg::RenameTo { from, to } => {
                let command = |session| Command::RenamePath { session, from, to };
                self.change(command, core, sender);
            }
            Msg::Remove(path) => {
                self.change(
                    |session| Command::RemovePath { session, path },
                    core,
                    sender,
                );
            }
            Msg::Download => {
                let Some(entry) = self.selected() else { return };
                // The server's name becomes part of a local path here.
                let name = match checked_name(&entry.name) {
                    Ok(name) => name.to_string(),
                    Err(error) => return self.show_error(Some(&error)),
                };
                let remote = join(&self.path, &entry.name);
                let is_dir = entry.kind == FileKind::Dir;
                let dialog = gtk::FileDialog::builder()
                    .title(format!("Download “{}” to", printable(&name)))
                    .accept_label("Download here")
                    .modal(true)
                    .build();
                let (window, sender) = (window.clone(), sender.clone());
                dialog.select_folder(
                    Some(&window.clone()),
                    None::<&gio::Cancellable>,
                    move |result| {
                        let Ok(folder) = result else { return };
                        let Some(folder) = folder.path() else {
                            let msg = Msg::Failed("Pick a folder on this computer.".into());
                            return sender.input(AppMsg::Files { node, msg });
                        };
                        let local = folder.join(&name);
                        let exists = local.symlink_metadata().is_ok();
                        let download = move |overwrite| {
                            let (remote, local) = (remote.clone(), local.clone());
                            let msg = Msg::DownloadTo {
                                remote,
                                local,
                                overwrite,
                            };
                            sender.input(AppMsg::Files { node, msg });
                        };
                        if exists {
                            let (heading, detail, confirm) =
                                replace_question(&name, is_dir, "folder");
                            dialogs::confirm(&window, &heading, detail, confirm, move || {
                                download(true)
                            });
                        } else {
                            download(false);
                        }
                    },
                );
            }
            Msg::Upload { folder } => {
                let dialog = gtk::FileDialog::builder()
                    .title(if folder {
                        "Upload a folder"
                    } else {
                        "Upload a file"
                    })
                    .accept_label("Upload")
                    .modal(true)
                    .build();
                let (dir, sender) = (self.path.clone(), sender.clone());
                let names: Vec<String> = self.entries.iter().map(|e| e.name.clone()).collect();
                let picked = {
                    let window = window.clone();
                    move |result: Result<gio::File, glib::Error>| {
                        let Ok(file) = result else { return };
                        let fail = |error: &str| {
                            let msg = Msg::Failed(error.into());
                            sender.input(AppMsg::Files { node, msg });
                        };
                        let Some(local) = file.path() else {
                            return fail("Pick a file on this computer.");
                        };
                        let Some(name) = upload_name(&local) else {
                            return fail("Burrow can only upload names that are valid UTF-8.");
                        };
                        let remote = join(&dir, &name);
                        let upload = move |overwrite| {
                            let (local, remote) = (local.clone(), remote.clone());
                            let msg = Msg::UploadFrom {
                                local,
                                remote,
                                overwrite,
                            };
                            sender.input(AppMsg::Files { node, msg });
                        };
                        if names.contains(&name) {
                            let (heading, detail, confirm) =
                                replace_question(&name, folder, "folder on the server");
                            dialogs::confirm(&window, &heading, detail, confirm, move || {
                                upload(true)
                            });
                        } else {
                            upload(false);
                        }
                    }
                };
                if folder {
                    dialog.select_folder(Some(window), None::<&gio::Cancellable>, picked);
                } else {
                    dialog.open(Some(window), None::<&gio::Cancellable>, picked);
                }
            }
            Msg::DownloadTo {
                remote,
                local,
                overwrite,
            } => {
                let name = local.file_name().map(|n| n.to_string_lossy().into_owned());
                let label = format!("Downloading {}", printable(&name.unwrap_or_default()));
                let command = |session| Command::Download {
                    session,
                    remote,
                    local,
                    overwrite,
                };
                self.transfer(label, command, core, sender);
            }
            Msg::UploadFrom {
                local,
                remote,
                overwrite,
            } => {
                let name = upload_name(&local).unwrap_or_default();
                let label = format!("Uploading {}", printable(&name));
                let command = |session| Command::Upload {
                    session,
                    local,
                    remote,
                    overwrite,
                };
                self.transfer(label, command, core, sender);
            }
            Msg::TransferStarted { transfer, label } => {
                let row = self.transfer_row(transfer, sender);
                row.what = Some(label);
                row.redraw();
            }
            Msg::TransferButton(transfer) => match self.transfers.get(&transfer) {
                Some(row) if row.ended => self.update(Msg::Dismiss(transfer), core, window, sender),
                Some(_) => {
                    let core = core.clone();
                    runtime().spawn(async move {
                        let _ = core.execute(Command::CancelTransfer { transfer }).await;
                    });
                }
                None => {}
            },
            Msg::Dismiss(transfer) => {
                if let Some(row) = self.transfers.remove(&transfer) {
                    self.transfers_box.remove(&row.row);
                }
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

    /// A transfer of this panel's session moved on.
    pub fn transfer_changed(
        &mut self,
        transfer: TransferId,
        state: &TransferState,
        core: &Core,
        sender: &ComponentSender<App>,
    ) {
        let node = self.node;
        let row = self.transfer_row(transfer, sender);
        row.ended = !matches!(state, TransferState::Running { .. });
        let dismiss_later = || {
            let sender = sender.clone();
            glib::timeout_add_local_once(DONE_TIME, move || {
                let msg = Msg::Dismiss(transfer);
                sender.input(AppMsg::Files { node, msg });
            });
        };
        match state {
            TransferState::Running { done, total } => {
                row.status = format!("{} of {}", size(*done), size(*total));
                if *total > 0 {
                    row.progress.set_fraction(*done as f64 / *total as f64);
                }
            }
            TransferState::Finished => {
                row.status = "done".into();
                row.progress.set_fraction(1.0);
                row.button.set_visible(false);
                dismiss_later();
            }
            TransferState::Cancelled => {
                row.status = "cancelled".into();
                row.button.set_visible(false);
                dismiss_later();
            }
            TransferState::Failed { error } => {
                row.status = printable(error);
                row.label.add_css_class("error-text");
                row.button.set_label("Dismiss");
            }
        }
        row.redraw();
        if *state == TransferState::Finished {
            // An upload may have added to the folder shown.
            self.list_dir(self.path.clone(), core, sender);
        }
    }

    /// The transfer's row, made on first use: its first event can arrive
    /// before the reply that started it.
    fn transfer_row(
        &mut self,
        transfer: TransferId,
        sender: &ComponentSender<App>,
    ) -> &mut Transfer {
        let node = self.node;
        let transfers_box = &self.transfers_box;
        self.transfers.entry(transfer).or_insert_with(|| {
            let label = gtk::Label::new(None);
            label.set_xalign(0.0);
            label.set_hexpand(true);
            label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            let progress = gtk::ProgressBar::new();
            progress.set_valign(gtk::Align::Center);
            progress.set_size_request(160, -1);
            let button = gtk::Button::with_label("Cancel");
            let sender = sender.clone();
            button.connect_clicked(move |_| {
                let msg = Msg::TransferButton(transfer);
                sender.input(AppMsg::Files { node, msg });
            });
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
            row.add_css_class("transfer");
            row.append(&label);
            row.append(&progress);
            row.append(&button);
            transfers_box.append(&row);
            let transfer = Transfer {
                row,
                label,
                progress,
                button,
                ended: false,
                what: None,
                status: "starting".into(),
            };
            transfer.redraw();
            transfer
        })
    }

    fn selected(&self) -> Option<&FileEntry> {
        let row = self.list.selected_row()?;
        self.entries.get(usize::try_from(row.index()).ok()?)
    }

    /// Lists `path`, or the home folder when it is empty.
    fn list_dir(&self, path: String, core: &Core, sender: &ComponentSender<App>) {
        let State::Running(session) = self.state else {
            return;
        };
        let (core, input, node) = (core.clone(), sender.input_sender().clone(), self.node);
        runtime().spawn(async move {
            let msg = match core.execute(Command::ListDir { session, path }).await {
                Ok(Response::Dir { path, entries }) => Msg::Listed { path, entries },
                Ok(other) => Msg::Failed(format!("unexpected reply {other:?}")),
                Err(e) => Msg::Failed(e.to_string()),
            };
            let _ = input.send(AppMsg::Files { node, msg });
        });
    }

    /// Runs a command that changes the folder, then lists it again.
    fn change(
        &self,
        command: impl FnOnce(SessionId) -> Command,
        core: &Core,
        sender: &ComponentSender<App>,
    ) {
        let State::Running(session) = self.state else {
            return;
        };
        let command = command(session);
        let (core, input, node) = (core.clone(), sender.input_sender().clone(), self.node);
        runtime().spawn(async move {
            let msg = match core.execute(command).await {
                Ok(_) => Msg::Refresh,
                Err(e) => Msg::Failed(e.to_string()),
            };
            let _ = input.send(AppMsg::Files { node, msg });
        });
    }

    fn transfer(
        &self,
        label: String,
        command: impl FnOnce(SessionId) -> Command,
        core: &Core,
        sender: &ComponentSender<App>,
    ) {
        let State::Running(session) = self.state else {
            return self.show_error(Some("Not connected."));
        };
        let command = command(session);
        let (core, input, node) = (core.clone(), sender.input_sender().clone(), self.node);
        runtime().spawn(async move {
            let msg = match core.execute(command).await {
                Ok(Response::Transfer { transfer }) => Msg::TransferStarted { transfer, label },
                Ok(other) => Msg::Failed(format!("unexpected reply {other:?}")),
                Err(e) => Msg::Failed(e.to_string()),
            };
            let _ = input.send(AppMsg::Files { node, msg });
        });
    }

    /// Draws the entries, selecting the one called `keep`.
    fn fill_list(&self, keep: Option<&str>) {
        // Rebuilding destroys the focused row, and GTK would then select
        // whichever row it focuses next.
        let had_focus = self.list.focus_child().is_some();
        self.list.remove_all();
        for entry in self.entries.iter().take(MAX_ROWS) {
            self.list.append(&row(entry));
        }
        if self.entries.len() > MAX_ROWS {
            let more = gtk::Label::new(Some(&format!(
                "…and {} more, not shown",
                self.entries.len() - MAX_ROWS
            )));
            more.add_css_class("dim");
            let row = gtk::ListBoxRow::new();
            row.set_child(Some(&more));
            row.set_activatable(false);
            row.set_selectable(false);
            self.list.append(&row);
        }
        self.list.unselect_all();
        let index = keep.and_then(|name| self.entries.iter().position(|e| e.name == name));
        if let Some(row) = index
            .filter(|&i| i < MAX_ROWS)
            .and_then(|i| self.list.row_at_index(i as i32))
        {
            self.list.select_row(Some(&row));
            if had_focus {
                row.grab_focus();
            }
        }
        let selected = self.selected().is_some();
        for button in &self.on_selection {
            button.set_sensitive(selected);
        }
    }
}

/// Asks the core for an SFTP session on `node`. The reply arrives as
/// [`Msg::Started`] or [`Msg::StartFailed`].
pub fn start(core: &Core, node: NodeId, sender: &ComponentSender<App>) {
    let (core, input) = (core.clone(), sender.input_sender().clone());
    runtime().spawn(async move {
        let msg = match core.execute(Command::OpenFiles { node }).await {
            Ok(Response::Session { session }) => Msg::Started(session),
            Ok(other) => Msg::StartFailed(format!("unexpected reply {other:?}")),
            Err(e) => Msg::StartFailed(e.to_string()),
        };
        let _ = input.send(AppMsg::Files { node, msg });
    });
}

fn row(entry: &FileEntry) -> gtk::ListBoxRow {
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    let icon = match entry.kind {
        FileKind::Dir => "folder-symbolic",
        FileKind::Symlink => "emblem-symbolic-link-symbolic",
        FileKind::File | FileKind::Other => "text-x-generic-symbolic",
    };
    content.append(&gtk::Image::from_icon_name(icon));
    let name = gtk::Label::new(Some(&printable(&entry.name)));
    name.set_xalign(0.0);
    name.set_hexpand(true);
    name.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    content.append(&name);
    let column = |text: String, chars: i32| {
        let label = gtk::Label::new(Some(&text));
        label.set_xalign(1.0);
        label.set_width_chars(chars);
        label.add_css_class("dim");
        label
    };
    let size = match entry.kind {
        FileKind::File => size(entry.size),
        _ => String::new(),
    };
    content.append(&column(size, 10));
    content.append(&column(entry.permissions.map(mode).unwrap_or_default(), 10));
    let modified = entry
        .modified
        .and_then(|secs| glib::DateTime::from_unix_local(secs as i64).ok())
        .and_then(|time| time.format("%Y-%m-%d %H:%M").ok())
        .map(String::from)
        .unwrap_or_default();
    content.append(&column(modified, 16));
    let row = gtk::ListBoxRow::new();
    row.set_child(Some(&content));
    row
}

/// Folders first, then by name.
fn sorted(mut entries: Vec<FileEntry>) -> Vec<FileEntry> {
    entries.sort_by(|a, b| {
        (a.kind != FileKind::Dir)
            .cmp(&(b.kind != FileKind::Dir))
            .then_with(|| a.name.cmp(&b.name))
    });
    entries
}

/// "Replace …?" for a name that is already there.
fn replace_question(name: &str, is_dir: bool, place: &str) -> (String, &'static str, &'static str) {
    let name = printable(name);
    if is_dir {
        (
            format!("Merge into the {place} “{name}”?"),
            "A folder with this name is already there. Files in it with the same names are replaced.",
            "Merge",
        )
    } else {
        (
            format!("Replace “{name}”?"),
            "Something with this name is already there, and it will be overwritten.",
            "Replace",
        )
    }
}

/// A name that is safe to put after a folder: on the server, and above all
/// on this computer, where `..` would leave the folder the user picked.
fn checked_name(name: &str) -> Result<&str, String> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
        Err(format!(
            "“{}” can't be used as a file name.",
            printable(name)
        ))
    } else {
        Ok(name)
    }
}

/// The name a local file gets on the server. Remote paths are text, so a
/// name that isn't UTF-8 can't be sent.
fn upload_name(local: &Path) -> Option<String> {
    local.file_name()?.to_str().map(String::from)
}

/// `name` inside the remote folder `dir`.
fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// The folder holding `path`, or `None` at the root.
fn parent(path: &str) -> Option<String> {
    let path = path.trim_end_matches('/');
    let cut = path.rfind('/')?;
    Some(if cut == 0 { "/" } else { &path[..cut] }.to_string())
}

/// A name or error from the server, with control characters replaced so
/// they can't change what is around them.
fn printable(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { '\u{fffd}' } else { c })
        .collect()
}

/// `rwxr-xr-x` for `0o755`.
fn mode(bits: u32) -> String {
    (0..9)
        .map(|i| {
            if bits & (1 << (8 - i)) == 0 {
                '-'
            } else {
                ['r', 'w', 'x'][i % 3]
            }
        })
        .collect()
}

/// "1.5 MiB".
fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if value < 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_join_and_climb() {
        assert_eq!(join("/", "etc"), "/etc");
        assert_eq!(join("/home/me", "notes"), "/home/me/notes");
        assert_eq!(parent("/home/me"), Some("/home".into()));
        assert_eq!(parent("/home"), Some("/".into()));
        assert_eq!(parent("/home/me/"), Some("/home".into()));
        assert_eq!(parent("/"), None);
        assert_eq!(parent(""), None);
    }

    #[test]
    fn unsafe_names_are_refused() {
        for name in ["", ".", "..", "a/b", "nul\0"] {
            assert!(checked_name(name).is_err(), "{name:?}");
        }
        assert_eq!(checked_name("..hidden"), Ok("..hidden"));
    }

    #[test]
    fn control_characters_are_not_shown() {
        assert_eq!(printable("a\x1b[2Jb\nc"), "a\u{fffd}[2Jb\u{fffd}c");
    }

    #[test]
    fn sizes_and_modes_read_well() {
        assert_eq!(size(512), "512 B");
        assert_eq!(size(1536), "1.5 KiB");
        assert_eq!(size(20 * 1024 * 1024), "20 MiB");
        assert_eq!(mode(0o755), "rwxr-xr-x");
        assert_eq!(mode(0o640), "rw-r-----");
    }

    #[test]
    fn folders_sort_first() {
        let entry = |name: &str, kind| FileEntry {
            name: name.into(),
            kind,
            size: 0,
            modified: None,
            permissions: None,
        };
        let names: Vec<String> = sorted(vec![
            entry("b.txt", FileKind::File),
            entry("z", FileKind::Dir),
            entry("a.txt", FileKind::File),
        ])
        .into_iter()
        .map(|e| e.name)
        .collect();
        assert_eq!(names, ["z", "a.txt", "b.txt"]);
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_names_are_not_uploaded() {
        use std::os::unix::ffi::OsStrExt;
        let path = Path::new(std::ffi::OsStr::from_bytes(b"/tmp/bad\xff"));
        assert_eq!(upload_name(path), None);
        assert_eq!(upload_name(Path::new("/tmp/good")), Some("good".into()));
    }
}
