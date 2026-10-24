//! Small modal dialogs: a one-line text prompt and a yes/no confirmation.

use relm4::gtk;
use relm4::gtk::prelude::*;
use relm4::gtk::{gio, glib};

/// Asks for one line of text and calls `done` with it, trimmed, when the
/// user confirms. Nothing is called on cancel or for empty text.
pub fn prompt(
    parent: &impl IsA<gtk::Window>,
    title: &str,
    label: &str,
    initial: &str,
    confirm: &str,
    done: impl Fn(String) + 'static,
) {
    let window = gtk::Window::builder()
        .title(title)
        .modal(true)
        .transient_for(parent)
        .resizable(false)
        .default_width(380)
        .build();
    window.add_css_class("prompt");

    let content = gtk::Box::new(gtk::Orientation::Vertical, 10);
    content.set_margin_top(18);
    content.set_margin_bottom(18);
    content.set_margin_start(18);
    content.set_margin_end(18);

    let text_label = gtk::Label::new(Some(label));
    text_label.set_xalign(0.0);
    let entry = gtk::Entry::new();
    entry.set_text(initial);
    content.append(&text_label);
    content.append(&entry);

    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::End);
    buttons.set_margin_top(6);
    let cancel = gtk::Button::with_label("Cancel");
    let ok = gtk::Button::with_label(confirm);
    ok.add_css_class("suggested-action");
    buttons.append(&cancel);
    buttons.append(&ok);
    content.append(&buttons);
    window.set_child(Some(&content));

    let submit = std::rc::Rc::new({
        let window = window.clone();
        let entry = entry.clone();
        move || {
            let text = entry.text().trim().to_string();
            if !text.is_empty() {
                window.close();
                done(text);
            }
        }
    });
    ok.connect_clicked({
        let submit = submit.clone();
        move |_| submit()
    });
    entry.connect_activate(move |_| submit());
    cancel.connect_clicked({
        let window = window.clone();
        move |_| window.close()
    });
    close_on_escape(&window);

    window.present();
    entry.grab_focus();
    entry.select_region(0, -1);
}

/// Shows a confirmation and calls `done` only if the user picks `confirm`.
pub fn confirm(
    parent: &impl IsA<gtk::Window>,
    heading: &str,
    detail: &str,
    confirm: &str,
    done: impl FnOnce() + 'static,
) {
    let dialog = gtk::AlertDialog::builder()
        .modal(true)
        .message(heading)
        .detail(detail)
        .buttons(["Cancel", confirm])
        .cancel_button(0)
        .default_button(0)
        .build();
    dialog.choose(Some(parent), None::<&gio::Cancellable>, move |choice| {
        if choice == Ok(1) {
            done();
        }
    });
}

pub fn close_on_escape(window: &gtk::Window) {
    let keys = gtk::EventControllerKey::new();
    keys.connect_key_pressed({
        let window = window.clone();
        move |_, key, _, _| {
            if key == gtk::gdk::Key::Escape {
                window.close();
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        }
    });
    window.add_controller(keys);
}
