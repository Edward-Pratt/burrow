//! Row widgets: the factory that makes them, dragging them, and drawing each from its node.

use std::cell::RefCell;
use std::rc::Rc;

use relm4::gtk;
use relm4::gtk::prelude::*;
use relm4::gtk::{gdk, glib};

use super::choosers::is_color;
use super::model::NodeObject;
use super::model::State;
use super::sync_children;

/// The widgets of one row.
#[derive(Clone)]
pub(super) struct Row {
    pub(super) expander: gtk::TreeExpander,
    pub(super) icon: gtk::Image,
    pub(super) name: gtk::Label,
    pub(super) dot: gtk::Label,
    pub(super) running: gtk::Label,
}

/// A row's hold on what it shows, let go when it shows something else.
pub(super) struct Bound {
    pub(super) object: NodeObject,
    pub(super) object_handler: glib::SignalHandlerId,
    pub(super) row: gtk::TreeListRow,
    pub(super) row_handler: glib::SignalHandlerId,
}

impl Bound {
    pub(super) fn release(self) {
        self.object.disconnect(self.object_handler);
        self.row.disconnect(self.row_handler);
    }
}

pub(super) fn factory(state: &Rc<State>) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    let state = state.clone();
    factory.connect_setup(move |_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let row = Row::new();
        row.add_drag(&state);
        item.set_child(Some(&row.expander));
        // The row's handlers live in this closure, which the list item
        // calls whenever it is given another row to show.
        let bound: RefCell<Option<Bound>> = RefCell::default();
        let state = state.clone();
        item.connect_item_notify(move |item| {
            if let Some(old) = bound.take() {
                old.release();
            }
            let tree_row = item.item().and_downcast::<gtk::TreeListRow>();
            row.expander.set_list_row(tree_row.as_ref());
            let Some(tree_row) = tree_row else { return };
            let Some(object) = tree_row.item().and_downcast::<NodeObject>() else {
                return;
            };
            bound.replace(Some(row.bind(&state, tree_row, object)));
        });
    });
    factory
}

impl Row {
    pub(super) fn new() -> Self {
        let content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let icon = gtk::Image::new();
        content.append(&icon);
        let name = gtk::Label::new(None);
        name.set_xalign(0.0);
        name.set_hexpand(true);
        name.set_ellipsize(gtk::pango::EllipsizeMode::End);
        content.append(&name);
        let dot = gtk::Label::new(None);
        content.append(&dot);
        let running = gtk::Label::new(Some("running"));
        running.add_css_class("running-badge");
        content.append(&running);
        // The expander draws the indent and, for folders, the disclosure
        // arrow; leaves get a space the arrow's width so names line up.
        let expander = gtk::TreeExpander::new();
        expander.set_child(Some(&content));
        Row {
            expander,
            icon,
            name,
            dot,
            running,
        }
    }

    /// Lets the row be dragged onto a folder.
    pub(super) fn add_drag(&self, state: &Rc<State>) {
        let drag = gtk::DragSource::new();
        drag.set_actions(gdk::DragAction::MOVE);
        // Weak: the drag source belongs to the expander.
        drag.connect_prepare(glib::clone!(
            #[weak(rename_to = expander)]
            self.expander,
            #[strong]
            state,
            #[upgrade_or_default]
            move |_, _, _| {
                let object = expander.list_row()?.item().and_downcast::<NodeObject>()?;
                state.dragged.set(Some(object.node_id()));
                // The object itself rather than its id as text, so a drop
                // on a terminal or a text field doesn't type the id.
                Some(gdk::ContentProvider::for_value(&object.to_value()))
            }
        ));
        drag.connect_drag_begin(glib::clone!(
            #[weak(rename_to = expander)]
            self.expander,
            move |_, drag| {
                let row = expander.list_row().and_then(|r| r.item());
                let Some(object) = row.and_downcast::<NodeObject>() else {
                    return;
                };
                let content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
                content.add_css_class("sidebar-drag");
                content.append(&gtk::Image::from_icon_name(&object.icon()));
                content.append(&gtk::Label::new(Some(&object.name())));
                let icon = gtk::DragIcon::for_drag(drag).downcast::<gtk::DragIcon>();
                if let Ok(icon) = icon {
                    icon.set_child(Some(&content));
                }
            }
        ));
        drag.connect_drag_end(glib::clone!(
            #[strong]
            state,
            move |_, _, _| state.dragged.set(None)
        ));
        self.expander.add_controller(drag);
    }

    pub(super) fn bind(
        &self,
        state: &Rc<State>,
        row: gtk::TreeListRow,
        object: NodeObject,
    ) -> Bound {
        self.draw(&object, row.is_expanded());
        let object_handler = object.connect_notify_local(None, {
            let (widgets, row) = (self.clone(), row.downgrade());
            move |object, _| {
                let expanded = row.upgrade().is_some_and(|r| r.is_expanded());
                widgets.draw(object, expanded);
            }
        });
        // Clicking the arrow opens the row directly, so the set of open
        // folders learns of it here.
        let row_handler = row.connect_expanded_notify({
            let (widgets, state, object) = (self.clone(), state.clone(), object.downgrade());
            move |row| {
                // Closing a folder throws away the rows inside it, and they
                // report being closed. Those folders stay in the set, to
                // open again with it. A thrown-away row isn't expandable.
                if !row.is_expandable() {
                    return;
                }
                let Some(object) = object.upgrade() else {
                    return;
                };
                let open = row.is_expanded();
                if open {
                    state.expanded.borrow_mut().insert(object.node_id());
                } else {
                    state.expanded.borrow_mut().remove(&object.node_id());
                }
                widgets.draw(&object, open);
                if open {
                    let expanded = state.expanded.borrow().clone();
                    sync_children(row, &expanded);
                }
            }
        });
        Bound {
            object,
            object_handler,
            row,
            row_handler,
        }
    }

    pub(super) fn draw(&self, object: &NodeObject, expanded: bool) {
        let icon = if object.folder() && expanded {
            "folder-open-symbolic".into()
        } else {
            object.icon()
        };
        self.icon.set_icon_name(Some(&icon));
        let name = object.name();
        self.name.set_label(&name);
        self.expander.set_tooltip_text(Some(&name));
        match object.color().filter(|c| is_color(c)) {
            Some(color) => {
                self.dot
                    .set_markup(&format!("<span foreground=\"{color}\">●</span>"));
                self.dot.set_visible(true);
            }
            None => self.dot.set_visible(false),
        }
        self.running.set_visible(object.running());
    }
}
