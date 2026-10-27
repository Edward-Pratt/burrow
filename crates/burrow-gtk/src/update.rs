//! "Check for updates…": runs `burrow-update`, the install script, which
//! lives beside the binaries it installed. The window only runs it and
//! shows what it says. Updates stay out of the core, so nothing that talks
//! to the daemon, such as the MCP bridge, can start one.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::OnceLock;

use relm4::gtk;
use relm4::gtk::prelude::*;
use relm4::gtk::{gio, glib};

use crate::dialogs;
use crate::files;

/// The binary to start in place of this one when the window has closed.
static RESTART: OnceLock<PathBuf> = OnceLock::new();

/// Starts `program` once the app has quit. Called as the window goes, so
/// a close the user cancelled restarts nothing.
pub fn restart_into(program: PathBuf) {
    let _ = RESTART.set(program);
}

type Restart = Rc<dyn Fn(PathBuf)>;

/// After the app quits: starts the updated window, if the user asked. The
/// old process is replaced rather than followed, so its D-Bus name is
/// gone before the new window asks for it.
pub fn restart_if_asked() {
    if let Some(program) = RESTART.get() {
        use std::os::unix::process::CommandExt;
        let error = std::process::Command::new(program).exec();
        eprintln!("burrow: could not start {}: {error}", program.display());
    }
}

/// The folder this binary was started from. Once the update has replaced
/// the file, Linux names it "… (deleted)".
fn bin_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe = exe.to_str()?.trim_end_matches(" (deleted)");
    Some(Path::new(exe).parent()?.to_path_buf())
}

/// What `burrow-update --check` printed.
#[derive(Debug, PartialEq)]
struct Check {
    follow: String,
    installed: String,
    available: String,
    /// The release's tag, when following tagged releases.
    tag: Option<String>,
}

fn parse_check(output: &str) -> Option<Check> {
    let field = |name: &str| {
        output.lines().find_map(|line| {
            let value = line.strip_prefix(name)?.strip_prefix(' ')?;
            Some(files::printable(value.trim()))
        })
    };
    Some(Check {
        follow: field("follow")?,
        installed: field("installed")?,
        available: field("available")?,
        tag: field("tag"),
    })
}

fn short(commit: &str) -> &str {
    commit.get(..7).unwrap_or(commit)
}

/// The last lines of a script's output, for when it failed.
fn tail(output: &str, lines: usize) -> String {
    let all: Vec<&str> = output.lines().collect();
    let start = all.len().saturating_sub(lines);
    let shown: Vec<String> = all[start..].iter().map(|l| files::printable(l)).collect();
    shown.join("\n")
}

struct Dialog {
    window: gtk::Window,
    spinner: gtk::Spinner,
    message: gtk::Label,
    detail: gtk::Label,
    close: gtk::Button,
    action: gtk::Button,
    /// What the action button runs now.
    clicked: RefCell<Option<glib::SignalHandlerId>>,
}

impl Dialog {
    fn busy(&self, message: &str) {
        self.spinner.set_visible(true);
        self.spinner.start();
        self.message.set_text(message);
        self.detail.set_text("");
        self.action.set_visible(false);
    }

    fn done(&self, message: &str, detail: &str) {
        self.spinner.stop();
        self.spinner.set_visible(false);
        self.message.set_text(message);
        self.detail.set_text(detail);
    }

    /// Shows `label` on the action button, and runs `run` when it's clicked,
    /// in place of what it ran before.
    fn offer(&self, label: &str, run: impl Fn() + 'static) {
        self.action.set_label(label);
        self.action.set_visible(true);
        self.action.grab_focus();
        if let Some(previous) = self.clicked.take() {
            self.action.disconnect(previous);
        }
        let handler = self.action.connect_clicked(move |_| run());
        self.clicked.replace(Some(handler));
    }
}

/// Opens the dialog. `restart` closes the window the usual way, then
/// starts the program it's given.
pub fn show(parent: &gtk::ApplicationWindow, restart: impl Fn(PathBuf) + 'static) {
    let window = gtk::Window::builder()
        .title("Updates")
        .modal(true)
        .transient_for(parent)
        .resizable(false)
        .default_width(420)
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 10);
    content.set_margin_top(18);
    content.set_margin_bottom(18);
    content.set_margin_start(18);
    content.set_margin_end(18);

    let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    let spinner = gtk::Spinner::new();
    let message = gtk::Label::new(None);
    message.set_xalign(0.0);
    message.set_wrap(true);
    message.set_hexpand(true);
    row.append(&spinner);
    row.append(&message);
    content.append(&row);

    let detail = gtk::Label::new(None);
    detail.set_xalign(0.0);
    detail.set_wrap(true);
    detail.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    detail.set_selectable(true);
    detail.set_yalign(0.0);
    detail.add_css_class("dim-label");
    // A fixed height, so the dialog keeps its size as the text changes,
    // and a failed update's output scrolls.
    let scroll = gtk::ScrolledWindow::builder()
        .child(&detail)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .min_content_height(120)
        .build();
    content.append(&scroll);

    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::End);
    buttons.set_margin_top(6);
    let close = gtk::Button::with_label("Close");
    let action = gtk::Button::new();
    action.add_css_class("suggested-action");
    action.set_visible(false);
    buttons.append(&close);
    buttons.append(&action);
    content.append(&buttons);
    window.set_child(Some(&content));
    dialogs::close_on_cancel(&window, &close);
    dialogs::close_on_escape(&window);

    let dialog = Rc::new(Dialog {
        window: window.clone(),
        spinner,
        message,
        detail,
        close,
        action,
        clicked: RefCell::new(None),
    });
    // What the action button runs holds the dialog, which holds the
    // button, so it goes when the window closes, and the dialog with it.
    let weak = Rc::downgrade(&dialog);
    window.connect_close_request(move |_| {
        if let Some(dialog) = weak.upgrade()
            && let Some(handler) = dialog.clicked.take()
        {
            dialog.action.disconnect(handler);
        }
        glib::Propagation::Proceed
    });
    window.present();

    // Only the install script's own install can update itself. A pacman
    // package or a build from a checkout has none beside it.
    let Some(script) = bin_dir()
        .map(|dir| dir.join("burrow-update"))
        .filter(|script| script.is_file())
    else {
        dialog.done(
            "This copy of Burrow can't update itself.",
            "Only an install made with dist/install.sh can. Update this one the way it \
             was installed, such as with makepkg for the pacman package.",
        );
        return;
    };
    glib::spawn_future_local(check(dialog, script, Rc::new(restart)));
}

async fn check(dialog: Rc<Dialog>, script: PathBuf, restart: Restart) {
    dialog.busy("Checking for updates…");
    let (ok, output) = run(&script, &["--check"], |_| {}).await;
    let Some(check) = parse_check(&output).filter(|_| ok) else {
        dialog.done("Burrow couldn't check for updates.", &tail(&output, 12));
        return;
    };
    if check.installed == check.available {
        dialog.done(
            "Burrow is up to date.",
            &format!("Installed: {}", short(&check.installed)),
        );
        return;
    }
    let detail = if let Some(tag) = &check.tag {
        format!(
            "Installed: {}\nNewest release: {tag}",
            short(&check.installed)
        )
    } else if check.follow == "release" {
        format!(
            "Installed: {}\nNewest: {}",
            short(&check.installed),
            short(&check.available)
        )
    } else {
        format!(
            "Installed: {}\nNewest on {}: {}\nIt builds from source, which takes a few minutes.",
            short(&check.installed),
            check.follow,
            short(&check.available)
        )
    };
    dialog.done("An update is ready.", &detail);
    let again = dialog.clone();
    dialog.offer("Update", move || {
        glib::spawn_future_local(update(again.clone(), script.clone(), restart.clone()));
    });
}

async fn update(dialog: Rc<Dialog>, script: PathBuf, restart: Restart) {
    dialog.busy("Updating…");
    let shown = dialog.detail.clone();
    let (ok, output) = run(&script, &[], move |line| {
        shown.set_text(&files::printable(line))
    })
    .await;
    if !ok {
        dialog.done("The update failed.", &tail(&output, 12));
        return;
    }
    dialog.done("Burrow is updated.", "Restart it to use the new version.");
    dialog.close.set_label("Later");
    let Some(window) = bin_dir().map(|dir| dir.join("burrow-gtk")) else {
        return;
    };
    let this = dialog.clone();
    dialog.offer("Restart", move || {
        this.window.close();
        restart(window.clone());
    });
}

/// Runs `script` with `args`, passing each line it prints to `line`, and
/// returns whether it succeeded, with everything it printed.
async fn run(script: &Path, args: &[&str], line: impl Fn(&str)) -> (bool, String) {
    let mut argv: Vec<&std::ffi::OsStr> = vec![script.as_os_str()];
    argv.extend(args.iter().map(std::ffi::OsStr::new));
    let flags = gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_MERGE;
    let process = match gio::Subprocess::newv(&argv, flags) {
        Ok(process) => process,
        Err(e) => return (false, e.to_string()),
    };
    let mut output = String::new();
    if let Some(pipe) = process.stdout_pipe() {
        let lines = gio::DataInputStream::new(&pipe);
        while let Ok(Some(text)) = lines.read_line_utf8_future(glib::Priority::DEFAULT).await {
            line(&text);
            output.push_str(&text);
            output.push('\n');
        }
    }
    let ok = process.wait_future().await.is_ok() && process.is_successful();
    (ok, output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_check_names_what_is_installed_and_available() {
        let output = "follow release\ninstalled abc1234def\navailable 0123456789\n";
        assert_eq!(
            parse_check(output),
            Some(Check {
                follow: "release".into(),
                installed: "abc1234def".into(),
                available: "0123456789".into(),
                tag: None,
            })
        );
        let stable = "follow stable\ninstalled abc1234def\navailable 0123456789\ntag v0.2.0\n";
        assert_eq!(
            parse_check(stable).and_then(|c| c.tag),
            Some("v0.2.0".into())
        );
        assert_eq!(parse_check("Another Burrow update is running.\n"), None);
    }

    #[test]
    fn a_failure_shows_its_last_lines() {
        assert_eq!(tail("a\nb\nc\n", 2), "b\nc");
        assert_eq!(tail("", 2), "");
    }
}
