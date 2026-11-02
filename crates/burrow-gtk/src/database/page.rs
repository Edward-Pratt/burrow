//! Building the database page: the editor, the result grid, the history and the schema beside them.

use std::rc::Rc;

use burrow_core::NodeId;
use relm4::gtk::prelude::*;
use relm4::gtk::{gio, glib};
use relm4::{ComponentSender, gtk};
use sourceview5::prelude::*;

use crate::app::{App, AppMsg};
use crate::session::{self, SessionPage};

use super::schema::schema_tree;
use super::{Database, Msg};

impl Database {
    pub fn new(node: NodeId, sender: &ComponentSender<App>) -> Database {
        let send = |msg: Msg| {
            let sender = sender.clone();
            move || {
                sender.input(AppMsg::Database {
                    node,
                    msg: msg.clone(),
                })
            }
        };
        let button = |label: &str, msg: Msg| {
            let button = gtk::Button::with_label(label);
            let send = send(msg);
            button.connect_clicked(move |_| send());
            button
        };

        let page = gtk::Box::new(gtk::Orientation::Vertical, 8);
        page.add_css_class("database-view");

        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let run = button("Run", Msg::Run);
        run.add_css_class("suggested-action");
        run.set_tooltip_text(Some("Run the selection, or everything (Ctrl+Enter)"));
        let cancel = button("Stop", Msg::Cancel);
        cancel.set_sensitive(false);
        let read_only_check = gtk::CheckButton::with_label("Read-only");
        read_only_check.set_active(true);
        read_only_check.set_tooltip_text(Some(
            "Transactions can't change anything. A query can still turn this off with SET.",
        ));
        read_only_check.connect_toggled({
            let sender = sender.clone();
            move |check| {
                let msg = Msg::ReadOnly(check.is_active());
                sender.input(AppMsg::Database { node, msg });
            }
        });
        let writable_note = gtk::Label::new(Some("Changes allowed"));
        writable_note.add_css_class("warning-text");
        writable_note.set_visible(false);
        bar.append(&run);
        bar.append(&cancel);
        bar.append(&read_only_check);
        bar.append(&writable_note);
        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        bar.append(&spacer);
        let history_button = button("History…", Msg::History);
        bar.append(&history_button);
        bar.append(&button("Export CSV…", Msg::Export));

        let buffer = sourceview5::Buffer::new(None::<&gtk::TextTagTable>);
        if let Some(sql) = sourceview5::LanguageManager::default().language("sql") {
            buffer.set_language(Some(&sql));
        }
        let editor = sourceview5::View::with_buffer(&buffer);
        editor.set_monospace(true);
        editor.set_show_line_numbers(true);
        editor.set_highlight_current_line(true);
        editor.set_auto_indent(true);
        editor.set_insert_spaces_instead_of_tabs(true);
        editor.set_tab_width(4);
        editor.add_css_class("sql-editor");
        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed({
            let send = send(Msg::Run);
            move |_, key, _, modifiers| {
                let enter = matches!(key, gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter);
                if enter && modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK) {
                    send();
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            }
        });
        editor.add_controller(keys);
        follow_theme(&buffer, &editor);
        let editor_scroller = gtk::ScrolledWindow::builder()
            .child(&editor)
            .vexpand(true)
            .min_content_height(120)
            .build();

        let status = gtk::Label::new(None);
        status.set_xalign(0.0);
        status.set_ellipsize(gtk::pango::EllipsizeMode::End);
        status.add_css_class("dim");
        let error = gtk::Label::new(None);
        error.set_xalign(0.0);
        error.set_wrap(true);
        error.set_selectable(true);
        error.add_css_class("error-text");
        error.set_visible(false);

        let rows = gio::ListStore::new::<glib::BoxedAnyObject>();
        let selection = gtk::SingleSelection::new(Some(rows.clone()));
        selection.set_autoselect(false);
        selection.set_can_unselect(true);
        let grid = gtk::ColumnView::new(Some(selection.clone()));
        grid.add_css_class("result-grid");
        grid.set_show_column_separators(true);
        grid.set_reorderable(false);
        let copy = gtk::EventControllerKey::new();
        copy.connect_key_pressed({
            let send = send(Msg::CopyRow);
            move |_, key, _, modifiers| {
                if key == gtk::gdk::Key::c
                    && modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK)
                {
                    send();
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            }
        });
        grid.add_controller(copy);
        let grid_scroller = gtk::ScrolledWindow::builder()
            .child(&grid)
            .vexpand(true)
            .build();
        let results = gtk::Box::new(gtk::Orientation::Vertical, 6);
        results.append(&status);
        results.append(&error);
        results.append(&grid_scroller);

        let work = gtk::Paned::new(gtk::Orientation::Vertical);
        work.set_start_child(Some(&editor_scroller));
        work.set_end_child(Some(&results));
        work.set_resize_start_child(false);
        work.set_shrink_start_child(false);
        work.set_position(180);

        let (tree, schemas) = schema_tree(node, sender);
        let tree_header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let tree_title = gtk::Label::new(Some("Tables"));
        tree_title.set_xalign(0.0);
        tree_title.set_hexpand(true);
        tree_title.add_css_class("heading");
        let refresh = gtk::Button::from_icon_name("view-refresh-symbolic");
        refresh.add_css_class("flat");
        refresh.set_tooltip_text(Some("List the tables again"));
        let send_refresh = send(Msg::RefreshSchema);
        refresh.connect_clicked(move |_| send_refresh());
        tree_header.append(&tree_title);
        tree_header.append(&refresh);
        let tree_scroller = gtk::ScrolledWindow::builder()
            .child(&tree)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();
        let side = gtk::Box::new(gtk::Orientation::Vertical, 6);
        side.add_css_class("schema-tree");
        side.append(&tree_header);
        side.append(&tree_scroller);

        let split = gtk::Paned::new(gtk::Orientation::Horizontal);
        split.set_start_child(Some(&side));
        split.set_end_child(Some(&work));
        split.set_shrink_start_child(false);
        split.set_resize_start_child(false);
        split.set_position(220);

        let main = gtk::Box::new(gtk::Orientation::Vertical, 8);
        main.append(&bar);
        main.append(&split);
        split.set_vexpand(true);

        let session = SessionPage::new(&main, send(Msg::Session(session::Msg::Reconnect)));
        session.widget().set_vexpand(true);
        page.append(session.widget());

        Database {
            node,
            page,
            session,
            read_only: true,
            running: false,
            buffer,
            editor,
            run,
            cancel,
            read_only_check,
            writable_note,
            history_button,
            status,
            summary: String::new(),
            note_id: 0,
            error,
            grid,
            rows,
            selection,
            schemas,
            export_sql: None,
        }
    }
}

/// Picks the editor's colours to match the window's, light or dark, now and
/// whenever they change. Without libadwaita nothing does it for us.
pub fn follow_theme(buffer: &sourceview5::Buffer, editor: &sourceview5::View) {
    let apply = {
        let buffer = buffer.clone();
        move |widget: &gtk::Widget| {
            // Light text means a dark theme.
            let color = widget.color();
            let dark = color.red() + color.green() + color.blue() > 1.5;
            let name = if dark { "Adwaita-dark" } else { "Adwaita" };
            if let Some(scheme) = sourceview5::StyleSchemeManager::default().scheme(name) {
                buffer.set_style_scheme(Some(&scheme));
            }
        }
    };
    let apply = Rc::new(apply);
    editor.connect_map({
        let apply = apply.clone();
        move |editor| apply(editor.upcast_ref())
    });
    if let Some(settings) = gtk::Settings::default() {
        let editor = editor.downgrade();
        let follow = move |_: &gtk::Settings| {
            if let Some(editor) = editor.upgrade() {
                // The new colours apply once the style is recomputed.
                let apply = apply.clone();
                glib::idle_add_local_once(move || apply(editor.upcast_ref()));
            }
        };
        settings.connect_gtk_interface_color_scheme_notify(follow.clone());
        settings.connect_gtk_theme_name_notify(follow);
    }
}
