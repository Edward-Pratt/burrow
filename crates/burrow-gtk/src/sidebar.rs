//! The sidebar: the core's flat node list shown as a tree.
//!
//! Each folder's children, and the top level's, live in a sorted
//! `gio::ListStore` of [`NodeObject`]s, and a `gtk::TreeListModel` over the
//! top level's store shows them as a tree. Changes update the objects in
//! place and rows redraw from them, so the selection, the focus and the
//! scroll position survive them.

use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use burrow_core::{Node, NodeId};
use relm4::gtk::prelude::*;
use relm4::gtk::subclass::prelude::*;
use relm4::gtk::{gdk, gio, glib, graphene};
use relm4::{ComponentSender, gtk};

use crate::app::{App, AppMsg};
use crate::dialogs;

/// The children of `parent`: folders first, then by name.
pub fn children(nodes: &HashMap<NodeId, Node>, parent: Option<NodeId>) -> Vec<&Node> {
    let mut children: Vec<&Node> = nodes.values().filter(|n| n.parent == parent).collect();
    children.sort_by_key(|n| sort_key(n.is_folder(), &n.name, n.id));
    children
}

/// Where a node goes among its siblings. The stores and [`children`] both
/// sort by it, so they always agree.
fn sort_key(folder: bool, name: &str, id: NodeId) -> (bool, String, NodeId) {
    (!folder, name.to_lowercase(), id)
}

/// "Work / Servers" for a node inside those folders; empty at the top level.
pub fn location(nodes: &HashMap<NodeId, Node>, node: &Node) -> String {
    let mut parts = Vec::new();
    let mut current = node.parent;
    while let Some(id) = current {
        let Some(parent) = nodes.get(&id) else { break };
        parts.push(parent.name.as_str());
        current = parent.parent;
    }
    parts.reverse();
    parts.join(" / ")
}

/// The folders `id` sits in, nearest first. `parent` looks a node's parent
/// up. It stops at a repeat, which the core never sends, rather than loop.
fn ancestors(id: NodeId, parent: impl Fn(NodeId) -> Option<NodeId>) -> Vec<NodeId> {
    let mut found = Vec::new();
    let mut current = parent(id);
    while let Some(folder) = current {
        if folder == id || found.contains(&folder) {
            break;
        }
        found.push(folder);
        current = parent(folder);
    }
    found
}

/// Whether node `id` can move into `to` (`None` is the top level): not
/// where it already is, and for a folder, not into itself or a folder
/// inside it.
fn can_move(id: NodeId, to: Option<NodeId>, parent: impl Fn(NodeId) -> Option<NodeId>) -> bool {
    if parent(id) == to {
        return false;
    }
    match to {
        None => true,
        Some(folder) => folder != id && !ancestors(folder, parent).contains(&id),
    }
}

/// Where "Move to…" offers to put node `id`: "Top level" first, then every
/// folder by its path, leaving out the node itself and folders inside it.
pub fn move_choices(nodes: &HashMap<NodeId, Node>, id: NodeId) -> Vec<(Option<NodeId>, String)> {
    let parent = |n: NodeId| nodes.get(&n).and_then(|n| n.parent);
    let mut folders: Vec<(Option<NodeId>, String)> = nodes
        .values()
        .filter(|n| n.is_folder() && n.id != id && !ancestors(n.id, parent).contains(&id))
        .map(|n| {
            let location = location(nodes, n);
            let path = if location.is_empty() {
                n.name.clone()
            } else {
                format!("{location} / {}", n.name)
            };
            (Some(n.id), path)
        })
        .collect();
    folders.sort_by_cached_key(|(id, path)| (path.to_lowercase(), *id));
    folders.insert(0, (None, "Top level".to_string()));
    folders
}

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

/// `icons` maps addon kinds to the icons their addons chose.
pub fn icon_name<'a>(node: &Node, expanded: bool, icons: &'a HashMap<String, String>) -> &'a str {
    kind_icon(&node.kind, expanded, icons)
}

fn kind_icon<'a>(kind: &str, expanded: bool, icons: &'a HashMap<String, String>) -> &'a str {
    match kind {
        "folder" if expanded => "folder-open-symbolic",
        "folder" => "folder-symbolic",
        "ssh-host" => "network-server-symbolic",
        "ssh-tunnel" => "preferences-system-network-proxy-symbolic",
        "postgres" => "drive-harddisk-symbolic",
        kind => icons
            .get(kind)
            .map_or("text-x-generic-symbolic", String::as_str),
    }
}

/// Whether `color` is `#rgb` or `#rrggbb`, as the core requires. Node files
/// can be written by hand and the core only checks colors set through it,
/// so this is checked again before a color goes into markup.
fn is_color(color: &str) -> bool {
    color
        .strip_prefix('#')
        .is_some_and(|hex| matches!(hex.len(), 3 | 6) && hex.chars().all(|c| c.is_ascii_hexdigit()))
}

mod imp {
    use std::cell::{Cell, RefCell};

    use burrow_core::NodeId;
    use relm4::gtk::glib;
    use relm4::gtk::glib::prelude::*;
    use relm4::gtk::glib::subclass::prelude::*;

    #[derive(Default, glib::Properties)]
    #[properties(wrapper_type = super::NodeObject)]
    pub struct NodeObject {
        #[property(get, construct_only)]
        id: RefCell<String>,
        #[property(get, set)]
        name: RefCell<String>,
        #[property(get, set)]
        kind: RefCell<String>,
        #[property(get, set)]
        color: RefCell<Option<String>>,
        #[property(get, set)]
        running: Cell<bool>,
        #[property(get, set)]
        folder: Cell<bool>,
        /// The icon for its kind, closed for a folder.
        #[property(get, set)]
        icon: RefCell<String>,
        /// The ids as ids, since burrow-gtk has no way to parse `id` back.
        pub(super) node: Cell<Option<NodeId>>,
        pub(super) parent: Cell<Option<NodeId>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for NodeObject {
        const NAME: &'static str = "BurrowNodeObject";
        type Type = super::NodeObject;
    }

    #[glib::derived_properties]
    impl ObjectImpl for NodeObject {}
}

glib::wrapper! {
    /// A node as the sidebar's stores hold it. Rows draw from its
    /// properties and redraw when they change.
    pub struct NodeObject(ObjectSubclass<imp::NodeObject>);
}

impl NodeObject {
    fn new(node: &Node, icon: &str, running: bool) -> Self {
        let object: Self = glib::Object::builder()
            .property("id", node.id.to_string())
            .build();
        object.imp().node.set(Some(node.id));
        object.update(node, icon);
        object.set_running(running);
        object
    }

    pub fn node_id(&self) -> NodeId {
        self.imp().node.get().expect("set when the object is made")
    }

    pub fn parent_id(&self) -> Option<NodeId> {
        self.imp().parent.get()
    }

    /// Takes the node's fields, notifying only the ones that changed so
    /// rows aren't redrawn for nothing.
    fn update(&self, node: &Node, icon: &str) {
        self.imp().parent.set(node.parent);
        if self.name() != node.name {
            self.set_name(node.name.as_str());
        }
        if self.kind() != node.kind {
            self.set_kind(node.kind.as_str());
        }
        if self.color() != node.color {
            // The generated setter can't clear it.
            self.set_property("color", node.color.as_deref());
        }
        if self.folder() != node.is_folder() {
            self.set_folder(node.is_folder());
        }
        self.set_icon_if_changed(icon);
    }

    fn set_icon_if_changed(&self, icon: &str) {
        if self.icon() != icon {
            self.set_icon(icon);
        }
    }

    fn sort_key(&self) -> (bool, String, NodeId) {
        sort_key(self.folder(), &self.name(), self.node_id())
    }
}

fn compare(a: &glib::Object, b: &glib::Object) -> Ordering {
    match (
        a.downcast_ref::<NodeObject>(),
        b.downcast_ref::<NodeObject>(),
    ) {
        (Some(a), Some(b)) => a.sort_key().cmp(&b.sort_key()),
        _ => Ordering::Equal,
    }
}

/// Whether the item at `position` sorts after the one before it and
/// before the one after it.
fn in_order(store: &gio::ListStore, position: u32) -> bool {
    let Some(item) = store.item(position) else {
        return true;
    };
    let before = position
        .checked_sub(1)
        .and_then(|p| store.item(p))
        .is_none_or(|b| compare(&b, &item) == Ordering::Less);
    let after = (store.item(position + 1)).is_none_or(|a| compare(&item, &a) == Ordering::Less);
    before && after
}

/// What the sidebar's closures share. Each field is its own `RefCell`, and
/// none is borrowed while a store changes or a row expands: those call back
/// into the tree's create function and the rows' handlers, which borrow
/// them too.
#[derive(Default)]
struct State {
    /// Each folder's children, and the top level's under `None`.
    stores: RefCell<HashMap<Option<NodeId>, gio::ListStore>>,
    objects: RefCell<HashMap<NodeId, NodeObject>>,
    /// Folders shown open. Kept here rather than read off the rows, since
    /// a folder's row is thrown away when its parent closes, and it should
    /// open again with its parent.
    expanded: RefCell<HashSet<NodeId>>,
    /// Nodes marked as running, such as connected tunnels.
    running: RefCell<HashSet<NodeId>>,
    /// The node being dragged, so a drop spot can be refused while the
    /// pointer is still moving.
    dragged: Cell<Option<NodeId>>,
}

impl State {
    fn store(&self, folder: Option<NodeId>) -> gio::ListStore {
        self.stores
            .borrow_mut()
            .entry(folder)
            .or_insert_with(gio::ListStore::new::<NodeObject>)
            .clone()
    }

    fn object(&self, id: NodeId) -> Option<NodeObject> {
        self.objects.borrow().get(&id).cloned()
    }

    fn parent_of(&self, id: NodeId) -> Option<NodeId> {
        self.object(id)?.parent_id()
    }
}

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
    fn add_keys(&self, sender: &ComponentSender<App>) {
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

    /// Lets rows be dropped on a folder row to move into it, on a leaf to
    /// move beside it, and below the rows to move to the top level.
    fn add_drop(&self, sender: &ComponentSender<App>) {
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

    /// Makes the stores hold exactly `nodes`. Objects are kept for nodes
    /// that are still there and stores are only refilled when their
    /// children changed, so open folders and the selection stay as they
    /// were.
    pub fn replace(&self, nodes: &HashMap<NodeId, Node>, icons: &HashMap<String, String>) {
        self.keep_selection(|| {
            let running = self.state.running.borrow().clone();
            self.state
                .objects
                .borrow_mut()
                .retain(|id, _| nodes.contains_key(id));
            for node in nodes.values() {
                let icon = icon_name(node, false, icons);
                match self.state.object(node.id) {
                    Some(object) => object.update(node, icon),
                    None => {
                        let object = NodeObject::new(node, icon, running.contains(&node.id));
                        self.state.objects.borrow_mut().insert(node.id, object);
                    }
                }
            }

            let folders = nodes.values().filter(|n| n.is_folder()).map(|n| Some(n.id));
            for folder in std::iter::once(None).chain(folders) {
                let wanted: Vec<NodeObject> = children(nodes, folder)
                    .iter()
                    .filter_map(|n| self.state.object(n.id))
                    .collect();
                let store = self.state.store(folder);
                if !holds(&store, &wanted) {
                    store.splice(0, store.n_items(), &wanted);
                }
            }
            let is_folder = |id: &NodeId| nodes.get(id).is_some_and(Node::is_folder);
            self.state
                .stores
                .borrow_mut()
                .retain(|folder, _| folder.as_ref().is_none_or(is_folder));
            self.state.expanded.borrow_mut().retain(is_folder);
        });
        self.sync_expansion();
        self.placeholder.set_visible(nodes.is_empty());
    }

    /// Adds a node the core created.
    pub fn insert(&self, node: &Node, icons: &HashMap<String, String>) {
        if self.state.object(node.id).is_some() {
            return self.update(node, icons);
        }
        let running = self.state.running.borrow().contains(&node.id);
        let object = NodeObject::new(node, icon_name(node, false, icons), running);
        self.state
            .objects
            .borrow_mut()
            .insert(node.id, object.clone());
        self.state
            .store(node.parent)
            .insert_sorted(&object, compare);
    }

    /// Takes a node's new fields. A node that was renamed moves to keep
    /// its folder sorted, and one that was moved goes to its new folder.
    pub fn update(&self, node: &Node, icons: &HashMap<String, String>) {
        let Some(object) = self.state.object(node.id) else {
            return self.insert(node, icons);
        };
        self.keep_selection(|| {
            let (parent, folder) = (object.parent_id(), object.folder());
            object.update(node, icon_name(node, false, icons));
            let old = self.state.store(parent);
            let Some(position) = old.find(&object) else {
                return;
            };
            // The tree only asks whether a row has children when it adds
            // it, so a node that became or stopped being a folder goes back
            // in too.
            if parent != node.parent || folder != node.is_folder() || !in_order(&old, position) {
                old.remove(position);
                self.state
                    .store(node.parent)
                    .insert_sorted(&object, compare);
            }
        });
        self.sync_expansion();
    }

    /// Takes away a node the core deleted. The core deletes a folder's
    /// contents first.
    pub fn remove(&self, id: NodeId) {
        let Some(object) = self.state.objects.borrow_mut().remove(&id) else {
            return;
        };
        let store = self.state.store(object.parent_id());
        if let Some(position) = store.find(&object) {
            store.remove(position);
        }
        self.state.stores.borrow_mut().remove(&Some(id));
        self.state.expanded.borrow_mut().remove(&id);
    }

    /// Takes the icons addons chose for their kinds.
    pub fn set_icons(&self, icons: &HashMap<String, String>) {
        let objects: Vec<NodeObject> = self.state.objects.borrow().values().cloned().collect();
        for object in objects {
            object.set_icon_if_changed(kind_icon(&object.kind(), false, icons));
        }
    }

    /// Marks exactly these nodes as running.
    pub fn set_running(&self, running: HashSet<NodeId>) {
        let changed: Vec<NodeId> = (self.state.running.borrow())
            .symmetric_difference(&running)
            .copied()
            .collect();
        for id in changed {
            if let Some(object) = self.state.object(id) {
                object.set_running(running.contains(&id));
            }
        }
        self.state.running.replace(running);
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

/// Whether `store` holds exactly `objects`, in order.
fn holds(store: &gio::ListStore, objects: &[NodeObject]) -> bool {
    store.n_items() as usize == objects.len()
        && (0..store.n_items())
            .zip(objects)
            .all(|(i, object)| store.item(i).as_ref() == Some(object.upcast_ref()))
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

/// The context menu for the selected row, opened from the keyboard. It
/// points just below the row, or at the top of the list when no row is
/// selected.
fn menu_for_selection(
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

/// Where a drop at a point in the list puts a node, and the row to light
/// up for it: a folder row takes it in, a leaf's folder takes it beside
/// the leaf, and the space below the rows is the top level. The top level
/// lights up the whole list, as does a leaf there.
fn drop_spot(view: &gtk::ListView, x: f64, y: f64) -> (Option<gtk::TreeListRow>, Option<NodeId>) {
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
fn light(lit: &RefCell<Option<gtk::Widget>>, spot: Option<gtk::Widget>) {
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
fn row_widget(view: &gtk::ListView, row: &gtk::TreeListRow) -> Option<gtk::Widget> {
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

fn to_pane(widget: &impl IsA<gtk::Widget>, pane: &gtk::Box, x: f64, y: f64) -> (f64, f64) {
    let point = graphene::Point::new(x as f32, y as f32);
    let point = widget.compute_point(pane, &point).unwrap_or(point);
    (point.x().into(), point.y().into())
}

/// The widgets of one row.
#[derive(Clone)]
struct Row {
    expander: gtk::TreeExpander,
    icon: gtk::Image,
    name: gtk::Label,
    dot: gtk::Label,
    running: gtk::Label,
}

/// A row's hold on what it shows, let go when it shows something else.
struct Bound {
    object: NodeObject,
    object_handler: glib::SignalHandlerId,
    row: gtk::TreeListRow,
    row_handler: glib::SignalHandlerId,
}

impl Bound {
    fn release(self) {
        self.object.disconnect(self.object_handler);
        self.row.disconnect(self.row_handler);
    }
}

fn factory(state: &Rc<State>) -> gtk::SignalListItemFactory {
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
    fn new() -> Self {
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
    fn add_drag(&self, state: &Rc<State>) {
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

    fn bind(&self, state: &Rc<State>, row: gtk::TreeListRow, object: NodeObject) -> Bound {
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

    fn draw(&self, object: &NodeObject, expanded: bool) {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str, kind: &str, parent: Option<NodeId>) -> Node {
        Node {
            id: NodeId::new(),
            parent,
            kind: kind.into(),
            name: name.into(),
            color: None,
            props: Default::default(),
            secrets: Default::default(),
        }
    }

    fn map(nodes: impl IntoIterator<Item = Node>) -> HashMap<NodeId, Node> {
        nodes.into_iter().map(|n| (n.id, n)).collect()
    }

    #[test]
    fn folders_come_first_then_names() {
        let nodes = map([
            node("b-host", "ssh-host", None),
            node("Zeta", "folder", None),
            node("a-host", "ssh-host", None),
            node("alpha", "folder", None),
        ]);
        let names: Vec<&str> = children(&nodes, None)
            .iter()
            .map(|n| n.name.as_str())
            .collect();
        assert_eq!(names, ["alpha", "Zeta", "a-host", "b-host"]);
    }

    #[test]
    fn stores_sort_as_children_do() {
        let nodes = map([
            node("b-host", "ssh-host", None),
            node("Zeta", "folder", None),
            node("a-host", "ssh-host", None),
            node("alpha", "folder", None),
            node("Alpha", "ssh-host", None),
        ]);
        let store = gio::ListStore::new::<NodeObject>();
        for node in nodes.values() {
            store.insert_sorted(&NodeObject::new(node, "", false), compare);
        }
        let stored: Vec<NodeId> = (0..store.n_items())
            .filter_map(|i| store.item(i).and_downcast::<NodeObject>())
            .map(|o| o.node_id())
            .collect();
        let expected: Vec<NodeId> = children(&nodes, None).iter().map(|n| n.id).collect();
        assert_eq!(stored, expected);
        assert!((0..store.n_items()).all(|i| in_order(&store, i)));

        // A rename that changes the order is noticed.
        let first = store.item(0).and_downcast::<NodeObject>().unwrap();
        first.set_name("zzz");
        assert!(!in_order(&store, 0));
    }

    #[test]
    fn location_lists_parent_folders() {
        let work = node("Work", "folder", None);
        let servers = node("Servers", "folder", Some(work.id));
        let host = node("web", "ssh-host", Some(servers.id));
        let nodes = map([work, servers, host.clone()]);
        assert_eq!(location(&nodes, &host), "Work / Servers");
    }

    #[test]
    fn ancestors_nearest_first_and_stop_at_a_loop() {
        let work = node("Work", "folder", None);
        let servers = node("Servers", "folder", Some(work.id));
        let host = node("web", "ssh-host", Some(servers.id));
        let nodes = map([work.clone(), servers.clone(), host.clone()]);
        let parent = |id| nodes.get(&id).and_then(|n: &Node| n.parent);
        assert_eq!(ancestors(host.id, parent), [servers.id, work.id]);
        assert!(ancestors(work.id, parent).is_empty());

        let (a, b) = (NodeId::new(), NodeId::new());
        let looped = |id| Some(if id == a { b } else { a });
        assert_eq!(ancestors(a, looped), [b]);
    }

    #[test]
    fn moves_into_itself_or_nowhere_are_refused() {
        let work = node("Work", "folder", None);
        let servers = node("Servers", "folder", Some(work.id));
        let host = node("web", "ssh-host", Some(servers.id));
        let other = node("Other", "folder", None);
        let nodes = map([work.clone(), servers.clone(), host.clone(), other.clone()]);
        let parent = |id| nodes.get(&id).and_then(|n: &Node| n.parent);

        assert!(can_move(host.id, None, parent));
        assert!(can_move(host.id, Some(work.id), parent));
        assert!(can_move(work.id, Some(other.id), parent));
        // Where it is already.
        assert!(!can_move(host.id, Some(servers.id), parent));
        assert!(!can_move(work.id, None, parent));
        // Into itself or a folder inside it.
        assert!(!can_move(work.id, Some(work.id), parent));
        assert!(!can_move(work.id, Some(servers.id), parent));
    }

    #[test]
    fn move_choices_leave_out_the_node_and_its_folders() {
        let work = node("Work", "folder", None);
        let servers = node("Servers", "folder", Some(work.id));
        let host = node("web", "ssh-host", Some(servers.id));
        let archive = node("archive", "folder", None);
        let nodes = map([work.clone(), servers.clone(), host.clone(), archive.clone()]);

        let labels = |id| -> Vec<String> {
            move_choices(&nodes, id)
                .into_iter()
                .map(|(_, label)| label)
                .collect()
        };
        assert_eq!(
            labels(host.id),
            ["Top level", "archive", "Work", "Work / Servers"]
        );
        assert_eq!(labels(work.id), ["Top level", "archive"]);
        assert_eq!(move_choices(&nodes, host.id)[0].0, None);
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
