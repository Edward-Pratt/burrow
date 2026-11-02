//! The dialogs that pick a folder to move to, or a colour.

use std::rc::Rc;

use burrow_core::{Node, NodeId};
use relm4::gtk;
use relm4::gtk::glib;
use relm4::gtk::prelude::*;

use crate::dialogs;

/// Asks which folder to move `node` to, from `choices` as
/// [`move_choices`] makes them, and calls `done` with a new one.
pub fn move_dialog(
    parent: &impl IsA<gtk::Window>,
    node: &Node,
    choices: Vec<(Option<NodeId>, String)>,
    done: impl Fn(Option<NodeId>) + 'static,
) {
    let window = gtk::Window::builder()
        .title("Move")
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

    let label = gtk::Label::new(Some(&format!("Move “{}” to", node.name)));
    label.set_xalign(0.0);
    label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    let labels: Vec<&str> = choices.iter().map(|(_, path)| path.as_str()).collect();
    let folders = gtk::DropDown::from_strings(&labels);
    let current = choices.iter().position(|(id, _)| *id == node.parent);
    folders.set_selected(current.unwrap_or(0) as u32);
    content.append(&label);
    content.append(&folders);

    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::End);
    buttons.set_margin_top(6);
    let cancel = gtk::Button::with_label("Cancel");
    let ok = gtk::Button::with_label("Move");
    ok.add_css_class("suggested-action");
    buttons.append(&cancel);
    buttons.append(&ok);
    content.append(&buttons);
    window.set_child(Some(&content));

    // Moving to where it is already would do nothing, so the button says so.
    let from = node.parent;
    let chosen = move |folders: &gtk::DropDown| {
        let index = folders.selected() as usize;
        choices.get(index).map(|(id, _)| *id)
    };
    let chosen = Rc::new(chosen);
    ok.set_sensitive(false);
    folders.connect_selected_notify(glib::clone!(
        #[weak]
        ok,
        #[strong]
        chosen,
        move |folders| ok.set_sensitive(chosen(folders).is_some_and(|to| to != from))
    ));
    // The window is held weakly by everything inside it, or it would keep
    // itself alive after closing.
    ok.connect_clicked(glib::clone!(
        #[weak]
        window,
        #[weak]
        folders,
        move |_| {
            if let Some(to) = chosen(&folders).filter(|to| *to != from) {
                window.close();
                done(to);
            }
        }
    ));
    dialogs::close_on_cancel(&window, &cancel);
    dialogs::close_on_escape(&window);
    window.present();
    folders.grab_focus();
}

/// The colours "Colour…" offers, in the `#rrggbb` form the core takes.
pub const COLOURS: [(&str, &str); 8] = [
    ("Red", "#e01b24"),
    ("Orange", "#ff7800"),
    ("Yellow", "#f6d32d"),
    ("Green", "#33d17a"),
    ("Blue", "#3584e4"),
    ("Purple", "#9141ac"),
    ("Brown", "#986a44"),
    ("Grey", "#77767b"),
];

/// Asks which colour to mark `node` with, and calls `done` with it, or
/// with `None` to clear it. The current one is the swatch with a frame.
pub fn colour_dialog(
    parent: &impl IsA<gtk::Window>,
    node: &Node,
    done: impl Fn(Option<String>) + 'static,
) {
    let window = gtk::Window::builder()
        .title("Colour")
        .modal(true)
        .transient_for(parent)
        .resizable(false)
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 10);
    content.set_margin_top(18);
    content.set_margin_bottom(18);
    content.set_margin_start(18);
    content.set_margin_end(18);
    let label = gtk::Label::new(Some(&format!("Colour for “{}”", node.name)));
    label.set_xalign(0.0);
    label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    content.append(&label);

    let current = node.color.as_deref().map(str::to_lowercase);
    let done = Rc::new(done);
    let swatches = gtk::Grid::new();
    swatches.set_column_spacing(6);
    swatches.set_row_spacing(6);
    for (index, (name, hex)) in (0..).zip(COLOURS) {
        let dot = gtk::Label::new(None);
        dot.set_markup(&format!(
            "<span foreground=\"{hex}\" size=\"xx-large\">●</span>"
        ));
        let button = gtk::Button::new();
        button.set_child(Some(&dot));
        button.set_tooltip_text(Some(name));
        // Read out, and found by tests, by its name rather than "●".
        button.update_property(&[gtk::accessible::Property::Label(name)]);
        button.set_has_frame(current.as_deref() == Some(hex));
        // The window is held weakly by everything inside it, or it would
        // keep itself alive after closing.
        button.connect_clicked(glib::clone!(
            #[weak]
            window,
            #[strong]
            done,
            move |_| {
                window.close();
                done(Some(hex.to_string()));
            }
        ));
        swatches.attach(&button, index % 4, index / 4, 1, 1);
    }
    content.append(&swatches);

    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_margin_top(6);
    let none = gtk::Button::with_label("None");
    none.set_has_frame(current.is_none());
    none.set_hexpand(true);
    none.set_halign(gtk::Align::Start);
    none.connect_clicked(glib::clone!(
        #[weak]
        window,
        move |_| {
            window.close();
            done(None);
        }
    ));
    let cancel = gtk::Button::with_label("Cancel");
    buttons.append(&none);
    buttons.append(&cancel);
    content.append(&buttons);
    window.set_child(Some(&content));
    dialogs::close_on_cancel(&window, &cancel);
    dialogs::close_on_escape(&window);
    window.present();
}

/// Whether `color` is `#rgb` or `#rrggbb`, as the core requires. Node files
/// can be written by hand and the core only checks colors set through it,
/// so this is checked again before a color goes into markup.
pub(super) fn is_color(color: &str) -> bool {
    color
        .strip_prefix('#')
        .is_some_and(|hex| matches!(hex.len(), 3 | 6) && hex.chars().all(|c| c.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_colours_are_what_the_core_takes() {
        for (name, hex) in COLOURS {
            assert!(is_color(hex) && hex.len() == 7, "{name}: {hex}");
            assert_eq!(hex, hex.to_lowercase(), "{name}");
        }
    }

    #[test]
    fn colors_are_checked_before_markup() {
        assert!(is_color("#e06c75"));
        assert!(is_color("#FFF"));
        assert!(!is_color("red"));
        assert!(!is_color("#12345"));
        assert!(!is_color("#fff\" size=\"100000"));
    }
}
