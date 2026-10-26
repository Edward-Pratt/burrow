//! Terminal tabs. For a local shell, VTE starts and runs the shell itself,
//! so the core is not involved. For an SSH host, the core runs the session
//! and VTE only draws: output is fed in, and keystrokes go back to the core.

use std::cell::Cell;
use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use relm4::gtk;
use relm4::gtk::prelude::*;
use relm4::gtk::{gdk, gio, glib, pango};
use vte4::prelude::*;

/// PCRE2 compile flags, which VTE takes as plain numbers. VTE wants
/// `MULTILINE` on every regex it is given.
const PCRE2_CASELESS: u32 = 0x0000_0008;
const PCRE2_MULTILINE: u32 = 0x0000_0400;

/// Web and mail addresses in the output. A sentence often ends just after
/// a link, so the last character can't be punctuation.
const URL_PATTERN: &str = r#"(?:https?://|mailto:)[^\s<>"'`]*[^\s<>"'`.,;:!?)\]}]"#;

/// How much one Ctrl+= or Ctrl+- changes the text size, and its limits.
const ZOOM_STEP: f64 = 1.1;
const ZOOM_RANGE: (f64, f64) = (0.5, 3.0);

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
    let terminal = base(on_title);
    terminal.connect_child_exited(move |_, _status| on_exit());

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

/// A terminal with no process of its own, for a session the core runs.
///
/// `on_input` gets what the user types (and VTE's replies to terminal
/// queries). `on_resize` gets the size in columns and rows whenever it
/// changes. The same rule as [`new`] applies: the callbacks must not hold
/// the terminal.
pub fn remote(
    on_title: impl Fn(String) + 'static,
    on_input: impl Fn(Vec<u8>) + 'static,
    on_resize: impl Fn(u16, u16) + 'static,
) -> vte4::Terminal {
    let terminal = base(on_title);
    terminal.connect_commit(move |_, text, _| on_input(text.as_bytes().to_vec()));

    // VTE has no signal for "the grid changed size" when it has no PTY of
    // its own, so check a few times a second. The timer holds the terminal
    // weakly and stops when it is gone.
    let last = Cell::new((0, 0));
    let weak = terminal.downgrade();
    glib::timeout_add_local(Duration::from_millis(200), move || {
        let Some(terminal) = weak.upgrade() else {
            return glib::ControlFlow::Break;
        };
        let size = size(&terminal);
        if terminal.is_mapped() && size != last.get() {
            last.set(size);
            on_resize(size.0, size.1);
        }
        glib::ControlFlow::Continue
    });
    terminal
}

/// Columns and rows.
pub fn size(terminal: &vte4::Terminal) -> (u16, u16) {
    let clamp = |n: std::ffi::c_long| n.clamp(1, u16::MAX.into()) as u16;
    (clamp(terminal.column_count()), clamp(terminal.row_count()))
}

/// Settings shared by local and remote terminals.
fn base(on_title: impl Fn(String) + 'static) -> vte4::Terminal {
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
    add_shortcuts(&terminal);
    add_links(&terminal);
    terminal
}

type Run = fn(&vte4::Terminal);

/// Ctrl+Shift+C and Ctrl+Shift+V, and zoom. VTE has no bindings of its
/// own, and plain Ctrl+C and Ctrl+V belong to the shell.
fn add_shortcuts(terminal: &vte4::Terminal) {
    let shortcuts: [(&str, Run); 8] = [
        ("<Control><Shift>c", |t| {
            t.copy_clipboard_format(vte4::Format::Text)
        }),
        ("<Control><Shift>v", |t| t.paste_clipboard()),
        ("<Control>equal", |t| zoom(t, ZOOM_STEP)),
        ("<Control>plus", |t| zoom(t, ZOOM_STEP)),
        ("<Control>KP_Add", |t| zoom(t, ZOOM_STEP)),
        ("<Control>minus", |t| zoom(t, 1.0 / ZOOM_STEP)),
        ("<Control>KP_Subtract", |t| zoom(t, 1.0 / ZOOM_STEP)),
        ("<Control>0", |t| t.set_font_scale(1.0)),
    ];
    add_shortcut_controller(terminal, &shortcuts);
}

/// Runs each shortcut on `terminal` before VTE's own key handler, which
/// would send the keys to the shell.
fn add_shortcut_controller(terminal: &vte4::Terminal, shortcuts: &[(&str, Run)]) {
    let controller = gtk::ShortcutController::new();
    controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    for &(trigger, run) in shortcuts {
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

fn zoom(terminal: &vte4::Terminal, by: f64) {
    let scale = (terminal.font_scale() * by).clamp(ZOOM_RANGE.0, ZOOM_RANGE.1);
    terminal.set_font_scale(scale);
}

/// Sets the terminal's font from a Pango description such as
/// "Monospace 11", or to the system's monospace font for `None`.
pub fn set_font(terminal: &vte4::Terminal, font: Option<&str>) {
    let font = font.map(pango::FontDescription::from_string);
    terminal.set_font(font.as_ref());
}

/// Links in the output open with Ctrl+click: addresses VTE finds in the
/// text, and OSC 8 links, which show other text than where they go, so
/// their tooltip says where.
fn add_links(terminal: &vte4::Terminal) {
    terminal.set_allow_hyperlink(true);
    let regex = vte4::Regex::for_match(URL_PATTERN, PCRE2_CASELESS | PCRE2_MULTILINE)
        .expect("the URL pattern is valid");
    let tag = terminal.match_add_regex(&regex, 0);
    terminal.match_set_cursor_name(tag, "pointer");

    let click = gtk::GestureClick::new();
    click.set_button(gdk::BUTTON_PRIMARY);
    // Before VTE starts a selection with the same click.
    click.set_propagation_phase(gtk::PropagationPhase::Capture);
    click.connect_pressed(|gesture, _, x, y| {
        if !(gesture.current_event_state()).contains(gdk::ModifierType::CONTROL_MASK) {
            return;
        }
        let Some(terminal) = gesture.widget().and_downcast::<vte4::Terminal>() else {
            return;
        };
        let uri = (terminal.check_hyperlink_at(x, y)).or_else(|| terminal.check_match_at(x, y).0);
        let Some(uri) = uri.filter(|uri| openable(uri)) else {
            return;
        };
        gesture.set_state(gtk::EventSequenceState::Claimed);
        let window = terminal.root().and_downcast::<gtk::Window>();
        gtk::UriLauncher::new(&uri).launch(window.as_ref(), None::<&gio::Cancellable>, |_| {});
    });
    terminal.add_controller(click);

    terminal.set_has_tooltip(true);
    terminal.connect_query_tooltip(|terminal, x, y, _, tooltip| {
        let Some(uri) = terminal.check_hyperlink_at(x.into(), y.into()) else {
            return false;
        };
        let uri: String = uri.chars().filter(|c| !c.is_control()).collect();
        if openable(&uri) {
            tooltip.set_text(Some(&format!("{uri}\nCtrl+click to open")));
        } else {
            tooltip.set_text(Some(&uri));
        }
        true
    });
}

/// Whether a click may open `uri`. Output comes from programs and servers
/// Burrow doesn't trust, and OSC 8 can link any URI, such as a `file:` path
/// or another program's own scheme, so only web and mail links open.
fn openable(uri: &str) -> bool {
    let scheme = uri
        .split_once(':')
        .map(|(scheme, _)| scheme.to_ascii_lowercase());
    matches!(scheme.as_deref(), Some("http" | "https" | "mailto"))
}

/// The tab page around a terminal: a find bar, then the terminal with a
/// scrollbar.
pub fn page(terminal: &vte4::Terminal) -> gtk::Box {
    let scroller = gtk::ScrolledWindow::builder()
        .child(terminal)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .build();
    let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
    page.append(&find_bar(terminal));
    page.append(&scroller);
    page
}

/// A bar that finds text in the terminal, shown with Ctrl+Shift+F. Finding
/// goes up first, since the newest output is at the bottom.
fn find_bar(terminal: &vte4::Terminal) -> gtk::SearchBar {
    let entry = gtk::SearchEntry::builder()
        .placeholder_text("Find")
        .hexpand(true)
        .build();
    let older = gtk::Button::from_icon_name("go-up-symbolic");
    older.set_tooltip_text(Some("Find older (Enter)"));
    let newer = gtk::Button::from_icon_name("go-down-symbolic");
    newer.set_tooltip_text(Some("Find newer (Shift+Enter)"));
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    row.add_css_class("linked");
    row.append(&entry);
    row.append(&older);
    row.append(&newer);
    let bar = gtk::SearchBar::builder()
        .child(&row)
        .show_close_button(true)
        .build();
    bar.connect_entry(&entry);
    terminal.search_set_wrap_around(true);

    // The terminal holds the bar through its shortcut, so the bar's own
    // handlers hold the terminal weakly.
    let weak = terminal.downgrade();
    let find = move |entry: &gtk::SearchEntry, older: bool| {
        let Some(terminal) = weak.upgrade() else {
            return;
        };
        let found = if older {
            terminal.search_find_previous()
        } else {
            terminal.search_find_next()
        };
        if found || entry.text().is_empty() {
            entry.remove_css_class("error");
        } else {
            entry.add_css_class("error");
        }
    };
    let find = std::rc::Rc::new(find);

    entry.connect_search_changed(glib::clone!(
        #[weak]
        terminal,
        #[strong]
        find,
        move |entry| {
            let text = entry.text();
            let regex = (!text.is_empty())
                .then(|| vte4::Regex::for_search(&literal(&text), PCRE2_CASELESS | PCRE2_MULTILINE))
                .and_then(Result::ok);
            terminal.search_set_regex(regex.as_ref(), 0);
            terminal.unselect_all();
            find(entry, true);
        }
    ));
    entry.connect_activate({
        let find = find.clone();
        move |entry| find(entry, true)
    });
    let shift_enter = gtk::ShortcutController::new();
    shift_enter.add_shortcut(gtk::Shortcut::new(
        gtk::ShortcutTrigger::parse_string("<Shift>Return"),
        Some(gtk::CallbackAction::new({
            let find = find.clone();
            move |widget, _| {
                if let Some(entry) = widget.downcast_ref::<gtk::SearchEntry>() {
                    find(entry, false);
                }
                glib::Propagation::Stop
            }
        })),
    ));
    entry.add_controller(shift_enter);
    older.connect_clicked(glib::clone!(
        #[weak]
        entry,
        #[strong]
        find,
        move |_| find(&entry, true)
    ));
    newer.connect_clicked(glib::clone!(
        #[weak]
        entry,
        move |_| find(&entry, false)
    ));
    entry.connect_stop_search(glib::clone!(
        #[weak]
        bar,
        move |_| bar.set_search_mode(false)
    ));
    // Closing the bar clears the highlight and gives the keys back.
    bar.connect_search_mode_enabled_notify(glib::clone!(
        #[weak]
        terminal,
        move |bar| {
            if !bar.is_search_mode() {
                terminal.search_set_regex(None, 0);
                terminal.unselect_all();
                terminal.grab_focus();
            }
        }
    ));

    let bar_weak = bar.downgrade();
    let open = gtk::CallbackAction::new(move |_, _| {
        if let Some(bar) = bar_weak.upgrade() {
            bar.set_search_mode(true);
        }
        glib::Propagation::Stop
    });
    let controller = gtk::ShortcutController::new();
    controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    controller.add_shortcut(gtk::Shortcut::new(
        gtk::ShortcutTrigger::parse_string("<Control><Shift>f"),
        Some(open),
    ));
    terminal.add_controller(controller);
    bar
}

/// `text` as a PCRE2 pattern that matches it literally: a backslash before
/// anything but a letter or digit makes it literal.
fn literal(text: &str) -> String {
    let mut pattern = String::with_capacity(text.len() * 2);
    for c in text.chars() {
        if !c.is_alphanumeric() && !c.is_whitespace() {
            pattern.push('\\');
        }
        pattern.push(c);
    }
    pattern
}

/// The terminal inside a tab page: a terminal page, or a box, such as an
/// SSH shell's with its toolbar, that ends with one.
pub fn in_page(page: &gtk::Widget) -> Option<vte4::Terminal> {
    if let Some(page) = page.downcast_ref::<gtk::Box>() {
        return in_page(&page.last_child()?);
    }
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

    #[test]
    fn found_text_is_matched_literally() {
        assert_eq!(literal("a.b*"), r"a\.b\*");
        assert_eq!(literal(r"C:\x (1)"), r"C\:\\x \(1\)");
        assert_eq!(literal("café 2"), "café 2");
    }

    #[test]
    fn only_web_and_mail_links_open() {
        assert!(openable("https://example.com/a?b=c"));
        assert!(openable("HTTP://example.com"));
        assert!(openable("mailto:someone@example.com"));
        assert!(!openable("file:///etc/passwd"));
        assert!(!openable("javascript:alert(1)"));
        assert!(!openable("ssh://host"));
        assert!(!openable("no scheme"));
    }
}
