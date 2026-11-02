//! SFTP file panels: a tab that browses an SSH host's files, shows what is
//! in them, and copies files and folders to and from this computer.
//!
//! Names come from the server, so they are cleaned up before they are
//! shown, and checked before they become part of a local path. Remote
//! paths are always built from the names as the server sent them.

use std::collections::HashMap;
use std::path::PathBuf;

use burrow_core::ipc::Client;
use burrow_core::{Command, FileEntry, NodeId, Response, SessionId, SessionState, TransferId};
use relm4::gtk::prelude::*;
use relm4::{ComponentSender, gtk};

use crate::app::{App, AppMsg};
use crate::runtime::runtime;
use crate::session::{self, SessionPage};

mod listing;
mod names;
mod page;
mod transfers;
mod update;

mod viewer;

use names::join;

pub(crate) use names::{printable, size};

#[derive(Clone, Debug)]
pub enum Msg {
    /// `open_files` and what follows.
    Session(session::Msg),
    Listed {
        path: String,
        entries: Vec<FileEntry>,
    },
    /// A command failed. Shown above the list.
    Failed(String),
    /// A file's start arrived, to show in a viewer.
    Opened {
        name: String,
        path: String,
        contents: viewer::Contents,
    },
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
        /// The session it runs on.
        session: SessionId,
        label: String,
    },
    /// A transfer's button: cancel while it runs, dismiss once it failed.
    TransferButton(TransferId),
    Dismiss(TransferId),
}

/// One SSH host's file panel.
pub struct Files {
    node: NodeId,
    pub page: gtk::Box,
    pub session: SessionPage,
    /// The folder shown, as the server names it. Empty until the first
    /// listing arrives.
    path: String,
    /// The entries shown, in row order.
    entries: Vec<FileEntry>,
    path_entry: gtk::Entry,
    scroller: gtk::ScrolledWindow,
    list: gtk::ListBox,
    /// The last row, saying how many entries aren't drawn. Hidden while
    /// they all are.
    more_row: gtk::ListBoxRow,
    more: gtk::Label,
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
    /// Shows that the transfer stopped without finishing, and turns its
    /// button into "Dismiss".
    fn stop(&mut self, status: &str) {
        self.ended = true;
        self.status = status.to_string();
        self.label.add_css_class("error-text");
        self.button.set_label("Dismiss");
        self.button.set_visible(true);
    }

    fn redraw(&self) {
        let what = self.what.as_deref().unwrap_or("Copying");
        self.label.set_label(&format!("{what}: {}", self.status));
    }
}

impl Files {
    fn show_error(&self, error: Option<&str>) {
        self.error.set_label(error.unwrap_or_default());
        self.error.set_visible(error.is_some());
    }

    /// The session's state changed.
    pub fn session_changed(
        &mut self,
        state: &SessionState,
        core: &Client,
        sender: &ComponentSender<App>,
    ) {
        if matches!(state, SessionState::Closed { .. }) {
            // The core cancels them, but their last events can arrive
            // after this, or not at all.
            for row in self.transfers.values_mut().filter(|row| !row.ended) {
                row.stop("stopped: the connection ended");
                row.redraw();
            }
        }
        let ended = |error: Option<&str>| session::ended("The connection ended", error);
        if self.session.session_changed(state, ended) {
            self.list_dir(self.path.clone(), core, sender);
        }
    }

    /// Whether the transfer is shown here.
    pub fn has_transfer(&self, transfer: TransferId) -> bool {
        self.transfers.contains_key(&transfer)
    }

    fn selected(&self) -> Option<&FileEntry> {
        let row = self.list.selected_row()?;
        self.entries.get(usize::try_from(row.index()).ok()?)
    }

    /// Lists `path`, or the home folder when it is empty.
    fn list_dir(&self, path: String, core: &Client, sender: &ComponentSender<App>) {
        let Some(session) = self.session.running() else {
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

    /// Reads the start of the file called `name` in the folder shown. It
    /// arrives as [`Msg::Opened`].
    fn open(&self, name: String, core: &Client, sender: &ComponentSender<App>) {
        let Some(session) = self.session.running() else {
            return self.show_error(Some("Not connected."));
        };
        let path = join(&self.path, &name);
        let command = Command::ReadFile {
            session,
            path: path.clone(),
            limit: Some(viewer::LIMIT),
        };
        let (core, input, node) = (core.clone(), sender.input_sender().clone(), self.node);
        runtime().spawn(async move {
            let msg = match core.execute(command).await {
                // Decoded here, away from the GTK thread.
                Ok(Response::File { data, size, sha256 }) => Msg::Opened {
                    name,
                    path,
                    contents: viewer::Contents::new(&data, size, sha256),
                },
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
        core: &Client,
        sender: &ComponentSender<App>,
    ) {
        let Some(session) = self.session.running() else {
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
}

/// Asks the core for an SFTP session on `node`.
pub fn start(core: &Client, node: NodeId, sender: &ComponentSender<App>) {
    let wrap = move |msg| AppMsg::Files {
        node,
        msg: Msg::Session(msg),
    };
    session::start(core, Command::OpenFiles { node }, sender, wrap);
}
