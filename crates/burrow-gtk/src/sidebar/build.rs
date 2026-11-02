//! Building the sidebar: its list view, its keys and the context menu.

use std::rc::Rc;

use relm4::gtk::prelude::*;
use relm4::gtk::{gdk, glib};
use relm4::{ComponentSender, gtk};

use crate::app::{App, AppMsg};

use super::drop::{row_widget, to_pane};
use super::model::NodeObject;
use super::model::State;
use super::rows::factory;
use super::{Sidebar, node_of, row_at};

impl Sidebar {
    pub fn new(sender: &ComponentSender<App>) -> Self {
        let state = Rc::new(State::default());
        let tree = gtk::TreeListModel::new(state.store(None), false, false, {
            let state = state.clone();
            move |item| {
                let object = item.downcast_ref::<NodeObject>()?;
                let store = object
                    .folder()
                    .then(|| state.store(Some(object.node_id())))?;
                Some(store.upcast())
            }
        });
        let selection = gtk::SingleSelection::new(Some(tree.clone()));
        selection.set_autoselect(false);
        selection.set_can_unselect(true);

        let view = gtk::ListView::new(Some(selection.clone()), Some(factory(&state)));
        view.add_css_class("navigation-sidebar");
        // Enter and double-click open, as in a file manager.
        view.set_single_click_activate(false);
        view.connect_activate({
            let sender = sender.clone();
            move |view, position| {
                let row = view.model().and_then(|m| m.item(position));
                if let Some(id) = row.and_downcast_ref().and_then(node_of) {
                    sender.input(AppMsg::RowActivated(id));
                }
            }
        });

        let scroller = gtk::ScrolledWindow::builder()
            .child(&view)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();
        let placeholder = gtk::Label::new(Some("No hosts yet.\nPress + or right-click here."));
        placeholder.set_wrap(true);
        placeholder.set_justify(gtk::Justification::Center);
        placeholder.set_halign(gtk::Align::Center);
        placeholder.set_valign(gtk::Align::Center);
        placeholder.set_margin_start(24);
        placeholder.set_margin_end(24);
        placeholder.add_css_class("dim");
        // Clicks go through to the list, whose menu offers "New …".
        placeholder.set_can_target(false);
        // Hidden until the first load, so it doesn't flash before the
        // nodes arrive.
        placeholder.set_visible(false);
        state.store(None).connect_items_changed({
            let placeholder = placeholder.clone();
            move |store, _, _, _| placeholder.set_visible(store.n_items() == 0)
        });
        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&scroller));
        overlay.add_overlay(&placeholder);
        let pane = gtk::Box::new(gtk::Orientation::Vertical, 0);
        pane.add_css_class("sidebar");
        pane.set_size_request(240, -1);
        pane.append(&overlay);

        let right_click = gtk::GestureClick::new();
        right_click.set_button(gdk::BUTTON_SECONDARY);
        // Weak: the gesture belongs to the view, which is inside the pane.
        let input = sender.clone();
        right_click.connect_pressed(glib::clone!(
            #[weak]
            view,
            #[weak]
            pane,
            #[strong]
            selection,
            move |gesture, _, x, y| {
                gesture.set_state(gtk::EventSequenceState::Claimed);
                let row = row_at(&view, x, y);
                // Selected first, so the menu and the keys that act on the
                // selection agree about which node they mean.
                if let Some(row) = &row {
                    selection.set_selected(row.position());
                }
                let (x, y) = to_pane(&view, &pane, x, y);
                let node = row.as_ref().and_then(node_of);
                input.input(AppMsg::ContextMenu { node, x, y });
            }
        ));
        view.add_controller(right_click);

        let sidebar = Sidebar {
            pane,
            view,
            selection,
            tree,
            placeholder,
            state,
        };
        sidebar.add_keys(sender);
        sidebar.add_drop(sender);
        sidebar
    }

    /// Delete, F2 and the menu keys only apply while the sidebar has focus,
    /// so they never eat a keypress meant for a text field or a terminal.
    pub(super) fn add_keys(&self, sender: &ComponentSender<App>) {
        let keys = gtk::EventControllerKey::new();
        // Weak: the controller belongs to the view, which is inside the pane.
        let sender = sender.clone();
        keys.connect_key_pressed(glib::clone!(
            #[weak(rename_to = view)]
            self.view,
            #[weak(rename_to = pane)]
            self.pane,
            #[strong(rename_to = selection)]
            self.selection,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_, key, _, modifiers| {
                let shift = modifiers.contains(gdk::ModifierType::SHIFT_MASK);
                let message = match key {
                    gdk::Key::Delete => AppMsg::DeleteSelected,
                    gdk::Key::F2 => AppMsg::RenameSelected,
                    gdk::Key::Menu => menu_for_selection(&view, &selection, &pane),
                    gdk::Key::F10 if shift => menu_for_selection(&view, &selection, &pane),
                    _ => return glib::Propagation::Proceed,
                };
                sender.input(message);
                glib::Propagation::Stop
            }
        ));
        self.view.add_controller(keys);
    }
}

/// The context menu for the selected row, opened from the keyboard. It
/// points just below the row, or at the top of the list when no row is
/// selected.
pub(super) fn menu_for_selection(
    view: &gtk::ListView,
    selection: &gtk::SingleSelection,
    pane: &gtk::Box,
) -> AppMsg {
    let row = selection.selected_item().and_downcast::<gtk::TreeListRow>();
    let node = row.as_ref().and_then(node_of);
    let widget = row.and_then(|row| row_widget(view, &row));
    let bounds = widget.and_then(|w| w.compute_bounds(pane));
    let (x, y) = match bounds {
        Some(b) => (f64::from(b.x()) + 24.0, f64::from(b.y() + b.height())),
        None => to_pane(view, pane, 24.0, 0.0),
    };
    AppMsg::ContextMenu { node, x, y }
}
