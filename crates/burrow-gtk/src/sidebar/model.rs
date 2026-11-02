//! The stores behind the tree: a NodeObject per node, sorted as children are, and the state the sidebar keeps.

use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use burrow_core::{Node, NodeId};
use relm4::gtk::prelude::*;
use relm4::gtk::subclass::prelude::*;
use relm4::gtk::{gio, glib};

use super::tree::sort_key;

/// `icons` maps addon kinds to the icons their addons chose.
pub fn icon_name<'a>(node: &Node, expanded: bool, icons: &'a HashMap<String, String>) -> &'a str {
    kind_icon(&node.kind, expanded, icons)
}

pub(super) fn kind_icon<'a>(
    kind: &str,
    expanded: bool,
    icons: &'a HashMap<String, String>,
) -> &'a str {
    match kind {
        "folder" if expanded => "folder-open-symbolic",
        "folder" => "folder-symbolic",
        "ssh-host" => "network-server-symbolic",
        "ssh-tunnel" => "preferences-system-network-proxy-symbolic",
        "postgres" => "drive-harddisk-symbolic",
        "vnc" => "video-display-symbolic",
        kind => icons
            .get(kind)
            .map_or("text-x-generic-symbolic", String::as_str),
    }
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
    pub(super) fn new(node: &Node, icon: &str, running: bool) -> Self {
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
    pub(super) fn update(&self, node: &Node, icon: &str) {
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

    pub(super) fn set_icon_if_changed(&self, icon: &str) {
        if self.icon() != icon {
            self.set_icon(icon);
        }
    }

    pub(super) fn sort_key(&self) -> (bool, String, NodeId) {
        sort_key(self.folder(), &self.name(), self.node_id())
    }
}

pub(super) fn compare(a: &glib::Object, b: &glib::Object) -> Ordering {
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
pub(super) fn in_order(store: &gio::ListStore, position: u32) -> bool {
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
pub(super) struct State {
    /// Each folder's children, and the top level's under `None`.
    pub(super) stores: RefCell<HashMap<Option<NodeId>, gio::ListStore>>,
    pub(super) objects: RefCell<HashMap<NodeId, NodeObject>>,
    /// Folders shown open. Kept here rather than read off the rows, since
    /// a folder's row is thrown away when its parent closes, and it should
    /// open again with its parent.
    pub(super) expanded: RefCell<HashSet<NodeId>>,
    /// Nodes marked as running, such as connected tunnels.
    pub(super) running: RefCell<HashSet<NodeId>>,
    /// The node being dragged, so a drop spot can be refused while the
    /// pointer is still moving.
    pub(super) dragged: Cell<Option<NodeId>>,
}

impl State {
    pub(super) fn store(&self, folder: Option<NodeId>) -> gio::ListStore {
        self.stores
            .borrow_mut()
            .entry(folder)
            .or_insert_with(gio::ListStore::new::<NodeObject>)
            .clone()
    }

    pub(super) fn object(&self, id: NodeId) -> Option<NodeObject> {
        self.objects.borrow().get(&id).cloned()
    }

    pub(super) fn parent_of(&self, id: NodeId) -> Option<NodeId> {
        self.object(id)?.parent_id()
    }
}
