//! The command palette (Ctrl+Shift+P): type to filter commands and nodes, Enter
//! to run the highlighted one.

use std::cell::RefCell;
use std::rc::Rc;

use relm4::gtk;
use relm4::gtk::glib;
use relm4::gtk::prelude::*;

use crate::dialogs::close_on_escape;

pub struct Item<M> {
    pub title: String,
    pub subtitle: String,
    pub message: M,
}

/// True when every word of `query` appears in the title or subtitle.
pub fn matches(query: &str, title: &str, subtitle: &str) -> bool {
    let haystack = format!("{title} {subtitle}").to_lowercase();
    query
        .to_lowercase()
        .split_whitespace()
        .all(|word| haystack.contains(word))
}

/// Opens the palette. `run` receives the chosen item's message.
pub fn show<M: Clone + 'static>(
    parent: &impl IsA<gtk::Window>,
    items: Vec<Item<M>>,
    run: impl Fn(M) + 'static,
) {
    let window = gtk::Window::builder()
        .modal(true)
        .transient_for(parent)
        .decorated(false)
        .default_width(560)
        .default_height(380)
        .build();
    window.add_css_class("palette");

    let search = gtk::Entry::new();
    search.set_placeholder_text(Some("Type a command or a name"));
    search.set_primary_icon_name(Some("system-search-symbolic"));
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::Browse);
    let scroller = gtk::ScrolledWindow::builder()
        .child(&list)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();

    let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
    content.set_margin_top(8);
    content.set_margin_bottom(8);
    content.set_margin_start(8);
    content.set_margin_end(8);
    content.append(&search);
    content.append(&scroller);
    window.set_child(Some(&content));

    let items = Rc::new(items);
    // Indexes into `items` of the rows currently shown.
    let shown: Rc<RefCell<Vec<usize>>> = Rc::default();
    let run = Rc::new(run);

    let refilter = {
        let (items, shown, list) = (items.clone(), shown.clone(), list.clone());
        move |query: &str| {
            list.remove_all();
            let mut visible = shown.borrow_mut();
            visible.clear();
            for (index, item) in items.iter().enumerate() {
                if matches(query, &item.title, &item.subtitle) {
                    list.append(&row(item));
                    visible.push(index);
                }
            }
            if let Some(first) = list.row_at_index(0) {
                list.select_row(Some(&first));
            }
        }
    };
    refilter("");
    search.connect_changed({
        let refilter = refilter.clone();
        move |entry| refilter(&entry.text())
    });

    let choose = Rc::new({
        let (window, items, shown, run) = (window.clone(), items, shown, run);
        move |row: Option<&gtk::ListBoxRow>| {
            let Some(row) = row else { return };
            let Some(&index) = shown.borrow().get(row.index() as usize) else {
                return;
            };
            window.close();
            run(items[index].message.clone());
        }
    });
    list.connect_row_activated({
        let choose = choose.clone();
        move |_, row| choose(Some(row))
    });
    search.connect_activate({
        let (choose, list) = (choose.clone(), list.clone());
        move |_| choose(list.selected_row().as_ref())
    });

    // Arrow keys move the highlight while focus stays in the search box.
    let keys = gtk::EventControllerKey::new();
    keys.connect_key_pressed({
        let (list, search) = (list.clone(), search.clone());
        move |_, key, _, _| {
            let step = match key {
                gtk::gdk::Key::Down => 1,
                gtk::gdk::Key::Up => -1,
                _ => return glib::Propagation::Proceed,
            };
            let current = list.selected_row().map_or(-1, |r| r.index());
            if let Some(next) = list.row_at_index(current + step) {
                list.select_row(Some(&next));
                // Focusing the row scrolls it into view; then hand focus
                // straight back so typing keeps working.
                next.grab_focus();
                search.grab_focus_without_selecting();
            }
            glib::Propagation::Stop
        }
    });
    search.add_controller(keys);
    close_on_escape(&window);

    window.present();
    search.grab_focus();
}

fn row<M>(item: &Item<M>) -> gtk::ListBoxRow {
    let content = gtk::Box::new(gtk::Orientation::Vertical, 2);
    content.set_margin_top(4);
    content.set_margin_bottom(4);
    content.set_margin_start(6);
    content.set_margin_end(6);
    let title = gtk::Label::new(Some(&item.title));
    title.set_xalign(0.0);
    content.append(&title);
    if !item.subtitle.is_empty() {
        let subtitle = gtk::Label::new(Some(&item.subtitle));
        subtitle.set_xalign(0.0);
        subtitle.add_css_class("palette-subtitle");
        content.append(&subtitle);
    }
    let row = gtk::ListBoxRow::new();
    row.set_child(Some(&content));
    row
}

#[cfg(test)]
mod tests {
    use super::matches;

    #[test]
    fn every_word_must_match() {
        assert!(matches("new fol", "New folder", ""));
        assert!(matches("prod work", "prod-web", "Work / Servers"));
        assert!(!matches("new host", "New folder", ""));
        assert!(matches("", "anything", ""));
    }
}
