//! Addon UI: drawing an addon's [`View`] with native widgets, the approval
//! dialog, and the list of installed addons.

use std::collections::BTreeMap;
use std::rc::Rc;

use burrow_core::{AddonInfo, AddonState, Block, FormInput, FormValue, View};
use relm4::gtk;
use relm4::gtk::prelude::*;

use crate::dialogs;

/// Rows of a table drawn before the rest are summed up. Views come from
/// another process, and a grid of widgets gets slow long before this.
const MAX_TABLE_ROWS: usize = 500;

/// What a view's buttons and forms call: the action and the form's values.
pub type OnAction = Rc<dyn Fn(String, BTreeMap<String, FormValue>)>;

/// The addon that owns `kind`, if any. The core leaves out addons whose
/// kinds clash, so there is at most one.
pub fn owner<'a>(addons: &'a [AddonInfo], kind: &str) -> Option<&'a AddonInfo> {
    addons
        .iter()
        .find(|a| a.kinds.iter().any(|k| k.kind == kind))
}

/// Whether the addon may be used: approved, whether or not it is running.
pub fn usable(addon: &AddonInfo) -> bool {
    !matches!(addon.state, AddonState::NeedsApproval { .. })
}

/// Replaces `page`'s children with the view's blocks.
pub fn render(page: &gtk::Box, view: &View, on_action: OnAction) {
    clear(page);
    for block in &view.blocks {
        page.append(&block_widget(block, &on_action));
    }
}

/// Replaces `page`'s children with a title, a message and optional buttons.
pub fn render_message(page: &gtk::Box, title: &str, message: &str, buttons: Vec<gtk::Button>) {
    clear(page);
    page.append(&heading(title));
    page.append(&text(message));
    if !buttons.is_empty() {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        for button in buttons {
            row.append(&button);
        }
        page.append(&row);
    }
}

fn clear(page: &gtk::Box) {
    while let Some(child) = page.first_child() {
        page.remove(&child);
    }
}

fn block_widget(block: &Block, on_action: &OnAction) -> gtk::Widget {
    match block {
        Block::Heading { text } => heading(text).upcast(),
        Block::Text { text: t } => text(t).upcast(),
        Block::Fields { fields } => {
            let grid = gtk::Grid::builder()
                .row_spacing(4)
                .column_spacing(16)
                .build();
            for (row, field) in fields.iter().enumerate() {
                let label = gtk::Label::new(Some(&field.label));
                label.set_xalign(0.0);
                label.set_yalign(0.0);
                label.add_css_class("dim");
                let value = gtk::Label::new(Some(&field.value));
                value.set_xalign(0.0);
                value.set_wrap(true);
                value.set_selectable(true);
                grid.attach(&label, 0, row as i32, 1, 1);
                grid.attach(&value, 1, row as i32, 1, 1);
            }
            grid.upcast()
        }
        Block::Table { columns, rows } => table(columns, rows),
        Block::Form {
            action,
            fields,
            submit,
        } => form(action, fields, submit, on_action),
        Block::Buttons { buttons } => {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            for button in buttons {
                let widget = gtk::Button::with_label(&button.label);
                let (action, on_action) = (button.action.clone(), on_action.clone());
                widget.connect_clicked(move |_| on_action(action.clone(), BTreeMap::new()));
                row.append(&widget);
            }
            row.upcast()
        }
    }
}

fn heading(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.set_wrap(true);
    label.add_css_class("page-title");
    label
}

fn text(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.set_wrap(true);
    label.set_selectable(true);
    label
}

fn table(columns: &[String], rows: &[Vec<String>]) -> gtk::Widget {
    let grid = gtk::Grid::builder()
        .row_spacing(2)
        .column_spacing(18)
        .build();
    grid.add_css_class("view-table");
    for (col, name) in columns.iter().enumerate() {
        let label = gtk::Label::new(Some(name));
        label.set_xalign(0.0);
        label.add_css_class("heading");
        grid.attach(&label, col as i32, 0, 1, 1);
    }
    for (row, cells) in rows.iter().take(MAX_TABLE_ROWS).enumerate() {
        for (col, cell) in cells.iter().take(columns.len().max(1)).enumerate() {
            let label = gtk::Label::new(Some(cell));
            label.set_xalign(0.0);
            label.set_selectable(true);
            grid.attach(&label, col as i32, row as i32 + 1, 1, 1);
        }
    }
    let column = gtk::Box::new(gtk::Orientation::Vertical, 4);
    let scroller = gtk::ScrolledWindow::builder()
        .child(&grid)
        .vscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .build();
    column.append(&scroller);
    if rows.len() > MAX_TABLE_ROWS {
        let more = gtk::Label::new(Some(&format!(
            "{} more rows not shown",
            rows.len() - MAX_TABLE_ROWS
        )));
        more.set_xalign(0.0);
        more.add_css_class("dim");
        column.append(&more);
    }
    column.upcast()
}

/// How to read one form field's value back.
enum Input {
    Entry(gtk::Entry),
    Multiline(gtk::TextView),
    Check(gtk::CheckButton),
    Choice(gtk::DropDown, Vec<String>),
}

impl Input {
    fn value(&self) -> FormValue {
        match self {
            Input::Entry(entry) => FormValue::Text(entry.text().to_string()),
            Input::Multiline(view) => {
                let buffer = view.buffer();
                let (start, end) = buffer.bounds();
                FormValue::Text(buffer.text(&start, &end, false).to_string())
            }
            Input::Check(check) => FormValue::Bool(check.is_active()),
            Input::Choice(dropdown, options) => FormValue::Text(
                options
                    .get(dropdown.selected() as usize)
                    .cloned()
                    .unwrap_or_default(),
            ),
        }
    }
}

fn form(
    action: &str,
    fields: &[burrow_core::FormField],
    submit: &str,
    on_action: &OnAction,
) -> gtk::Widget {
    let column = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let mut inputs = Vec::new();
    for field in fields {
        let (widget, input): (gtk::Widget, Input) = match &field.input {
            FormInput::Text { value } => {
                let entry = gtk::Entry::new();
                entry.set_text(value);
                (entry.clone().upcast(), Input::Entry(entry))
            }
            FormInput::Multiline { value } => {
                let view = gtk::TextView::new();
                view.buffer().set_text(value);
                view.set_wrap_mode(gtk::WrapMode::WordChar);
                view.set_monospace(true);
                view.add_css_class("multiline");
                let scroller = gtk::ScrolledWindow::builder()
                    .child(&view)
                    .min_content_height(120)
                    .build();
                (scroller.upcast(), Input::Multiline(view))
            }
            FormInput::Check { value } => {
                let check = gtk::CheckButton::with_label(&field.label);
                check.set_active(*value);
                (check.clone().upcast(), Input::Check(check))
            }
            FormInput::Choice { options, value } => {
                let names: Vec<&str> = options.iter().map(String::as_str).collect();
                let dropdown = gtk::DropDown::from_strings(&names);
                if let Some(index) = options.iter().position(|o| o == value) {
                    dropdown.set_selected(index as u32);
                }
                dropdown.set_halign(gtk::Align::Start);
                (
                    dropdown.clone().upcast(),
                    Input::Choice(dropdown, options.clone()),
                )
            }
        };
        // A check button carries its own label.
        if !matches!(input, Input::Check(_)) {
            let label = gtk::Label::new(Some(&field.label));
            label.set_xalign(0.0);
            column.append(&label);
        }
        column.append(&widget);
        inputs.push((field.name.clone(), input));
    }

    let button = gtk::Button::with_label(submit);
    button.add_css_class("suggested-action");
    button.set_halign(gtk::Align::Start);
    button.set_margin_top(4);
    let (action, on_action) = (action.to_string(), on_action.clone());
    button.connect_clicked(move |_| {
        let values = inputs
            .iter()
            .map(|(name, input)| (name.clone(), input.value()))
            .collect();
        on_action(action.clone(), values);
    });
    column.append(&button);
    column.upcast()
}

/// What each permission lets an addon do, for the approval dialog.
fn describe_permission(permission: &str) -> &str {
    match permission {
        "nodes:read" => "See every item in your sidebar, and changes to them",
        "nodes:write" => "Create, change and delete its own kinds of item",
        other => other,
    }
}

pub fn state_text(state: &AddonState) -> String {
    match state {
        AddonState::NeedsApproval { changed: false } => "Not approved".into(),
        AddonState::NeedsApproval { changed: true } => "Changed since you approved it".into(),
        AddonState::Ready => "Approved".into(),
        AddonState::Running => "Running".into(),
        AddonState::Failed { error } => format!("Stopped: {error}"),
    }
}

/// Shows what an addon asks for and calls `approve` if the user agrees.
pub fn review(parent: &impl IsA<gtk::Window>, addon: &AddonInfo, approve: impl Fn() + 'static) {
    let window = gtk::Window::builder()
        .title("Approve addon")
        .modal(true)
        .transient_for(parent)
        .default_width(460)
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 10);
    content.set_margin_top(18);
    content.set_margin_bottom(18);
    content.set_margin_start(18);
    content.set_margin_end(18);

    let title = gtk::Label::new(Some(&format!("{} {}", addon.name, addon.version)));
    title.set_xalign(0.0);
    title.add_css_class("page-title");
    content.append(&title);
    if matches!(addon.state, AddonState::NeedsApproval { changed: true }) {
        let changed = gtk::Label::new(Some(
            "This addon changed since you approved it: its files, permissions or kinds.",
        ));
        changed.set_xalign(0.0);
        changed.set_wrap(true);
        changed.add_css_class("warning-text");
        content.append(&changed);
    }

    let mut fields = vec![
        ("Id", addon.id.clone()),
        ("Program", addon.program.display().to_string()),
    ];
    if !addon.kinds.is_empty() {
        let kinds: Vec<&str> = addon.kinds.iter().map(|k| k.label.as_str()).collect();
        fields.push(("Handles", kinds.join(", ")));
    }
    let permissions = if addon.permissions.is_empty() {
        "None: it only answers when you use it".to_string()
    } else {
        addon
            .permissions
            .iter()
            .map(|p| format!("• {}", describe_permission(p)))
            .collect::<Vec<_>>()
            .join("\n")
    };
    fields.push(("Can", permissions));
    let grid = gtk::Grid::builder()
        .row_spacing(6)
        .column_spacing(16)
        .build();
    for (row, (label, value)) in fields.iter().enumerate() {
        let label = gtk::Label::new(Some(label));
        label.set_xalign(0.0);
        label.set_yalign(0.0);
        label.add_css_class("dim");
        let value = gtk::Label::new(Some(value));
        value.set_xalign(0.0);
        value.set_wrap(true);
        value.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        value.set_max_width_chars(48);
        grid.attach(&label, 0, row as i32, 1, 1);
        grid.attach(&value, 1, row as i32, 1, 1);
    }
    content.append(&grid);

    let warning = gtk::Label::new(Some(
        "An addon is a normal program that runs as you. Its permissions limit what it \
         can do through Burrow, not what it can do on your computer. Only approve \
         addons you trust.",
    ));
    warning.set_xalign(0.0);
    warning.set_wrap(true);
    warning.set_max_width_chars(56);
    warning.add_css_class("dim");
    content.append(&warning);

    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::End);
    buttons.set_margin_top(6);
    let cancel = gtk::Button::with_label("Cancel");
    let ok = gtk::Button::with_label("Approve");
    ok.add_css_class("suggested-action");
    buttons.append(&cancel);
    buttons.append(&ok);
    content.append(&buttons);
    window.set_child(Some(&content));

    ok.connect_clicked({
        let window = window.clone();
        move |_| {
            window.close();
            approve();
        }
    });
    cancel.connect_clicked({
        let window = window.clone();
        move |_| window.close()
    });
    dialogs::close_on_escape(&window);
    window.present();
    cancel.grab_focus();
}

/// Lists the installed addons. `review` and `revoke` get the addon's id.
pub fn manage(
    parent: &impl IsA<gtk::Window>,
    addons: &[AddonInfo],
    review: impl Fn(String) + 'static,
    revoke: impl Fn(String) + 'static,
    open_folder: impl Fn() + 'static,
) {
    let window = gtk::Window::builder()
        .title("Addons")
        .modal(true)
        .transient_for(parent)
        .default_width(480)
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 10);
    content.set_margin_top(18);
    content.set_margin_bottom(18);
    content.set_margin_start(18);
    content.set_margin_end(18);

    let (review, revoke) = (Rc::new(review), Rc::new(revoke));
    if addons.is_empty() {
        content.append(&text(
            "No addons are installed. An addon is a folder with an addon.toml \
             inside the addons folder.",
        ));
    }
    for addon in addons {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        let labels = gtk::Box::new(gtk::Orientation::Vertical, 2);
        labels.set_hexpand(true);
        let name = gtk::Label::new(Some(&format!("{} {}", addon.name, addon.version)));
        name.set_xalign(0.0);
        let state = gtk::Label::new(Some(&state_text(&addon.state)));
        state.set_xalign(0.0);
        state.set_wrap(true);
        state.add_css_class("dim");
        labels.append(&name);
        labels.append(&state);
        row.append(&labels);

        let id = addon.id.clone();
        let button = if usable(addon) {
            let button = gtk::Button::with_label("Revoke");
            let (window, revoke) = (window.clone(), revoke.clone());
            button.connect_clicked(move |_| {
                window.close();
                revoke(id.clone());
            });
            button
        } else {
            let button = gtk::Button::with_label("Review…");
            let (window, review) = (window.clone(), review.clone());
            button.connect_clicked(move |_| {
                window.close();
                review(id.clone());
            });
            button
        };
        button.set_valign(gtk::Align::Center);
        row.append(&button);
        content.append(&row);
    }

    let folder = gtk::Button::with_label("Open addons folder");
    folder.set_halign(gtk::Align::Start);
    folder.set_margin_top(6);
    folder.connect_clicked(move |_| open_folder());
    content.append(&folder);

    window.set_child(Some(&content));
    dialogs::close_on_escape(&window);
    window.present();
}
