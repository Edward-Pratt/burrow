//! Small modal dialogs: a one-line text prompt, a yes/no confirmation, and
//! the questions SSH sessions ask (host keys, passwords).

use std::cell::RefCell;
use std::rc::Rc;

use burrow_core::{Prompt, PromptAnswer, PromptKind, SecretValue};
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

/// Asks a session's question ([`Prompt`]) and calls `done` exactly once:
/// with the answer, or with [`PromptAnswer::Cancel`] if the dialog is
/// dismissed.
pub fn answer(
    parent: &impl IsA<gtk::Window>,
    prompt: &Prompt,
    done: impl FnOnce(PromptAnswer) + 'static,
) {
    if prompt.kind == PromptKind::Confirm {
        let dialog = gtk::AlertDialog::builder()
            .modal(true)
            .message(&prompt.title)
            .detail(&prompt.message)
            .buttons(["Cancel", "Trust and connect"])
            .cancel_button(0)
            .default_button(0)
            .build();
        dialog.choose(Some(parent), None::<&gio::Cancellable>, move |choice| {
            done(if choice == Ok(1) {
                PromptAnswer::Confirm
            } else {
                PromptAnswer::Cancel
            });
        });
        return;
    }

    let window = gtk::Window::builder()
        .title(&prompt.title)
        .modal(true)
        .transient_for(parent)
        .resizable(false)
        .default_width(380)
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 10);
    content.set_margin_top(18);
    content.set_margin_bottom(18);
    content.set_margin_start(18);
    content.set_margin_end(18);

    let message = gtk::Label::new(Some(&prompt.message));
    message.set_xalign(0.0);
    message.set_wrap(true);
    message.set_max_width_chars(48);
    content.append(&message);

    let (entry, save): (gtk::Widget, Option<gtk::CheckButton>) = match prompt.kind {
        PromptKind::Secret { can_save } => {
            let entry = gtk::PasswordEntry::new();
            entry.set_show_peek_icon(true);
            let save = can_save.then(|| gtk::CheckButton::with_label("Save in the keyring"));
            (entry.upcast(), save)
        }
        _ => (gtk::Entry::new().upcast(), None),
    };
    content.append(&entry);
    if let Some(save) = &save {
        content.append(save);
    }

    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::End);
    buttons.set_margin_top(6);
    let cancel = gtk::Button::with_label("Cancel");
    let ok = gtk::Button::with_label("Continue");
    ok.add_css_class("suggested-action");
    buttons.append(&cancel);
    buttons.append(&ok);
    content.append(&buttons);
    window.set_child(Some(&content));

    // Every way out of the dialog goes through `finish`, which answers
    // once.
    let done = Rc::new(RefCell::new(Some(done)));
    let finish = {
        let done = done.clone();
        move |answer: PromptAnswer| {
            if let Some(done) = done.borrow_mut().take() {
                done(answer);
            }
        }
    };
    let submit = Rc::new({
        let (window, entry, finish) = (window.clone(), entry.clone(), finish.clone());
        move || {
            let text = entry
                .downcast_ref::<gtk::Editable>()
                .map(|e| e.text().to_string())
                .unwrap_or_default();
            finish(PromptAnswer::Text {
                value: SecretValue::new(text),
                save: save.as_ref().is_some_and(|s| s.is_active()),
            });
            window.close();
        }
    });
    ok.connect_clicked({
        let submit = submit.clone();
        move |_| submit()
    });
    // Enter submits. Both entry types have their own `activate` signal.
    if let Some(entry) = entry.downcast_ref::<gtk::PasswordEntry>() {
        let submit = submit.clone();
        entry.connect_activate(move |_| submit());
    } else if let Some(entry) = entry.downcast_ref::<gtk::Entry>() {
        let submit = submit.clone();
        entry.connect_activate(move |_| submit());
    }
    cancel.connect_clicked({
        let window = window.clone();
        move |_| window.close()
    });
    window.connect_close_request(move |_| {
        finish(PromptAnswer::Cancel);
        glib::Propagation::Proceed
    });
    close_on_escape(&window);

    window.present();
    entry.grab_focus();
}
