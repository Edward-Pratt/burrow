//! The node tree as data: children, locations, ancestors, and where a node may move.

use std::collections::HashMap;

use burrow_core::{Node, NodeId};

/// The children of `parent`: folders first, then by name.
pub fn children(nodes: &HashMap<NodeId, Node>, parent: Option<NodeId>) -> Vec<&Node> {
    let mut children: Vec<&Node> = nodes.values().filter(|n| n.parent == parent).collect();
    children.sort_by_key(|n| sort_key(n.is_folder(), &n.name, n.id));
    children
}

/// Where a node goes among its siblings. The stores and [`children`] both
/// sort by it, so they always agree.
pub(super) fn sort_key(folder: bool, name: &str, id: NodeId) -> (bool, String, NodeId) {
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
pub(super) fn ancestors(id: NodeId, parent: impl Fn(NodeId) -> Option<NodeId>) -> Vec<NodeId> {
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
pub(super) fn can_move(
    id: NodeId,
    to: Option<NodeId>,
    parent: impl Fn(NodeId) -> Option<NodeId>,
) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;
    use relm4::gtk::gio;
    use relm4::gtk::prelude::*;

    use super::super::model::{NodeObject, compare, in_order};

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
}
