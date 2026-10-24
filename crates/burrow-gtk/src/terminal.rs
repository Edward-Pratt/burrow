//! Local terminal tabs. VTE starts and runs the shell itself, so the core is
//! not involved: a local shell has no connection or secrets to manage.

use std::collections::HashSet;
use std::path::Path;

use relm4::gtk;
use relm4::gtk::glib;
use relm4::gtk::prelude::*;
use vte4::prelude::*;

/// Entries in `/etc/shells` that are not meant for an interactive session.
const NOT_INTERACTIVE: [&str; 5] = [
    "nologin",
    "false",
    "git-shell",
    "rbash",
    "systemd-home-fallback-shell",
];

/// The user's login shell, from `$SHELL`.
pub fn default_shell() -> String {
    std::env::var("SHELL")
        .ok()
        .filter(|shell| !shell.is_empty())
        .unwrap_or_else(|| "/bin/sh".into())
}

/// Shells to offer in the palette: `$SHELL` first, then `/etc/shells`.
pub fn shells() -> Vec<String> {
    let listed = std::fs::read_to_string("/etc/shells").unwrap_or_default();
    parse_shells(&default_shell(), &listed)
        .into_iter()
        .filter(|path| Path::new(path).exists())
        .collect()
}

/// One entry per shell name. Distros list the same shell under `/bin` and
/// `/usr/bin`, and both are the same program.
fn parse_shells(login: &str, listed: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    std::iter::once(login)
        .chain(listed.lines().map(str::trim))
        .filter(|line| line.starts_with('/'))
        .filter(|path| {
            let name = shell_name(path);
            !NOT_INTERACTIVE.contains(&name) && seen.insert(name.to_string())
        })
        .map(String::from)
        .collect()
}

/// `zsh` for `/usr/bin/zsh`.
pub fn shell_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// A terminal running `shell` in the home folder.
///
/// `on_title` gets the title the shell sets (usually the current command or
/// folder), and `on_exit` runs when the shell exits. Neither may hold the
/// terminal itself: a closure stored on the terminal that owns the terminal
/// is a reference cycle, and the shell would outlive its tab.
pub fn new(
    shell: &str,
    on_title: impl Fn(String) + 'static,
    on_exit: impl Fn() + 'static,
) -> vte4::Terminal {
    let terminal = vte4::Terminal::new();
    terminal.set_hexpand(true);
    terminal.set_vexpand(true);
    terminal.set_scrollback_lines(10_000);
    terminal.set_scroll_on_keystroke(true);

    terminal.connect_window_title_changed(move |terminal| {
        if let Some(title) = terminal.window_title().filter(|t| !t.is_empty()) {
            on_title(title.to_string());
        }
    });
    terminal.connect_child_exited(move |_, _status| on_exit());
    add_clipboard_shortcuts(&terminal);

    let home = glib::home_dir();
    terminal.spawn_async(
        vte4::PtyFlags::DEFAULT,
        home.to_str(),
        &[shell],
        // Added to Burrow's own environment, not a replacement for it.
        &[],
        glib::SpawnFlags::DEFAULT,
        || {},
        -1,
        None::<&gtk::gio::Cancellable>,
        glib::clone!(
            #[weak]
            terminal,
            #[to_owned]
            shell,
            move |result| {
                if let Err(e) = result {
                    let message = format!("Could not start {shell}: {}\r\n", e.message());
                    terminal.feed(message.as_bytes());
                }
            }
        ),
    );
    terminal
}

/// Ctrl+Shift+C and Ctrl+Shift+V. VTE has no bindings of its own, and plain
/// Ctrl+C and Ctrl+V belong to the shell.
fn add_clipboard_shortcuts(terminal: &vte4::Terminal) {
    let controller = gtk::ShortcutController::new();
    // Before VTE's own key handler, which would send the keys to the shell.
    controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    type Run = fn(&vte4::Terminal);
    let shortcuts: [(&str, Run); 2] = [
        ("<Control><Shift>c", |t| {
            t.copy_clipboard_format(vte4::Format::Text)
        }),
        ("<Control><Shift>v", |t| t.paste_clipboard()),
    ];
    for (trigger, run) in shortcuts {
        // The action is handed the terminal it is attached to, so it needs no
        // reference of its own.
        let action = gtk::CallbackAction::new(move |widget, _| {
            if let Some(terminal) = widget.downcast_ref::<vte4::Terminal>() {
                run(terminal);
            }
            glib::Propagation::Stop
        });
        controller.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string(trigger),
            Some(action),
        ));
    }
    terminal.add_controller(controller);
}

/// The tab page around a terminal: the terminal plus a scrollbar.
pub fn page(terminal: &vte4::Terminal) -> gtk::ScrolledWindow {
    gtk::ScrolledWindow::builder()
        .child(terminal)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build()
}

/// The terminal inside a tab page, if the page is a terminal page.
pub fn in_page(page: &gtk::Widget) -> Option<vte4::Terminal> {
    page.downcast_ref::<gtk::ScrolledWindow>()?
        .child()?
        .downcast()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARCH: &str = "# Pathnames of valid login shells.\n\
        # See shells(5) for details.\n\n\
        /bin/sh\n/bin/bash\n/bin/rbash\n/usr/bin/sh\n/usr/bin/bash\n\
        /usr/bin/rbash\n/usr/bin/systemd-home-fallback-shell\n\
        /usr/bin/git-shell\n/bin/zsh\n/usr/bin/zsh\n";

    #[test]
    fn login_shell_comes_first() {
        assert_eq!(
            parse_shells("/usr/bin/zsh", ARCH),
            ["/usr/bin/zsh", "/bin/sh", "/bin/bash"]
        );
    }

    #[test]
    fn non_interactive_shells_are_skipped() {
        let shells = parse_shells("/usr/bin/nologin", ARCH);
        assert!(!shells.iter().any(|s| s.ends_with("nologin")));
        assert!(!shells.iter().any(|s| s.contains("git-shell")));
        assert!(!shells.iter().any(|s| s.contains("fallback")));
    }

    #[test]
    fn names_come_from_the_path() {
        assert_eq!(shell_name("/usr/bin/zsh"), "zsh");
        assert_eq!(shell_name("fish"), "fish");
    }
}
