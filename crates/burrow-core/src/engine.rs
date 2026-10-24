use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::broadcast;

use crate::api::{API_VERSION, Command, Event, NodeId, Problem, Response, SecretValue};
use crate::error::{Error, Result};
use crate::secrets::{KeyringStore, SecretKey, SecretStore};
use crate::tree::{Entry, Tree};

/// How many events a slow subscriber can fall behind before it starts
/// missing them (it then gets `RecvError::Lagged` and should re-list).
const EVENT_BUFFER: usize = 256;

pub struct CoreOptions {
    /// Burrow's config directory. The tree lives in its `tree/` subfolder.
    pub config_dir: PathBuf,
    pub secrets: Arc<dyn SecretStore>,
}

impl CoreOptions {
    /// The real setup: `$XDG_CONFIG_HOME/burrow` and the desktop keyring.
    pub fn system() -> Result<Self> {
        let dirs = directories::ProjectDirs::from("", "", "burrow")
            .ok_or_else(|| Error::invalid("config dir", "could not find the home directory"))?;
        Ok(CoreOptions {
            config_dir: dirs.config_dir().to_path_buf(),
            secrets: Arc::new(KeyringStore::new()?),
        })
    }
}

/// The handle every client uses. Cheap to clone; all clones share state.
#[derive(Clone)]
pub struct Core {
    inner: Arc<Inner>,
}

struct Inner {
    tree: Mutex<Tree>,
    problems: Mutex<Vec<Problem>>,
    events: broadcast::Sender<Event>,
    secrets: Arc<dyn SecretStore>,
}

impl Core {
    pub fn open(options: CoreOptions) -> Result<Core> {
        let (tree, problems) = Tree::load(&options.config_dir.join("tree"))?;
        let (events, _) = broadcast::channel(EVENT_BUFFER);
        Ok(Core {
            inner: Arc::new(Inner {
                tree: Mutex::new(tree),
                problems: Mutex::new(problems),
                events,
                secrets: options.secrets,
            }),
        })
    }

    /// Receives every [`Event`] sent after this call.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.inner.events.subscribe()
    }

    /// The directory holding the node files.
    pub fn tree_dir(&self) -> PathBuf {
        self.inner.tree().dir().to_path_buf()
    }

    /// Runs one command. File and keyring I/O happen on a blocking thread so
    /// callers on an async runtime are never stalled.
    pub async fn execute(&self, command: Command) -> Result<Response> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || inner.execute(command))
            .await
            .map_err(|e| Error::Task(e.to_string()))?
    }

    /// Looks up a node's secret.
    ///
    /// This is deliberately not a [`Command`]: secrets never cross the API
    /// boundary to addons or other processes. Only trusted in-process code,
    /// such as the core's own SSH and database connectors, calls this.
    pub async fn resolve_secret(&self, id: NodeId, field: &str) -> Result<Option<SecretValue>> {
        let inner = self.inner.clone();
        let field = field.to_string();
        tokio::task::spawn_blocking(move || inner.resolve_secret(id, &field))
            .await
            .map_err(|e| Error::Task(e.to_string()))?
    }

    /// Re-reads the tree from disk and sends events for whatever changed.
    pub(crate) fn reload(&self) -> Result<()> {
        self.inner.reload()
    }
}

impl Inner {
    fn tree(&self) -> MutexGuard<'_, Tree> {
        // A panic while holding the lock leaves the tree as it was before the
        // failed command's disk writes, which is still usable.
        self.tree.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn emit(&self, event: Event) {
        // Fails only when nobody is subscribed, which is fine.
        let _ = self.events.send(event);
    }

    fn execute(&self, command: Command) -> Result<Response> {
        match command {
            Command::Version => Ok(Response::Version { api: API_VERSION }),
            Command::ListNodes => Ok(Response::Nodes {
                nodes: self.tree().nodes(),
            }),
            Command::GetNode { id } => Ok(Response::Node {
                node: self.tree().get(id)?.node.clone(),
            }),
            Command::CreateNode {
                parent,
                kind,
                name,
                color,
                props,
            } => {
                let node = self.tree().create(parent, kind, name, color, props)?;
                self.emit(Event::NodeCreated { node: node.clone() });
                Ok(Response::Node { node })
            }
            Command::UpdateNode { id, patch } => {
                let mut tree = self.tree();
                let before = tree.get(id)?.node.clone();
                let node = tree.update(id, patch)?;
                if node != before {
                    self.emit(Event::NodeUpdated { node: node.clone() });
                }
                Ok(Response::Node { node })
            }
            Command::MoveNode { id, parent } => {
                let mut tree = self.tree();
                let before = tree.get(id)?.node.parent;
                let node = tree.move_node(id, parent)?;
                if node.parent != before {
                    self.emit(Event::NodeUpdated { node: node.clone() });
                }
                Ok(Response::Node { node })
            }
            Command::DeleteNode { id, recursive } => {
                let removed = self.tree().delete(id, recursive)?;
                for entry in &removed {
                    self.emit(Event::NodeDeleted { id: entry.node.id });
                }
                self.forget_secrets(&removed);
                Ok(Response::Ok)
            }
            Command::SetSecret { id, field, value } => {
                validate_field(&field)?;
                let key = match self.tree().get(id)?.secret_refs.get(&field) {
                    Some(reference) => SecretKey::for_node_ref(id, &field, reference)?,
                    None => SecretKey::for_field(id, &field),
                };
                // Store the value first, without holding the tree lock across
                // a D-Bus round trip, then record the reference.
                self.secrets.set(&key, &value)?;
                match self.tree().set_secret_ref(id, &field, Some(key.to_ref())) {
                    Ok(node) => {
                        self.emit(Event::NodeUpdated { node: node.clone() });
                        Ok(Response::Node { node })
                    }
                    Err(e) => {
                        // The node went away meanwhile; don't leave an orphan.
                        let _ = self.secrets.delete(&key);
                        Err(e)
                    }
                }
            }
            Command::ClearSecret { id, field } => {
                let (node, reference) = {
                    let mut tree = self.tree();
                    let reference = tree.get(id)?.secret_refs.get(&field).cloned();
                    (tree.set_secret_ref(id, &field, None)?, reference)
                };
                if let Some(reference) = reference {
                    // A reference that isn't this node's own is dropped from
                    // the file but never followed into the store.
                    if let Ok(key) = SecretKey::for_node_ref(id, &field, &reference) {
                        self.secrets.delete(&key)?;
                    }
                    self.emit(Event::NodeUpdated { node: node.clone() });
                }
                Ok(Response::Node { node })
            }
            Command::ListProblems => Ok(Response::Problems {
                problems: self.problems.lock().unwrap().clone(),
            }),
            Command::ReloadConfig => {
                self.reload()?;
                Ok(Response::Ok)
            }
        }
    }

    fn resolve_secret(&self, id: NodeId, field: &str) -> Result<Option<SecretValue>> {
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
    fn forget_secrets(&self, removed: &[Entry]) {
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

    fn reload(&self) -> Result<()> {
        let mut tree = self.tree();
        let (new, problems) = Tree::load(tree.dir())?;
        let events = diff(&tree, &new);
        *tree = new;
        drop(tree);

        for event in events {
            self.emit(event);
        }
        let mut current = self.problems.lock().unwrap();
        if *current != problems {
            current.clone_from(&problems);
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

fn validate_field(field: &str) -> Result<()> {
    let ok = !field.is_empty()
        && field
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if ok {
        Ok(())
    } else {
        Err(Error::invalid(
            "secret field",
            format!("{field:?} must be lowercase letters, digits, '_' or '-'"),
        ))
    }
}

impl std::fmt::Debug for Core {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Core")
            .field("tree_dir", &self.tree_dir())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_is_send_and_sync() {
        fn check<T: Send + Sync + 'static>() {}
        check::<Core>();
    }
}
