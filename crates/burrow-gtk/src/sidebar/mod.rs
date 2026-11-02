//! The sidebar: the core's flat node list shown as a tree.
//!
//! Each folder's children, and the top level's, live in a sorted
//! `gio::ListStore` of [`NodeObject`]s, and a `gtk::TreeListModel` over the
//! top level's store shows them as a tree. Changes update the objects in
//! place and rows redraw from them, so the selection, the focus and the
//! scroll position survive them.

use std::collections::HashSet;
use std::rc::Rc;

use burrow_core::NodeId;
use relm4::gtk;
use relm4::gtk::prelude::*;

mod build;
mod choosers;
mod drop;
mod model;
mod nodes;
mod rows;
mod tree;

use model::NodeObject;
use model::State;
use tree::ancestors;

pub use choosers::{colour_dialog, move_dialog};
pub use model::icon_name;
pub use tree::{location, move_choices};

pub struct Sidebar {
    /// The pane around the list. Popovers hang off it, never off the list
    /// view: the view owns its children and adds and removes them as rows
    /// come and go, and a popover among a list's children once hung the
    /// window (see commit 9fa1e36).
    pub pane: gtk::Box,
    view: gtk::ListView,
    selection: gtk::SingleSelection,
    tree: gtk::TreeListModel,
    /// Shown over the list while there are no nodes.
    placeholder: gtk::Label,
    state: Rc<State>,
}

impl Sidebar {
    /// Moves the keyboard focus to the selected row, or to the first one.
    pub fn focus(&self) {
        let selected = self.selection.selected();
        if selected != gtk::INVALID_LIST_POSITION {
            self.view
                .scroll_to(selected, gtk::ListScrollFlags::FOCUS, None);
        } else if self.selection.n_items() > 0 {
            let flags = gtk::ListScrollFlags::FOCUS | gtk::ListScrollFlags::SELECT;
            self.view.scroll_to(0, flags, None);
        }
        // `scroll_to` only moves the focus if the list has it already, and
        // otherwise just remembers the row, which this then focuses.
        self.view.grab_focus();
    }

    /// The selected node.
    pub fn selected(&self) -> Option<NodeId> {
        let row = self.selection.selected_item();
        row.and_downcast_ref().and_then(node_of)
    }

    /// Opens a folder, or closes it if it is open.
    pub fn toggle(&self, folder: NodeId) {
        {
            let mut expanded = self.state.expanded.borrow_mut();
            if !expanded.remove(&folder) {
                expanded.insert(folder);
            }
        }
        self.sync_expansion();
    }

    /// Opens a folder. If its own folders are closed, it shows open once
    /// they are opened.
    pub fn expand(&self, folder: NodeId) {
        self.state.expanded.borrow_mut().insert(folder);
        self.sync_expansion();
    }

    /// Opens every folder `id` is in, so its row shows.
    pub fn reveal(&self, id: NodeId) {
        let folders = ancestors(id, |id| self.state.parent_of(id));
        self.state.expanded.borrow_mut().extend(folders);
        self.sync_expansion();
    }

    /// Opens and closes rows to match the set of open folders. Runs after
    /// every change, since a folder that moved, or whose parent opened
    /// again, gets a new row that starts closed.
    fn sync_expansion(&self) {
        // A copy, since expanding a row calls its handler, which updates
        // the set.
        let expanded = self.state.expanded.borrow().clone();
        let top = self.state.store(None);
        for position in 0..top.n_items() {
            if let Some(row) = self.tree.child_row(position) {
                sync_row(&row, &expanded);
            }
        }
    }

    /// Runs `change`, then selects again the node that was selected if the
    /// change lost it, as moving a row to another store does.
    fn keep_selection(&self, change: impl FnOnce()) {
        let selected = self.selected();
        let focused = self.has_focus();
        change();
        if let Some(id) = selected
            && self.selected() != Some(id)
            && self.state.object(id).is_some()
        {
            self.select(id, focused);
        }
    }

    /// Selects a node the user just created and scrolls to it, opening its
    /// folders. The focus moves with the selection if the sidebar has it.
    pub fn show_new(&self, id: NodeId) {
        self.select(id, self.has_focus());
    }

    /// Selects a node and scrolls to it, opening its folders. `focus` moves
    /// the keyboard focus to its row too.
    fn select(&self, id: NodeId, focus: bool) {
        self.reveal(id);
        let Some(row) = self.row_for(id) else { return };
        let mut flags = gtk::ListScrollFlags::SELECT;
        if focus {
            flags |= gtk::ListScrollFlags::FOCUS;
        }
        self.view.scroll_to(row.position(), flags, None);
    }

    /// The row showing `id`, if its folders are open.
    fn row_for(&self, id: NodeId) -> Option<gtk::TreeListRow> {
        row_for(&self.state, &self.tree, id)
    }

    fn has_focus(&self) -> bool {
        let focus = self.view.root().and_then(|root| root.focus());
        focus.is_some_and(|f| f == self.view || f.is_ancestor(&self.view))
    }
}

fn sync_row(row: &gtk::TreeListRow, expanded: &HashSet<NodeId>) {
    let Some(object) = row.item().and_downcast::<NodeObject>() else {
        return;
    };
    if !object.folder() {
        return;
    }
    let open = expanded.contains(&object.node_id());
    if row.is_expanded() != open {
        row.set_expanded(open);
    }
    if open {
        sync_children(row, expanded);
    }
}

fn sync_children(row: &gtk::TreeListRow, expanded: &HashSet<NodeId>) {
    let count = row.children().map_or(0, |c| c.n_items());
    for position in 0..count {
        if let Some(child) = row.child_row(position) {
            sync_row(&child, expanded);
        }
    }
}

fn node_of(row: &gtk::TreeListRow) -> Option<NodeId> {
    let object = row.item().and_downcast::<NodeObject>()?;
    Some(object.node_id())
}

/// The row under a point in the list view, or `None` over empty space.
fn row_at(view: &gtk::ListView, x: f64, y: f64) -> Option<gtk::TreeListRow> {
    let mut widget = view.pick(x, y, gtk::PickFlags::DEFAULT)?;
    loop {
        if let Some(expander) = widget.downcast_ref::<gtk::TreeExpander>() {
            return expander.list_row();
        }
        // The row's padding, around the expander.
        if widget.css_name() == "row" {
            let expander = widget.first_child().and_downcast::<gtk::TreeExpander>()?;
            return expander.list_row();
        }
        if widget == *view {
            return None;
        }
        widget = widget.parent()?;
    }
}

/// The row showing `id` in `tree`, if its folders are open.
fn row_for(state: &State, tree: &gtk::TreeListModel, id: NodeId) -> Option<gtk::TreeListRow> {
    let mut path = ancestors(id, |id| state.parent_of(id));
    path.reverse();
    path.push(id);
    let mut row: Option<gtk::TreeListRow> = None;
    for step in path {
        let object = state.object(step)?;
        let index = state.store(object.parent_id()).find(&object)?;
        row = Some(match &row {
            None => tree.child_row(index)?,
            Some(parent) => parent.child_row(index)?,
        });
    }
    row
}
