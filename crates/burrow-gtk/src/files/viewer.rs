//! A window showing the start of a remote file, which can edit and save it
//! when it is plain text read whole.
//!
//! The contents come from the server, so they are cleaned up the way names
//! are: control characters never reach the text view, and a file that
//! looks binary isn't drawn at all. A file the cleaning changed is shown
//! read-only, since saving what is shown would change it.

use std::cell::RefCell;
use std::rc::Rc;

use burrow_core::ipc::Client;
use burrow_core::{Command, SessionId, sha256_hex};
use relm4::gtk;
use relm4::gtk::glib;
use relm4::gtk::prelude::*;
use sourceview5::prelude::*;

use super::{printable, size};
use crate::database::follow_theme;
use crate::dialogs;
use crate::runtime::runtime;

/// How much of a file is read. A text view gets slow well before the
/// core's own limit.
pub const LIMIT: u64 = 512 << 10;

/// Lines longer than this are broken up. GTK lays out a whole line at once,
/// so one huge line (minified code, a log with no newlines) would stall it.
const MAX_LINE: usize = 1000;

/// The start of a file, ready to show. Made off the GTK thread.
#[derive(Clone, Debug, PartialEq)]
pub struct Contents {
    /// `None` when the file looks binary.
    text: Option<String>,
    /// How many bytes were read.
    read: u64,
    /// The file's size, as the server reports it. Files such as those in
    /// `/proc` report 0 whatever they hold.
    size: u64,
    /// Whether long lines were broken up.
    split: bool,
    /// Whether `text` is the whole file, unchanged, so saving it back
    /// changes only what the user edits.
    editable: bool,
    /// The SHA-256 of what was read, which a save expects the file to
    /// still have.
    sha256: String,
}

impl Contents {
    pub fn new(data: &[u8], size: u64, sha256: String) -> Contents {
        let read = data.len() as u64;
        let whole = read == size;
        let data = if size > read { complete(data) } else { data };
        let (text, split) = match decode(data) {
            Some((text, split)) => (Some(text), split),
            None => (None, false),
        };
        let editable = whole && text.as_deref().is_some_and(|text| text.as_bytes() == data);
        Contents {
            text,
            read,
            size,
            split,
            editable,
            sha256,
        }
    }

    /// One line about what is shown, and what isn't.
    fn summary(&self) -> String {
        let total = size(self.size.max(self.read));
        if self.text.is_none() {
            return format!("{total}. This looks like a binary file, so it isn't shown.");
        }
        let mut summary = if self.size > self.read {
            format!("The first {} of {total}.", size(self.read))
        } else {
            format!("{total}.")
        };
        if self.split {
            summary.push_str(" Long lines are broken up.");
        }
        summary
    }

    /// Why the text can't be edited, when it can't.
    fn read_only(&self) -> Option<&'static str> {
        match &self.text {
            None => None,
            Some(_) if self.editable => None,
            Some(_) if self.read != self.size => Some("Read-only, since not all of it is shown."),
            Some(_) => Some(
                "Read-only, since some characters are shown differently from the file, \
                 such as control characters or Windows line ends.",
            ),
        }
    }
}

/// Where an edited file is saved.
pub struct Save {
    pub core: Client,
    pub session: SessionId,
    pub path: String,
    /// Runs after each save, to list the folder again.
    pub saved: Box<dyn Fn()>,
}

/// Opens a viewer for `contents`, the start of the file called `name`. With
/// `save`, a file that can be edited opens in an editor that saves there.
pub fn show(parent: &gtk::ApplicationWindow, name: &str, contents: &Contents, save: Option<Save>) {
    let name = printable(name);
    let window = gtk::Window::builder()
        .title(&name)
        .transient_for(parent)
        .destroy_with_parent(true)
        .build();
    window.add_css_class("file-viewer");

    let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
    content.set_margin_top(12);
    content.set_margin_bottom(12);
    content.set_margin_start(12);
    content.set_margin_end(12);
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let summary = gtk::Label::new(Some(&contents.summary()));
    summary.set_xalign(0.0);
    summary.set_wrap(true);
    summary.set_hexpand(true);
    header.append(&summary);
    content.append(&header);

    let Some(text) = &contents.text else {
        window.set_default_size(420, -1);
        window.set_child(Some(&content));
        dialogs::close_on_escape(&window);
        window.present();
        return;
    };
    window.set_default_size(800, 600);
    summary.add_css_class("dim");
    let buffer = sourceview5::Buffer::new(None::<&gtk::TextTagTable>);
    let language = sourceview5::LanguageManager::default().guess_language(Some(&name), None);
    buffer.set_language(language.as_ref());
    // Not undoable: loading isn't an edit.
    buffer.set_enable_undo(false);
    buffer.set_text(text);
    buffer.set_enable_undo(true);
    buffer.set_modified(false);
    buffer.place_cursor(&buffer.start_iter());
    let view = sourceview5::View::with_buffer(&buffer);
    view.set_monospace(true);
    view.set_show_line_numbers(true);
    follow_theme(&buffer, &view);
    let scroller = gtk::ScrolledWindow::builder()
        .child(&view)
        .vexpand(true)
        .hexpand(true)
        .build();
    content.append(&scroller);
    window.set_child(Some(&content));

    match save.filter(|_| contents.editable) {
        Some(save) => editor(&window, &header, &summary, &buffer, &name, contents, save),
        None => {
            if let Some(reason) = contents.read_only() {
                summary.set_label(&format!("{} {reason}", summary.label()));
            }
            view.set_editable(false);
            view.set_cursor_visible(false);
            dialogs::close_on_escape(&window);
        }
    }
    window.present();
    view.grab_focus();
}

/// Turns the viewer into an editor: a Save button and Ctrl+S, a title that
/// marks unsaved changes, and a question before closing with them.
fn editor(
    window: &gtk::Window,
    header: &gtk::Box,
    summary: &gtk::Label,
    buffer: &sourceview5::Buffer,
    name: &str,
    contents: &Contents,
    save: Save,
) {
    let status = gtk::Label::new(None);
    status.set_wrap(true);
    status.set_xalign(1.0);
    let button = gtk::Button::with_label("Save");
    button.set_tooltip_text(Some("Save (Ctrl+S)"));
    button.set_sensitive(false);
    header.append(&status);
    header.append(&button);

    buffer.connect_modified_changed(glib::clone!(
        #[weak]
        window,
        #[weak]
        button,
        #[weak]
        status,
        #[to_owned]
        name,
        move |buffer| {
            let modified = buffer.is_modified();
            button.set_sensitive(modified);
            if modified {
                status.set_label("");
            }
            window.set_title(Some(&if modified {
                format!("• {name}")
            } else {
                name.clone()
            }));
        }
    ));

    let editor = Rc::new(Editor {
        save,
        expect: RefCell::new(Some(contents.sha256.clone())),
        window: window.downgrade(),
        buffer: buffer.downgrade(),
        summary: summary.downgrade(),
        status: status.downgrade(),
        button: button.downgrade(),
    });
    button.connect_clicked({
        let editor = editor.clone();
        move |_| editor.save(false)
    });
    let shortcuts = gtk::ShortcutController::new();
    shortcuts.set_scope(gtk::ShortcutScope::Managed);
    shortcuts.add_shortcut(gtk::Shortcut::new(
        gtk::ShortcutTrigger::parse_string("<Control>s"),
        Some(gtk::CallbackAction::new({
            let editor = editor.clone();
            move |_, _| {
                editor.save(false);
                glib::Propagation::Stop
            }
        })),
    ));
    window.add_controller(shortcuts);

    // Escape and the close button ask first when there are changes.
    let close = gtk::ShortcutController::new();
    close.add_shortcut(gtk::Shortcut::new(
        gtk::ShortcutTrigger::parse_string("Escape"),
        Some(gtk::CallbackAction::new(|widget, _| {
            if let Some(window) = widget.downcast_ref::<gtk::Window>() {
                window.close();
            }
            glib::Propagation::Stop
        })),
    ));
    window.add_controller(close);
    window.connect_close_request(glib::clone!(
        #[weak]
        buffer,
        #[upgrade_or]
        glib::Propagation::Proceed,
        move |window| {
            if !buffer.is_modified() {
                return glib::Propagation::Proceed;
            }
            let target = window.clone();
            let discard = glib::clone!(
                #[weak]
                buffer,
                move || {
                    buffer.set_modified(false);
                    target.close();
                }
            );
            dialogs::confirm(
                window,
                "Close without saving?",
                "Your changes to this file will be lost.",
                "Close",
                discard,
            );
            glib::Propagation::Stop
        }
    ));
}

/// What a save needs. Widgets are held weakly: the window holds this
/// through its button and shortcut.
struct Editor {
    save: Save,
    /// The hash the file on the server should still have. `None` while a
    /// save runs.
    expect: RefCell<Option<String>>,
    window: glib::WeakRef<gtk::Window>,
    buffer: glib::WeakRef<sourceview5::Buffer>,
    summary: glib::WeakRef<gtk::Label>,
    status: glib::WeakRef<gtk::Label>,
    button: glib::WeakRef<gtk::Button>,
}

impl Editor {
    /// Saves the text over the file, if the file is as it was read. With
    /// `replace`, saves over whatever the file holds now.
    fn save(self: &Rc<Self>, replace: bool) {
        let (Some(buffer), Some(status), Some(button)) = (
            self.buffer.upgrade(),
            self.status.upgrade(),
            self.button.upgrade(),
        ) else {
            return;
        };
        let Some(expect) = self.expect.borrow_mut().take() else {
            return;
        };
        let text = buffer.text(&buffer.start_iter(), &buffer.end_iter(), true);
        let data = text.as_bytes().to_vec();
        let sha256 = sha256_hex(&data);
        let command = Command::WriteFile {
            session: self.save.session,
            path: self.save.path.clone(),
            data,
            overwrite: replace,
            expect: (!replace).then(|| expect.clone()),
        };
        status.remove_css_class("error-text");
        status.set_label("Saving…");
        button.set_sensitive(false);
        let core = self.save.core.clone();
        let editor = self.clone();
        glib::spawn_future_local(async move {
            let result = runtime()
                .spawn(async move { core.execute(command).await })
                .await
                .map_err(|e| e.to_string())
                .and_then(|r| r.map_err(|e| e.to_string()));
            editor.saved(result.map(|_| sha256), expect, &text);
        });
    }

    fn saved(self: &Rc<Self>, result: Result<String, String>, expect: String, saved: &str) {
        let (Some(window), Some(buffer), Some(status), Some(button)) = (
            self.window.upgrade(),
            self.buffer.upgrade(),
            self.status.upgrade(),
            self.button.upgrade(),
        ) else {
            return;
        };
        match result {
            Ok(sha256) => {
                *self.expect.borrow_mut() = Some(sha256);
                // Typing during the save left changes still to save.
                let now = buffer.text(&buffer.start_iter(), &buffer.end_iter(), true);
                buffer.set_modified(now != saved);
                button.set_sensitive(now != saved);
                status.set_label("Saved.");
                if let Some(summary) = self.summary.upgrade() {
                    summary.set_label(&format!("{}.", size(saved.len() as u64)));
                }
                (self.save.saved)();
            }
            Err(error) => {
                *self.expect.borrow_mut() = Some(expect);
                button.set_sensitive(true);
                status.add_css_class("error-text");
                let conflict = error.contains("changed on the server");
                status.set_label(&if conflict {
                    "Changed on the server.".to_string()
                } else {
                    printable(&error)
                });
                if conflict {
                    let editor = self.clone();
                    dialogs::confirm(
                        &window,
                        "The file changed on the server",
                        "Someone changed it after it was opened. Saving replaces their \
                         changes with yours.",
                        "Replace",
                        move || editor.save(true),
                    );
                }
            }
        }
    }
}

/// The text of `data`, and whether long lines were broken up, or `None`
/// when it looks binary.
fn decode(data: &[u8]) -> Option<(String, bool)> {
    if data.contains(&0) {
        return None;
    }
    let lossy = String::from_utf8_lossy(data);
    // Text in another encoding still reads, with the odd character
    // replaced. Much more than that and it isn't text.
    let odd = lossy
        .chars()
        .filter(|&c| c == '\u{fffd}' || c.is_control() && !"\t\n\r\x0c\x1b".contains(c))
        .count();
    if odd * 10 > lossy.chars().count() {
        return None;
    }
    let mut text = String::with_capacity(lossy.len());
    let (mut line, mut split) = (0, false);
    let mut chars = lossy.chars().peekable();
    while let Some(c) = chars.next() {
        let c = match c {
            '\r' if chars.peek() == Some(&'\n') => continue,
            '\r' | '\n' => '\n',
            '\t' => '\t',
            // Bidirectional overrides can make text read differently from
            // what it is.
            c if c.is_control()
                || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') =>
            {
                '\u{fffd}'
            }
            c => c,
        };
        if c == '\n' {
            line = 0;
        } else {
            if line == MAX_LINE {
                text.push('\n');
                line = 0;
                split = true;
            }
            line += 1;
        }
        text.push(c);
    }
    Some((text, split))
}

/// `data` without the character a read that stops partway through a file
/// can cut in two at its end.
fn complete(data: &[u8]) -> &[u8] {
    // A character is at most four bytes, so a cut one starts in the last
    // three.
    for back in 1..=data.len().min(3) {
        let byte = data[data.len() - back];
        // Continuation bytes are 10xxxxxx; anything else starts a character.
        if byte & 0xc0 != 0x80 {
            let len = match byte {
                0xc0..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf7 => 4,
                _ => 1,
            };
            return if len > back {
                &data[..data.len() - back]
            } else {
                data
            };
        }
    }
    data
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contents(data: &[u8], size: u64) -> Contents {
        Contents::new(data, size, String::new())
    }

    fn text(data: &[u8]) -> String {
        contents(data, data.len() as u64).text.unwrap()
    }

    #[test]
    fn control_characters_are_not_shown() {
        assert_eq!(
            text(b"a\x1b[2Jb\r\nc\rd\te\x07"),
            "a\u{fffd}[2Jb\nc\nd\te\u{fffd}"
        );
        assert_eq!(
            text("left \u{202e}right\u{85} and more".as_bytes()),
            "left \u{fffd}right\u{fffd} and more"
        );
    }

    #[test]
    fn other_encodings_read_but_binary_is_not_shown() {
        // Latin-1 "café".
        assert_eq!(text(b"caf\xe9 au lait"), "caf\u{fffd} au lait");
        assert_eq!(contents(b"ELF\0\x01\x02", 6).text, None);
        assert_eq!(contents(&[0xff, 0xfe, 0x81, 0x90, b'a'], 5).text, None);
        assert_eq!(contents(b"", 0).text, Some(String::new()));
    }

    #[test]
    fn a_cut_read_drops_a_split_character() {
        let data = "aé€😀".as_bytes();
        for cut in 1..data.len() {
            let start = contents(&data[..cut], data.len() as u64);
            let shown = start.text.as_deref().unwrap();
            assert!(data.starts_with(shown.as_bytes()), "{cut}: {shown:?}");
            assert!(start.summary().starts_with("The first"));
        }
        // A whole file that ends badly still shows that it does.
        assert_eq!(text(b"ends badly \xe2\x82"), "ends badly \u{fffd}");
    }

    #[test]
    fn long_lines_are_broken_up() {
        let line = "x".repeat(MAX_LINE * 2 + 5);
        let long = contents(format!("{line}\nshort").as_bytes(), 0);
        let text = long.text.as_deref().unwrap();
        let lengths: Vec<usize> = text.lines().map(str::len).collect();
        assert_eq!(lengths, [MAX_LINE, MAX_LINE, 5, 5]);
        assert!(long.split);
        assert!(long.summary().contains("broken up"));
        assert!(!contents(b"short\n", 6).split);
    }

    #[test]
    fn the_summary_says_what_is_missing() {
        assert_eq!(contents(b"hi", 2).summary(), "2 B.");
        assert_eq!(
            contents(b"hi", 3 << 30).summary(),
            "The first 2 B of 3.0 GiB."
        );
        // Files in /proc say they are empty.
        assert_eq!(contents(b"hi", 0).summary(), "2 B.");
        assert!(contents(b"\0", 1).summary().contains("binary"));
    }

    #[test]
    fn only_plain_text_read_whole_can_be_edited() {
        let editable = |data: &[u8], size| contents(data, size).editable;
        assert!(editable(b"key = value\n\tindented\n", 22));
        assert!(editable("café ✓\n".as_bytes(), 10));
        assert!(editable(b"", 0));
        // Saving what is shown would change these.
        assert!(!editable(b"windows\r\n", 9));
        assert!(!editable(b"colour \x1b[31m", 12));
        assert!(!editable(b"caf\xe9", 4));
        assert!(!editable("hidden \u{202e}".as_bytes(), 10));
        assert!(!editable(
            "x".repeat(MAX_LINE + 1).as_bytes(),
            MAX_LINE as u64 + 1
        ));
        // Not all of it was read, or the server doesn't know its size.
        assert!(!editable(b"start", 100));
        assert!(!editable(b"from proc", 0));
        assert_eq!(contents(b"\0", 1).read_only(), None);
        assert!(
            contents(b"start", 100)
                .read_only()
                .unwrap()
                .contains("not all")
        );
        assert!(
            contents(b"a\r\n", 3)
                .read_only()
                .unwrap()
                .contains("line ends")
        );
    }
}
