//! Turns the core's flat node list into the rows of the sidebar.

use std::collections::{HashMap, HashSet};

use burrow_core::{Node, NodeId};
use relm4::gtk;
use relm4::gtk::prelude::*;

/// Indent per folder level, in pixels.
const INDENT: i32 = 14;

/// The children of `parent`: folders first, then by name.
pub fn children(nodes: &HashMap<NodeId, Node>, parent: Option<NodeId>) -> Vec<&Node> {
    let mut children: Vec<&Node> = nodes.values().filter(|n| n.parent == parent).collect();
    children.sort_by_key(|n| (!n.is_folder(), n.name.to_lowercase(), n.id));
    children
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

pub fn icon_name(node: &Node, expanded: bool) -> &'static str {
    match node.kind.as_str() {
        "folder" if expanded => "folder-open-symbolic",
        "folder" => "folder-symbolic",
        "ssh-host" => "network-server-symbolic",
        _ => "text-x-generic-symbolic",
    }
}

/// Clears `list` and fills it with the visible rows. Returns the node id of
/// each row, in order, so row indexes can be mapped back to nodes.
pub fn rebuild(
    list: &gtk::ListBox,
    nodes: &HashMap<NodeId, Node>,
    expanded: &HashSet<NodeId>,
) -> Vec<NodeId> {
    list.remove_all();
    let mut ids = Vec::new();
    add_rows(list, nodes, expanded, None, 0, &mut ids);
    ids
}

fn add_rows(
    list: &gtk::ListBox,
    nodes: &HashMap<NodeId, Node>,
    expanded: &HashSet<NodeId>,
    parent: Option<NodeId>,
    depth: i32,
    ids: &mut Vec<NodeId>,
) {
    for node in children(nodes, parent) {
        let is_open = expanded.contains(&node.id);
        list.append(&row(node, depth, is_open));
        ids.push(node.id);
        if node.is_folder() && is_open {
            add_rows(list, nodes, expanded, Some(node.id), depth + 1, ids);
        }
    }
}

fn row(node: &Node, depth: i32, expanded: bool) -> gtk::ListBoxRow {
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    content.set_margin_start(depth * INDENT);

    // Folders get a disclosure arrow; leaves get a spacer of the same width
    // so names line up.
    let arrow = gtk::Image::from_icon_name(if expanded {
        "pan-down-symbolic"
    } else {
        "pan-end-symbolic"
    });
    arrow.set_opacity(if node.is_folder() { 0.7 } else { 0.0 });
    content.append(&arrow);
    content.append(&gtk::Image::from_icon_name(icon_name(node, expanded)));

    let name = gtk::Label::new(Some(&node.name));
    name.set_xalign(0.0);
    name.set_hexpand(true);
    name.set_ellipsize(gtk::pango::EllipsizeMode::End);
    content.append(&name);

    if let Some(color) = &node.color {
        // Colors are validated by the core as #rgb or #rrggbb, so they are
        // safe to put in markup.
        let dot = gtk::Label::new(None);
        dot.set_markup(&format!("<span foreground=\"{color}\">●</span>"));
        content.append(&dot);
    }

    let row = gtk::ListBoxRow::new();
    row.set_child(Some(&content));
    row.set_tooltip_text(Some(&node.name));
    row
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

    #[test]
    fn folders_come_first_then_names() {
        let mut nodes = HashMap::new();
        for n in [
            node("b-host", "ssh-host", None),
            node("Zeta", "folder", None),
            node("a-host", "ssh-host", None),
            node("alpha", "folder", None),
        ] {
            nodes.insert(n.id, n);
        }
        let names: Vec<&str> = children(&nodes, None)
            .iter()
            .map(|n| n.name.as_str())
            .collect();
        assert_eq!(names, ["alpha", "Zeta", "a-host", "b-host"]);
    }

    #[test]
    fn location_lists_parent_folders() {
        let work = node("Work", "folder", None);
        let servers = node("Servers", "folder", Some(work.id));
        let host = node("web", "ssh-host", Some(servers.id));
        let nodes: HashMap<_, _> = [work, servers, host.clone()]
            .into_iter()
            .map(|n| (n.id, n))
            .collect();
        assert_eq!(location(&nodes, &host), "Work / Servers");
    }
}
