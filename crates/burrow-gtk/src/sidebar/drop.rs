//! Dropping a dragged row on a folder, and the highlight that shows where it lands.

use std::cell::RefCell;
use std::rc::Rc;

use burrow_core::NodeId;
use relm4::gtk::prelude::*;
use relm4::gtk::{gdk, glib, graphene};
use relm4::{ComponentSender, gtk};

use crate::app::{App, AppMsg};

use super::model::NodeObject;
use super::tree::can_move;
use super::{Sidebar, row_at, row_for};

impl Sidebar {
    /// Lets rows be dropped on a folder row to move into it, on a leaf to
    /// move beside it, and below the rows to move to the top level.
    pub(super) fn add_drop(&self, sender: &ComponentSender<App>) {
        let drop = gtk::DropTarget::new(NodeObject::static_type(), gdk::DragAction::MOVE);
        // The row or list lit up as the drop spot.
        let lit: Rc<RefCell<Option<gtk::Widget>>> = Rc::default();
        let hover = Rc::new(glib::clone!(
            #[weak(rename_to = view)]
            self.view,
            #[strong(rename_to = state)]
            self.state,
            #[strong]
            lit,
            #[upgrade_or]
            gdk::DragAction::empty(),
            move |x: f64, y: f64| {
                let (row, folder) = drop_spot(&view, x, y);
                let allowed = (state.dragged.get())
                    .is_some_and(|id| can_move(id, folder, |id| state.parent_of(id)));
                let spot = match row {
                    _ if !allowed => None,
                    Some(row) => row_widget(&view, &row),
                    None => Some(view.clone().upcast()),
                };
                light(&lit, spot);
                if allowed {
                    gdk::DragAction::MOVE
                } else {
                    gdk::DragAction::empty()
                }
            }
        ));
        drop.connect_enter({
            let hover = hover.clone();
            move |_, x, y| hover(x, y)
        });
        drop.connect_motion(move |_, x, y| hover(x, y));
        drop.connect_leave({
            let lit = lit.clone();
            move |_| light(&lit, None)
        });
        let sender = sender.clone();
        drop.connect_drop(glib::clone!(
            #[weak(rename_to = view)]
            self.view,
            #[strong(rename_to = state)]
            self.state,
            #[strong(rename_to = tree)]
            self.tree,
            #[strong(rename_to = selection)]
            self.selection,
            #[upgrade_or]
            false,
            move |_, value, x, y| {
                light(&lit, None);
                let Ok(object) = value.get::<NodeObject>() else {
                    return false;
                };
                let id = object.node_id();
                let (_, folder) = drop_spot(&view, x, y);
                if !can_move(id, folder, |id| state.parent_of(id)) {
                    return false;
                }
                // Selected, so the selection follows it to its new folder
                // and shows where it went.
                if let Some(row) = row_for(&state, &tree, id) {
                    selection.set_selected(row.position());
                }
                sender.input(AppMsg::MoveNode { id, parent: folder });
                true
            }
        ));
        self.view.add_controller(drop);
    }
}

/// Where a drop at a point in the list puts a node, and the row to light
/// up for it: a folder row takes it in, a leaf's folder takes it beside
/// the leaf, and the space below the rows is the top level. The top level
/// lights up the whole list, as does a leaf there.
pub(super) fn drop_spot(
    view: &gtk::ListView,
    x: f64,
    y: f64,
) -> (Option<gtk::TreeListRow>, Option<NodeId>) {
    let Some(row) = row_at(view, x, y) else {
        return (None, None);
    };
    let Some(object) = row.item().and_downcast::<NodeObject>() else {
        return (None, None);
    };
    if object.folder() {
        (Some(row), Some(object.node_id()))
    } else {
        (row.parent(), object.parent_id())
    }
}

/// Moves the drop spot's highlight to `spot`.
pub(super) fn light(lit: &RefCell<Option<gtk::Widget>>, spot: Option<gtk::Widget>) {
    let mut lit = lit.borrow_mut();
    if *lit == spot {
        return;
    }
    if let Some(old) = lit.take() {
        old.remove_css_class("drop-target");
    }
    if let Some(new) = &spot {
        new.add_css_class("drop-target");
    }
    *lit = spot;
}

/// The widget drawing `row`, if it is on screen.
pub(super) fn row_widget(view: &gtk::ListView, row: &gtk::TreeListRow) -> Option<gtk::Widget> {
    let mut child = view.first_child();
    while let Some(widget) = child {
        let expander = widget.first_child().and_downcast::<gtk::TreeExpander>();
        if widget.is_mapped() && expander.and_then(|e| e.list_row()).as_ref() == Some(row) {
            return Some(widget);
        }
        child = widget.next_sibling();
    }
    None
}

pub(super) fn to_pane(
    widget: &impl IsA<gtk::Widget>,
    pane: &gtk::Box,
    x: f64,
    y: f64,
) -> (f64, f64) {
    let point = graphene::Point::new(x as f32, y as f32);
    let point = widget.compute_point(pane, &point).unwrap_or(point);
    (point.x().into(), point.y().into())
}
