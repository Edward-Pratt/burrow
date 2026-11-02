//! Downloads and uploads: asking where, starting them and showing how they go.

use std::time::Duration;

use burrow_core::ipc::Client;
use burrow_core::{Command, FileKind, Response, SessionId, TransferId, TransferState};
use relm4::gtk::prelude::*;
use relm4::gtk::{gio, glib};
use relm4::{ComponentSender, gtk};

use crate::app::{App, AppMsg};
use crate::dialogs;
use crate::runtime::runtime;

use super::names::{checked_name, join, printable, replace_question, size, upload_name};
use super::{Files, Msg, Transfer};

/// How long a finished transfer stays in the list.
pub(super) const DONE_TIME: Duration = Duration::from_secs(4);

impl Files {
    /// Asks where to save the selected entry, then starts the download,
    /// asking again before replacing what is there.
    pub(super) fn download(&self, window: &gtk::ApplicationWindow, sender: &ComponentSender<App>) {
        let node = self.node;
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
                    let (heading, detail, confirm) = replace_question(&name, is_dir, "folder");
                    dialogs::confirm(&window, &heading, detail, confirm, move || download(true));
                } else {
                    download(false);
                }
            },
        );
    }

    /// Asks for a file, or a folder, to put in the current folder, then
    /// starts the upload, asking again before replacing what is there.
    pub(super) fn upload(
        &self,
        folder: bool,
        window: &gtk::ApplicationWindow,
        sender: &ComponentSender<App>,
    ) {
        let node = self.node;
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
                    dialogs::confirm(&window, &heading, detail, confirm, move || upload(true));
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

    /// A transfer of this panel's session moved on.
    pub fn transfer_changed(
        &mut self,
        transfer: TransferId,
        state: &TransferState,
        core: &Client,
        sender: &ComponentSender<App>,
    ) {
        let node = self.node;
        let row = self.transfer_row(transfer, sender);
        // A row ends once. Events that come later, after its session
        // ended, say less than the row does.
        if row.ended {
            return;
        }
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
            TransferState::Failed { error } => row.stop(&printable(error)),
        }
        row.redraw();
        if *state == TransferState::Finished {
            // An upload may have added to the folder shown.
            self.list_dir(self.path.clone(), core, sender);
        }
    }

    /// The transfer's row, made on first use: its first event can arrive
    /// before the reply that started it.
    pub(super) fn transfer_row(
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

    pub(super) fn transfer(
        &self,
        label: String,
        command: impl FnOnce(SessionId) -> Command,
        core: &Client,
        sender: &ComponentSender<App>,
    ) {
        let Some(session) = self.session.running() else {
            return self.show_error(Some("Not connected."));
        };
        let command = command(session);
        let (core, input, node) = (core.clone(), sender.input_sender().clone(), self.node);
        runtime().spawn(async move {
            let msg = match core.execute(command).await {
                Ok(Response::Transfer { transfer }) => Msg::TransferStarted {
                    transfer,
                    session,
                    label,
                },
                Ok(other) => Msg::Failed(format!("unexpected reply {other:?}")),
                Err(e) => Msg::Failed(e.to_string()),
            };
            let _ = input.send(AppMsg::Files { node, msg });
        });
    }
}
