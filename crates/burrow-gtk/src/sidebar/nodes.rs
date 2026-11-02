//! Taking the core's nodes: replacing, inserting, updating and removing them in the stores.

use std::collections::{HashMap, HashSet};

use burrow_core::{Node, NodeId};
use relm4::gtk::gio;
use relm4::gtk::prelude::*;

use super::Sidebar;
use super::model::NodeObject;
use super::model::{compare, icon_name, in_order, kind_icon};
use super::tree::children;

impl Sidebar {
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
}

/// Whether `store` holds exactly `objects`, in order.
pub(super) fn holds(store: &gio::ListStore, objects: &[NodeObject]) -> bool {
    store.n_items() as usize == objects.len()
        && (0..store.n_items())
            .zip(objects)
            .all(|(i, object)| store.item(i).as_ref() == Some(object.upcast_ref()))
}
