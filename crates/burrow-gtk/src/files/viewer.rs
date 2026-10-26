//! A read-only window showing the start of a remote file.
//!
//! The contents come from the server, so they are cleaned up the way names
//! are: control characters never reach the text view, and a file that
//! looks binary isn't drawn at all.

use relm4::gtk;
use relm4::gtk::prelude::*;

use super::{printable, size};
use crate::dialogs;

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
}

impl Contents {
    pub fn new(data: &[u8], size: u64) -> Contents {
        let read = data.len() as u64;
        let data = if size > read { complete(data) } else { data };
        let (text, split) = match decode(data) {
            Some((text, split)) => (Some(text), split),
            None => (None, false),
        };
        Contents {
            text,
            read,
            size,
            split,
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
}

/// Opens a viewer for `contents`, the start of the file called `name`.
pub fn show(parent: &gtk::ApplicationWindow, name: &str, contents: &Contents) {
    let window = gtk::Window::builder()
        .title(printable(name))
        .transient_for(parent)
        .destroy_with_parent(true)
        .build();
    window.add_css_class("file-viewer");

    let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
    content.set_margin_top(12);
    content.set_margin_bottom(12);
    content.set_margin_start(12);
    content.set_margin_end(12);
    let summary = gtk::Label::new(Some(&contents.summary()));
    summary.set_xalign(0.0);
    summary.set_wrap(true);
    content.append(&summary);

    match &contents.text {
        Some(text) => {
            window.set_default_size(800, 600);
            summary.add_css_class("dim");
            let view = gtk::TextView::new();
            view.set_editable(false);
            view.set_cursor_visible(false);
            view.set_monospace(true);
            view.buffer().set_text(text);
            let scroller = gtk::ScrolledWindow::builder()
                .child(&view)
                .vexpand(true)
                .hexpand(true)
                .build();
            content.append(&scroller);
        }
        None => window.set_default_size(420, -1),
    }
    window.set_child(Some(&content));
    dialogs::close_on_escape(&window);
    window.present();
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

    fn text(data: &[u8]) -> String {
        Contents::new(data, data.len() as u64).text.unwrap()
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
        assert_eq!(Contents::new(b"ELF\0\x01\x02", 6).text, None);
        assert_eq!(Contents::new(&[0xff, 0xfe, 0x81, 0x90, b'a'], 5).text, None);
        assert_eq!(Contents::new(b"", 0).text, Some(String::new()));
    }

    #[test]
    fn a_cut_read_drops_a_split_character() {
        let data = "aé€😀".as_bytes();
        for cut in 1..data.len() {
            let contents = Contents::new(&data[..cut], data.len() as u64);
            let shown = contents.text.as_deref().unwrap();
            assert!(data.starts_with(shown.as_bytes()), "{cut}: {shown:?}");
            assert!(contents.summary().starts_with("The first"));
        }
        // A whole file that ends badly still shows that it does.
        assert_eq!(text(b"ends badly \xe2\x82"), "ends badly \u{fffd}");
    }

    #[test]
    fn long_lines_are_broken_up() {
        let line = "x".repeat(MAX_LINE * 2 + 5);
        let contents = Contents::new(format!("{line}\nshort").as_bytes(), 0);
        let text = contents.text.as_deref().unwrap();
        let lengths: Vec<usize> = text.lines().map(str::len).collect();
        assert_eq!(lengths, [MAX_LINE, MAX_LINE, 5, 5]);
        assert!(contents.split);
        assert!(contents.summary().contains("broken up"));
        assert!(!Contents::new(b"short\n", 6).split);
    }

    #[test]
    fn the_summary_says_what_is_missing() {
        assert_eq!(Contents::new(b"hi", 2).summary(), "2 B.");
        assert_eq!(
            Contents::new(b"hi", 3 << 30).summary(),
            "The first 2 B of 3.0 GiB."
        );
        // Files in /proc say they are empty.
        assert_eq!(Contents::new(b"hi", 0).summary(), "2 B.");
        assert!(Contents::new(b"\0", 1).summary().contains("binary"));
    }
}
