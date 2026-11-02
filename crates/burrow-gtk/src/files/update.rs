//! The panel's messages: what the user does, and what the core answers.

use burrow_core::ipc::Client;
use burrow_core::{Command, FileKind};
use relm4::gtk::prelude::*;
use relm4::{ComponentSender, gtk};

use crate::app::{App, AppMsg};
use crate::dialogs;
use crate::runtime::runtime;
use crate::session::{self, State};

use super::listing::sorted;
use super::names::{checked_name, join, parent, printable, upload_name};
use super::{Files, Msg, start};

use super::viewer;

impl Files {
    /// Handles a message for this panel. `window` is the parent of its
    /// dialogs.
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
            Msg::Listed { path, entries } => {
                self.show_error(None);
                // Listing the same folder again keeps what was selected.
                let same = path == self.path;
                let keep = self.selected().filter(|_| same).map(|e| e.name.clone());
                self.path = path;
                self.path_entry.set_text(&printable(&self.path));
                self.fill_list(sorted(entries), same, keep.as_deref());
                self.session.show_content();
            }
            Msg::Failed(error) => self.show_error(Some(&printable(&error))),
            Msg::Opened {
                name,
                path,
                contents,
            } => {
                // A file opened from an ended session can still be read.
                let save = self.session.running().map(|session| viewer::Save {
                    core: core.clone(),
                    session,
                    path,
                    saved: Box::new({
                        let sender = sender.clone();
                        move || {
                            sender.input(AppMsg::Files {
                                node,
                                msg: Msg::Refresh,
                            })
                        }
                    }),
                });
                viewer::show(window, &name, &contents, save);
            }
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
                    FileKind::File => self.open(entry.name.clone(), core, sender),
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
            Msg::Download => self.download(window, sender),
            Msg::Upload { folder } => self.upload(folder, window, sender),
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
            Msg::TransferStarted {
                transfer,
                session,
                label,
            } => {
                let gone = self.session.state != State::Running(session);
                let row = self.transfer_row(transfer, sender);
                row.what = Some(label);
                // Its session ended before the reply came, so no more
                // events will arrive for it.
                if gone && !row.ended {
                    row.stop("stopped: the connection ended");
                }
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
        }
    }
}
