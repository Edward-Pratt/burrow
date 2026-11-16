//! The clipboard, shared with a desktop only while it has the focus.
//!
//! The server is not trusted. It may set the clipboard at any time, which
//! could plant a command for the next paste into a terminal, so its text
//! is taken only while its desktop has the focus, and without control
//! characters other than newlines and tabs. Going the other way, the
//! local clipboard is sent when the desktop gets the focus and when it
//! changes while the desktop has it, so a password copied in another app
//! reaches a desktop only once the user moves to it.
//!
//! vnc-rs sends and reads the text as UTF-8, where RFB says Latin-1, so
//! text beyond ASCII may arrive garbled on some servers.

use std::cell::RefCell;
use std::rc::Rc;

use relm4::gtk;
use relm4::gtk::glib;
use relm4::gtk::prelude::*;
use vnc::X11Event;

use super::input::Shared;

/// The most text sent at once. vnc-rs drops the connection over 1 MiB.
const MAX: usize = 1 << 20;

/// Watches the focus and the local clipboard for `picture`.
pub(super) fn add(picture: &gtk::Picture, shared: &Rc<Shared>) {
    let focus = gtk::EventControllerFocus::new();
    focus.connect_enter({
        let (picture, shared) = (picture.downgrade(), shared.clone());
        move |_| {
            if let Some(picture) = picture.upgrade() {
                send_local(&picture, &shared);
            }
        }
    });
    picture.add_controller(focus);
    let clipboard = picture.clipboard();
    let changed = clipboard.connect_changed({
        let (picture, shared) = (picture.downgrade(), shared.clone());
        move |_| {
            if let Some(picture) = picture.upgrade().filter(focused) {
                send_local(&picture, &shared);
            }
        }
    });
    // The clipboard lives as long as the display, so the handler would
    // outlive the tab.
    let changed = RefCell::new(Some(changed));
    picture.connect_destroy(move |_| {
        if let Some(changed) = changed.take() {
            clipboard.disconnect(changed);
        }
    });
}

/// Sends the local clipboard's text to the desktop, unless it is what the
/// two sides already share.
pub(super) fn send_local(picture: &gtk::Picture, shared: &Rc<Shared>) {
    let (clipboard, shared) = (picture.clipboard(), shared.clone());
    glib::spawn_future_local(async move {
        let Ok(Some(text)) = clipboard.read_text_future().await else {
            return;
        };
        let text = text.to_string();
        if text.len() > MAX || shared.copied.borrow().as_ref() == Some(&text) {
            return;
        }
        shared.copied.replace(Some(text.clone()));
        shared.send(X11Event::CopyText(text));
    });
}

/// Takes `text` from the server onto the local clipboard, if its desktop
/// has the focus.
pub(super) fn from_server(picture: &gtk::Picture, shared: &Shared, text: &str) {
    let text = clean(text);
    if shared.copied.borrow().as_ref() == Some(&text) {
        return;
    }
    if !focused(picture) || text.is_empty() {
        // The server's clipboard no longer holds what was shared, so the
        // local one is sent again when the desktop next has the focus.
        shared.copied.replace(None);
        return;
    }
    // Remembered first, so the change this makes isn't sent back.
    shared.copied.replace(Some(text.clone()));
    picture.clipboard().set_text(&text);
}

/// Whether `picture` has the keyboard in an active window. A tab behind
/// another is unmapped, which GTK should already take the focus from.
fn focused(picture: &gtk::Picture) -> bool {
    picture.has_focus() && picture.is_mapped()
}

/// `text` without control characters, but for newlines and tabs. A
/// carriage return goes too, so Windows line ends paste as plain ones.
fn clean(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_text_loses_control_characters() {
        assert_eq!(clean("ls\r\n-la\tx"), "ls\n-la\tx");
        assert_eq!(clean("a\u{1b}[31mred\u{7}\u{0}"), "a[31mred");
        assert_eq!(clean("caf\u{e9} \u{9b}2J"), "caf\u{e9} 2J");
        assert_eq!(clean("\r\u{1b}"), "");
    }
}
