//! The "New SSH host" dialog. A host's settings are either typed here, and
//! the core writes them to the OpenSSH config, or already in that config,
//! and the node only names the alias.
//!
//! The dialog stays open while the core answers, so a refusal, such as an
//! alias that is already taken, shows next to the fields.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use burrow_core::ipc::Client;
use burrow_core::{Command, NodeId, Response, SSH_HOST_KIND};
use relm4::gtk;
use relm4::gtk::prelude::*;
use relm4::gtk::{gio, glib};

use crate::dialogs;
use crate::runtime::runtime;

/// What the form asks the core for.
#[derive(Debug, PartialEq)]
enum Request {
    /// Write a new host to the OpenSSH config.
    Write {
        alias: String,
        hostname: String,
        user: Option<String>,
        port: Option<u16>,
        key: Option<PathBuf>,
    },
    /// Use an alias the OpenSSH config already has.
    Existing { alias: String },
}

/// The fields as typed, trimmed.
#[derive(Default)]
struct Typed {
    write: bool,
    alias: String,
    hostname: String,
    user: String,
    port: String,
    key: String,
}

impl Typed {
    /// Checks only what the user can fix at a glance. The core checks the
    /// characters, and whether the alias is free.
    fn request(self) -> Result<Request, &'static str> {
        if self.alias.is_empty() {
            return Err("Give the host an alias, the name you'll use for it.");
        }
        if !self.write {
            return Ok(Request::Existing { alias: self.alias });
        }
        if self.hostname.is_empty() {
            return Err("Say which server to connect to: its host name or IP address.");
        }
        let port = match self.port.as_str() {
            "" => None,
            text => match text.parse::<u16>() {
                Ok(port) if port > 0 => Some(port),
                _ => return Err("The port is a number from 1 to 65535."),
            },
        };
        let optional = |text: String| (!text.is_empty()).then_some(text);
        Ok(Request::Write {
            alias: self.alias,
            hostname: self.hostname,
            user: optional(self.user),
            port,
            key: optional(self.key).map(PathBuf::from),
        })
    }
}

/// `~/…` for paths in the home folder, as people know them.
fn shown(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) if !home.as_os_str().is_empty() => format!("~/{}", rest.display()),
        _ => path.display().to_string(),
    }
}

/// Runs `command` on the core from the GTK main loop.
async fn run(core: &Client, command: Command) -> Result<Response, String> {
    let core = core.clone();
    runtime()
        .spawn(async move { core.execute(command).await })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

/// Shows the dialog. The new node arrives as a `node_created` event like
/// any other.
pub fn new_host(parent_window: &impl IsA<gtk::Window>, core: Client, parent: Option<NodeId>) {
    let window = gtk::Window::builder()
        .title("New SSH host")
        .modal(true)
        .transient_for(parent_window)
        .resizable(false)
        .default_width(440)
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_margin_top(18);
    content.set_margin_bottom(18);
    content.set_margin_start(18);
    content.set_margin_end(18);

    let write = gtk::CheckButton::with_label("Enter its settings here");
    let existing = gtk::CheckButton::with_label("Use a host already in ~/.ssh/config");
    existing.set_group(Some(&write));
    write.set_active(true);
    content.append(&write);
    content.append(&existing);

    let grid = gtk::Grid::builder()
        .row_spacing(8)
        .column_spacing(12)
        .build();
    let label = |text: &str| {
        let label = gtk::Label::new(Some(text));
        label.set_xalign(0.0);
        label
    };
    let entry = |placeholder: &str| {
        let entry = gtk::Entry::new();
        entry.set_hexpand(true);
        entry.set_placeholder_text(Some(placeholder));
        entry
    };

    let alias = entry("The name for ssh, such as web");
    grid.attach(&label("Alias"), 0, 0, 1, 1);
    grid.attach(&alias, 1, 0, 2, 1);

    let hostname = entry("Such as example.com or 192.0.2.10");
    let user = entry("The same as on this computer");
    let port = entry("22");
    let key = entry("Your usual keys");
    let choose = gtk::Button::with_label("Choose…");
    let rows: [(&str, &gtk::Entry); 4] = [
        ("Host name", &hostname),
        ("User", &user),
        ("Port", &port),
        ("Key", &key),
    ];
    let mut settings: Vec<gtk::Widget> = Vec::new();
    for (row, (text, entry)) in rows.into_iter().enumerate() {
        let row = row as i32 + 1;
        let label = label(text);
        let width = if text == "Key" { 1 } else { 2 };
        grid.attach(&label, 0, row, 1, 1);
        grid.attach(entry, 1, row, width, 1);
        settings.push(label.upcast());
        settings.push(entry.clone().upcast());
    }
    grid.attach(&choose, 2, 4, 1, 1);
    settings.push(choose.clone().upcast());
    content.append(&grid);

    let note = gtk::Label::new(None);
    note.set_xalign(0.0);
    note.set_wrap(true);
    note.set_max_width_chars(56);
    note.add_css_class("dim");
    content.append(&note);

    let error = gtk::Label::new(None);
    error.set_xalign(0.0);
    error.set_wrap(true);
    error.set_max_width_chars(56);
    error.add_css_class("error-text");
    error.set_visible(false);
    content.append(&error);

    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::End);
    buttons.set_margin_top(6);
    let cancel = gtk::Button::with_label("Cancel");
    let ok = gtk::Button::with_label("Create");
    ok.add_css_class("suggested-action");
    buttons.append(&cancel);
    buttons.append(&ok);
    content.append(&buttons);
    window.set_child(Some(&content));

    let show_mode = {
        let note = note.clone();
        move |write: &gtk::CheckButton| {
            let on = write.is_active();
            for widget in &settings {
                widget.set_visible(on);
            }
            note.set_label(if on {
                "Burrow saves these settings in ~/.ssh/config.d/burrow.conf, so \
                 “ssh <alias>” in a terminal connects the same way."
            } else {
                "Burrow connects with the settings ssh uses for this alias."
            });
        }
    };
    show_mode(&write);
    write.connect_toggled(show_mode);

    choose.connect_clicked({
        let (window, key) = (window.clone(), key.clone());
        move |_| {
            let dialog = gtk::FileDialog::builder()
                .title("Choose a private key")
                .accept_label("Choose")
                .modal(true)
                .build();
            let ssh = glib::home_dir().join(".ssh");
            if ssh.is_dir() {
                dialog.set_initial_folder(Some(&gio::File::for_path(ssh)));
            }
            let key = key.clone();
            dialog.open(Some(&window), None::<&gio::Cancellable>, move |result| {
                if let Some(path) = result.ok().and_then(|file| file.path()) {
                    key.set_text(&path.to_string_lossy());
                }
            });
        }
    });

    let submit = Rc::new({
        let window = window.clone();
        let (write, alias, hostname, user, port, key) = (
            write.clone(),
            alias.clone(),
            hostname.clone(),
            user.clone(),
            port.clone(),
            key.clone(),
        );
        let (ok, error) = (ok.clone(), error.clone());
        move || {
            if !ok.is_sensitive() {
                return;
            }
            let text = |entry: &gtk::Entry| entry.text().trim().to_string();
            let typed = Typed {
                write: write.is_active(),
                alias: text(&alias),
                hostname: text(&hostname),
                user: text(&user),
                port: text(&port),
                key: text(&key),
            };
            let request = match typed.request() {
                Ok(request) => request,
                Err(message) => {
                    error.set_label(message);
                    error.set_visible(true);
                    return;
                }
            };
            error.set_visible(false);
            ok.set_sensitive(false);
            let (window, core, ok, error) =
                (window.clone(), core.clone(), ok.clone(), error.clone());
            glib::spawn_future_local(async move {
                match create(&window, &core, parent, request).await {
                    Ok(()) => window.close(),
                    Err(message) => {
                        error.set_label(&message);
                        error.set_visible(true);
                        ok.set_sensitive(true);
                    }
                }
            });
        }
    });
    ok.connect_clicked({
        let submit = submit.clone();
        move |_| submit()
    });
    for entry in [&alias, &hostname, &user, &port, &key] {
        let submit = submit.clone();
        entry.connect_activate(move |_| submit());
    }
    cancel.connect_clicked({
        let window = window.clone();
        move |_| window.close()
    });
    dialogs::close_on_escape(&window);

    window.present();
    alias.grab_focus();
}

/// Sends the request, first asking to add the `Include` line when the
/// OpenSSH config doesn't read Burrow's file yet.
async fn create(
    window: &gtk::Window,
    core: &Client,
    parent: Option<NodeId>,
    request: Request,
) -> Result<(), String> {
    let command = match request {
        Request::Existing { alias } => {
            let mut props = toml::Table::new();
            props.insert("alias".into(), alias.clone().into());
            Command::CreateNode {
                parent,
                kind: SSH_HOST_KIND.into(),
                name: alias,
                color: None,
                props,
            }
        }
        Request::Write {
            alias,
            hostname,
            user,
            port,
            key,
        } => {
            let check = Command::CheckSshInclude {
                alias: alias.clone(),
            };
            let Response::SshInclude {
                included,
                config,
                line,
            } = run(core, check).await?
            else {
                return Err("Burrow's core gave an unexpected answer.".into());
            };
            if !included && !ask_include(window, &alias, &config, &line).await {
                let config = shown(&config, &glib::home_dir());
                return Err(format!(
                    "The host wasn't added. Burrow needs that line to add hosts; \
                     you can also put it at the top of {config} yourself:\n{line}"
                ));
            }
            Command::AddSshHost {
                parent,
                alias,
                hostname,
                user,
                port,
                identity_file: key,
                add_include: !included,
            }
        }
    };
    run(core, command).await.map(drop)
}

async fn ask_include(window: &gtk::Window, alias: &str, config: &Path, line: &str) -> bool {
    let config = shown(config, &glib::home_dir());
    let dialog = gtk::AlertDialog::builder()
        .modal(true)
        .message(format!("Add a line to {config}?"))
        .detail(format!(
            "Burrow saves the hosts you add in a file of its own. This line at the top \
             of {config} makes ssh read that file, so “ssh {alias}” in a terminal \
             connects the same way:\n\n{line}\n\nBurrow can't add hosts without it, \
             and won't ask again once it is there."
        ))
        .buttons(["Cancel", "Add the line"])
        .cancel_button(0)
        // Changing someone's SSH config takes a deliberate click.
        .default_button(0)
        .build();
    dialog.choose_future(Some(window)).await == Ok(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(alias: &str, hostname: &str) -> Typed {
        Typed {
            write: true,
            alias: alias.into(),
            hostname: hostname.into(),
            ..Default::default()
        }
    }

    #[test]
    fn empty_optional_fields_are_left_out() {
        assert_eq!(
            typed("web", "example.com").request(),
            Ok(Request::Write {
                alias: "web".into(),
                hostname: "example.com".into(),
                user: None,
                port: None,
                key: None,
            })
        );
        let full = Typed {
            user: "deploy".into(),
            port: "2222".into(),
            key: "/home/u/.ssh/id".into(),
            ..typed("web", "example.com")
        };
        let Ok(Request::Write {
            user, port, key, ..
        }) = full.request()
        else {
            panic!("expected a host to write");
        };
        assert_eq!(user.as_deref(), Some("deploy"));
        assert_eq!(port, Some(2222));
        assert_eq!(key, Some(PathBuf::from("/home/u/.ssh/id")));
    }

    #[test]
    fn missing_or_bad_fields_are_explained() {
        assert!(typed("", "example.com").request().is_err());
        assert!(typed("web", "").request().is_err());
        for port in ["0", "65536", "ssh", "-1"] {
            let typed = Typed {
                port: port.into(),
                ..typed("web", "example.com")
            };
            assert!(typed.request().is_err(), "{port}");
        }
    }

    #[test]
    fn an_existing_alias_needs_only_the_alias() {
        let existing = Typed {
            write: false,
            ..typed("web", "")
        };
        assert_eq!(
            existing.request(),
            Ok(Request::Existing {
                alias: "web".into()
            })
        );
    }

    #[test]
    fn paths_in_the_home_folder_are_shown_with_a_tilde() {
        let home = Path::new("/home/u");
        assert_eq!(
            shown(Path::new("/home/u/.ssh/config"), home),
            "~/.ssh/config"
        );
        assert_eq!(shown(Path::new("/etc/ssh/config"), home), "/etc/ssh/config");
        assert_eq!(shown(Path::new("/home/u2/config"), home), "/home/u2/config");
    }
}
