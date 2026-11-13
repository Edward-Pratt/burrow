//! The state behind every [`Core`] clone, kept consistent: the tree under
//! its lock, events going out, and the announcements (autostart,
//! monitoring, grants, problems) that follow a change to the tree, from a
//! command or from a reload of the files on disk.

use std::sync::MutexGuard;

use crate::api::{Command, Event, Node, NodeId, Response, SecretValue, SessionId};
use crate::error::Result;
use crate::secrets::SecretKey;
use crate::tree::{Entry, Tree};

use super::Inner;
use super::checks::check_kind;

impl Inner {
    pub(super) fn tree(&self) -> MutexGuard<'_, Tree> {
        // A panic while holding the lock leaves the tree as it was before the
        // failed command's disk writes, which is still usable.
        self.tree.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(super) fn emit(&self, event: Event) {
        match &event {
            Event::SessionChanged { session, state, .. } => self.sessions.record(*session, state),
            Event::NodeDeleted { id } => self.sessions.close_node(*id),
            _ => {}
        }
        // Fails only when nobody is subscribed, which is fine.
        let _ = self.events.send(event);
    }

    /// Has `session` say where it stands again, for a client that joins
    /// it. Sent straight to the subscribers: nothing changed, so there is
    /// nothing to record.
    pub(super) fn repeat(&self, session: SessionId) {
        self.sessions.repeat(session, |event| {
            let _ = self.events.send(event);
        });
    }

    pub(super) fn lookup(&self) -> impl Fn(NodeId) -> Option<Node> + '_ {
        |id| self.tree().get(id).ok().map(|e| e.node.clone())
    }

    pub(super) fn autostart_enabled(&self) -> Vec<NodeId> {
        self.autostart.enabled(&self.lookup())
    }

    /// Announces a change in which tunnels start with Burrow, such as an
    /// edit that withdrew a consent. Called with the tree unlocked.
    fn check_autostart(&self) {
        if let Some(nodes) = self.autostart_nodes.refresh(self.autostart_enabled()) {
            self.emit(Event::AutostartChanged { nodes });
        }
    }

    pub(super) fn monitoring_enabled(&self) -> Vec<NodeId> {
        self.monitoring.nodes(&self.lookup())
    }

    /// Announces a change in which hosts are monitored in the background,
    /// and stops the monitors of hosts that left. One that comes back,
    /// such as a host file put back as it was, starts with Burrow.
    pub(super) fn check_monitoring(&self) {
        let nodes = self.monitoring_enabled();
        self.sessions.stop_background(&nodes);
        if let Some(nodes) = self.monitoring_nodes.refresh(nodes) {
            self.emit(Event::MonitoringChanged { nodes });
        }
    }

    /// Announces a change in what the MCP bridge may do, such as an edit
    /// that made a grant stale. Called with the tree unlocked.
    fn check_grants(&self) {
        if let Some(grants) = self.grants_shown.refresh(self.grants.list(&self.lookup())) {
            self.emit(Event::GrantsChanged { grants });
        }
    }

    pub(super) fn execute(&self, command: Command) -> Result<Response> {
        let changes_tree = command.class().changes_tree;
        let response = self.execute_inner(command);
        if changes_tree {
            self.check_autostart();
            self.check_monitoring();
            self.check_grants();
        }
        response
    }

    /// The node `id`, which must be of `kind`.
    pub(super) fn node_of_kind(&self, id: NodeId, kind: &str) -> Result<Node> {
        let node = self.tree().get(id)?.node.clone();
        check_kind(&node, kind)?;
        Ok(node)
    }

    /// `id`'s node, then the one its `props.via` names if that exists,
    /// read under one lock so a choice pinned to both covers settings
    /// that existed together.
    pub(super) fn snapshot_with_via(&self, id: NodeId) -> Result<Vec<Node>> {
        let tree = self.tree();
        let node = tree.get(id)?.node.clone();
        let via = node
            .props
            .get("via")
            .and_then(|v| v.as_str())
            .and_then(|via| via.parse().ok())
            .and_then(|via| tree.get(NodeId(via)).ok())
            .map(|e| e.node.clone());
        Ok(std::iter::once(node).chain(via).collect())
    }

    pub(super) fn resolve_secret(&self, id: NodeId, field: &str) -> Result<Option<SecretValue>> {
        let reference = self.tree().get(id)?.secret_refs.get(field).cloned();
        match reference {
            Some(reference) => self
                .secrets
                .get(&SecretKey::for_node_ref(id, field, &reference)?),
            None => Ok(None),
        }
    }

    /// Best effort: the nodes are already gone, so a keyring failure here
    /// only leaves an unused secret behind.
    pub(super) fn forget_secrets(&self, removed: &[Entry]) {
        let refs = removed.iter().flat_map(|e| {
            e.secret_refs
                .iter()
                .map(move |(field, r)| (e.node.id, field, r))
        });
        for (id, field, reference) in refs {
            let result = SecretKey::for_node_ref(id, field, reference)
                .and_then(|key| self.secrets.delete(&key));
            if let Err(e) = result {
                tracing::warn!("could not delete secret {reference}: {e}");
            }
        }
    }

    pub(super) fn reload(&self) -> Result<()> {
        let mut tree = self.tree();
        let (new, tree_problems) = Tree::load(tree.dir())?;
        let events = diff(&tree, &new);
        *tree = new;
        drop(tree);

        for event in events {
            self.emit(event);
        }
        self.check_autostart();
        self.check_monitoring();
        self.check_grants();
        if self.addons.rescan() {
            self.emit(Event::AddonsChanged {
                addons: self.addons.infos(),
            });
        }
        *self.tree_problems.lock().unwrap() = tree_problems.clone();
        let problems = [tree_problems, self.addons.problems()].concat();
        if let Some(problems) = self.problems.refresh(problems) {
            self.emit(Event::ProblemsChanged { problems });
        }
        Ok(())
    }
}

/// Events that turn `old` into `new`: creations parents first, deletions
/// children first. Our own writes produce no difference, so the watcher
/// seeing them is harmless.
fn diff(old: &Tree, new: &Tree) -> Vec<Event> {
    let mut created: Vec<&Entry> = Vec::new();
    let mut updated = Vec::new();
    for (id, entry) in new.entries() {
        match old.entries().get(id) {
            None => created.push(entry),
            Some(before) if before.node != entry.node => updated.push(Event::NodeUpdated {
                node: entry.node.clone(),
            }),
            Some(_) => {}
        }
    }
    let mut deleted: Vec<NodeId> = old
        .entries()
        .keys()
        .filter(|id| !new.entries().contains_key(id))
        .copied()
        .collect();

    created.sort_by_key(|e| new.depth(e.node.id));
    deleted.sort_by_key(|&id| std::cmp::Reverse(old.depth(id)));

    deleted
        .into_iter()
        .map(|id| Event::NodeDeleted { id })
        .chain(created.into_iter().map(|e| Event::NodeCreated {
            node: e.node.clone(),
        }))
        .chain(updated)
        .collect()
}
