//! The settings dialog.

use relm4::gtk::prelude::*;
use relm4::{ComponentSender, gtk};

use crate::dialogs;
use crate::settings::{Theme, UiSettings};

use super::App;
use super::message::AppMsg;

pub(super) fn show_settings(
    window: &gtk::ApplicationWindow,
    current: &UiSettings,
    sender: &ComponentSender<App>,
) {
    let dialog = gtk::Window::builder()
        .title("Settings")
        .modal(true)
        .transient_for(window)
        .resizable(false)
        .default_width(360)
        .build();
    let grid = gtk::Grid::builder()
        .row_spacing(12)
        .column_spacing(16)
        .margin_top(18)
        .margin_bottom(18)
        .margin_start(18)
        .margin_end(18)
        .build();
    let row_label = |text: &str| {
        let label = gtk::Label::new(Some(text));
        label.set_xalign(0.0);
        label.set_hexpand(true);
        label
    };

    let names: Vec<&str> = Theme::ALL.iter().map(|t| t.label()).collect();
    let theme = gtk::DropDown::from_strings(&names);
    let selected = Theme::ALL.iter().position(|&t| t == current.theme);
    theme.set_selected(selected.unwrap_or(0) as u32);
    theme.connect_selected_notify({
        let sender = sender.clone();
        move |dropdown| {
            if let Some(&theme) = Theme::ALL.get(dropdown.selected() as usize) {
                sender.input(AppMsg::SetTheme(theme));
            }
        }
    });
    grid.attach(&row_label("Theme"), 0, 0, 1, 1);
    grid.attach(&theme, 1, 0, 1, 1);

    // Only fixed-width fonts, since the terminal grid needs one. The list
    // holds faces when a face can be chosen, and families otherwise.
    let fonts = gtk::FontDialog::new();
    fonts.set_title("Terminal font");
    fonts.set_filter(Some(&gtk::CustomFilter::new(|item| {
        let family = match item.downcast_ref::<gtk::pango::FontFace>() {
            Some(face) => face.family(),
            None => match item.downcast_ref::<gtk::pango::FontFamily>() {
                Some(family) => family.clone(),
                None => return false,
            },
        };
        family.is_monospace()
    })));
    let font = gtk::FontDialogButton::new(Some(fonts));
    font.set_level(gtk::FontLevel::Font);
    // What the button shows for the system's font.
    const SYSTEM_FONT: &str = "Monospace 11";
    let initial = current.terminal_font.as_deref().unwrap_or(SYSTEM_FONT);
    font.set_font_desc(&gtk::pango::FontDescription::from_string(initial));
    let reset = gtk::Button::from_icon_name("edit-undo-symbolic");
    reset.set_tooltip_text(Some("Use the system's monospace font"));
    reset.set_sensitive(current.terminal_font.is_some());
    let chosen = font.connect_font_desc_notify({
        let (sender, reset) = (sender.clone(), reset.clone());
        move |button| {
            if let Some(desc) = button.font_desc() {
                reset.set_sensitive(true);
                sender.input(AppMsg::SetTerminalFont(Some(desc.to_string())));
            }
        }
    });
    reset.connect_clicked({
        let (sender, font) = (sender.clone(), font.clone());
        move |reset| {
            reset.set_sensitive(false);
            // Showing the system's font is not choosing it.
            font.block_signal(&chosen);
            font.set_font_desc(&gtk::pango::FontDescription::from_string(SYSTEM_FONT));
            font.unblock_signal(&chosen);
            sender.input(AppMsg::SetTerminalFont(None));
        }
    });
    let font_row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    font_row.append(&font);
    font_row.append(&reset);
    grid.attach(&row_label("Terminal font"), 0, 1, 1, 1);
    grid.attach(&font_row, 1, 1, 1, 1);

    let hint = gtk::Label::new(Some(
        "Ctrl+= and Ctrl+- change the size in one terminal, and Ctrl+0 resets it.",
    ));
    hint.set_wrap(true);
    hint.set_xalign(0.0);
    hint.set_max_width_chars(40);
    hint.add_css_class("dim-label");
    grid.attach(&hint, 0, 2, 2, 1);

    dialog.set_child(Some(&grid));
    dialogs::close_on_escape(&dialog);
    dialog.present();
}
