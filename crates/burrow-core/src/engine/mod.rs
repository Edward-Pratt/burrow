//! The core behind every client: the tree, secrets, sessions, monitors,
//! grants and addons behind one cheap-to-clone handle, [`Core`], whose
//! clones share `Inner`. This file holds the handle, its options and
//! paths, opening it, and the small types the rest of the module shares.
//! The work is in the submodules: `execute` routes each command, `blocking`
//! runs the ones that touch disk and keyring, `internal` is what the
//! core's own SSH, database and addon code reaches on it, `inner` keeps
//! the shared state consistent, `permit` checks what an addon may send and
//! `checks` validates kinds and field names.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};

use tokio::sync::broadcast;

use crate::addon::Host;
use crate::api::{Event, Grant, NodeId, Problem, SessionKind};
use crate::db;
use crate::error::{Error, Result};
use crate::grants::Grants;
use crate::pins::Pins;
use crate::secrets::{KeyringStore, SecretStore};
use crate::ssh::alerts::Alerts;
use crate::ssh::trend::Trends;
use crate::ssh::{self, Agents, Autostart, Connections, Sessions, Transfers, Waits};
use crate::tree::Tree;

mod blocking;
mod checks;
mod execute;
mod inner;
mod internal;
mod permit;

/// How many events a slow subscriber can fall behind before it starts
/// missing them (it then gets `RecvError::Lagged` and should re-list).
const EVENT_BUFFER: usize = 256;

/// How long a service action may take, in seconds. A unit stopping can
/// wait out its own timeout, which is 90 seconds by default.
const CONTROL_TIMEOUT: u32 = 120;

pub struct CoreOptions {
    /// Burrow's config directory. The tree lives in its `tree/` subfolder
    /// and addons in `addons/`.
    pub config_dir: PathBuf,
    /// Where runtime state goes, such as which addons are approved. Kept
    /// apart from the config directory, which may be shared.
    pub state_dir: PathBuf,
    pub secrets: Arc<dyn SecretStore>,
    /// Used instead of `~/.ssh/config`, as `ssh -F` does. `None` reads the
    /// user's own.
    pub ssh_config: Option<PathBuf>,
}

/// Where Burrow keeps things, without the secret store: what clients need
/// to know, since only the daemon opens the keyring.
pub struct Paths {
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
    pub ssh_config: Option<PathBuf>,
}

impl Paths {
    /// `$XDG_CONFIG_HOME/burrow` and `$XDG_STATE_HOME/burrow`, changed by
    /// the environment: `BURROW_CONFIG_DIR` points Burrow at another config
    /// folder, `BURROW_STATE_DIR` at another state folder, and
    /// `BURROW_SSH_CONFIG` at another OpenSSH config (as `ssh -F`), which
    /// keeps development runs away from your real ones.
    pub fn from_env() -> Result<Self> {
        let dirs = directories::ProjectDirs::from("", "", "burrow")
            .ok_or_else(|| Error::invalid("config dir", "could not find the home directory"))?;
        let var = |name| std::env::var_os(name).map(PathBuf::from);
        Ok(Paths {
            config_dir: var("BURROW_CONFIG_DIR").unwrap_or_else(|| dirs.config_dir().into()),
            state_dir: var("BURROW_STATE_DIR")
                .unwrap_or_else(|| dirs.state_dir().unwrap_or(dirs.data_local_dir()).into()),
            ssh_config: var("BURROW_SSH_CONFIG"),
        })
    }
}

impl CoreOptions {
    /// [`Paths::from_env`] and the desktop keyring.
    pub fn from_env() -> Result<Self> {
        let paths = Paths::from_env()?;
        Ok(CoreOptions {
            config_dir: paths.config_dir,
            state_dir: paths.state_dir,
            secrets: Arc::new(KeyringStore::new()?),
            ssh_config: paths.ssh_config,
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
    /// The tree's problems; the addon host keeps its own.
    tree_problems: Mutex<Vec<Problem>>,
    /// Both together, as last announced.
    problems: Announced<Vec<Problem>>,
    addons: Host,
    events: broadcast::Sender<Event>,
    secrets: Arc<dyn SecretStore>,
    sessions: Sessions,
    connections: Connections,
    transfers: Transfers,
    autostart: Autostart,
    /// Which hosts burrow-agent may be put on.
    agents: Agents,
    /// The tunnels starting with Burrow, as last announced.
    autostart_nodes: Announced<Vec<NodeId>>,
    /// Which hosts are monitored in the background.
    monitoring: Pins,
    /// Those hosts, as last announced.
    monitoring_nodes: Announced<Vec<NodeId>>,
    /// Their history on disk.
    trends: Trends,
    /// What their monitors found wrong.
    alerts: Alerts,
    /// How long their monitors wait, which tests shorten.
    monitor_waits: Mutex<Waits>,
    grants: Grants,
    /// Where each command the MCP bridge sends is recorded.
    audit_log: PathBuf,
    /// The grants, as last announced.
    grants_shown: Announced<Vec<Grant>>,
    history: db::History,
    ssh_config: Option<PathBuf>,
    /// Held while a host is added, so two never interleave their checks
    /// and writes to the OpenSSH config.
    adding_host: tokio::sync::Mutex<()>,
}

impl Core {
    pub fn open(options: CoreOptions) -> Result<Core> {
        let (tree, tree_problems) = Tree::load(&options.config_dir.join("tree"))?;
        let addons_dir = options.config_dir.join("addons");
        std::fs::create_dir_all(&addons_dir).map_err(|e| Error::io(&addons_dir, e))?;
        let addons = Host::new(
            addons_dir,
            options.state_dir.join("addons.toml"),
            options.state_dir.join("addons"),
        );
        addons.rescan();
        let problems = [tree_problems.clone(), addons.problems()].concat();
        let (events, _) = broadcast::channel(EVENT_BUFFER);
        let core = Core {
            inner: Arc::new(Inner {
                tree: Mutex::new(tree),
                tree_problems: Mutex::new(tree_problems),
                problems: Announced::new(problems),
                addons,
                events,
                secrets: options.secrets,
                sessions: Sessions::default(),
                connections: Connections::default(),
                transfers: Transfers::default(),
                autostart: Autostart::load(options.state_dir.join("autostart.toml")),
                agents: Agents::load(options.state_dir.join("agent.toml")),
                autostart_nodes: Announced::new(Vec::new()),
                monitoring: Pins::load(options.state_dir.join("monitoring.toml")),
                monitoring_nodes: Announced::new(Vec::new()),
                trends: Trends::new(options.state_dir.join("monitor")),
                alerts: Alerts::default(),
                monitor_waits: Mutex::default(),
                grants: Grants::load(options.state_dir.join("mcp.toml")),
                audit_log: options.state_dir.join("mcp-audit.log"),
                grants_shown: Announced::new(Vec::new()),
                history: db::History::new(options.state_dir.join("history")),
                ssh_config: options.ssh_config,
                adding_host: tokio::sync::Mutex::default(),
            }),
        };
        // Nobody has subscribed yet, so there is no one to tell.
        let inner = &core.inner;
        inner.autostart_nodes.refresh(inner.autostart_enabled());
        inner.monitoring_nodes.refresh(inner.monitoring_enabled());
        inner
            .grants_shown
            .refresh(inner.grants.list(&inner.lookup()));
        Ok(core)
    }

    /// Starts the tunnels the user chose to start with Burrow, and the
    /// hosts' background monitors. The process that hosts the core calls
    /// this once, on its runtime.
    pub fn autostart(&self) {
        for node in self.inner.monitoring_enabled() {
            self.start_monitoring(node);
        }
        for node in self.inner.autostart_enabled() {
            if let Ok(node) = self.node(node)
                && self
                    .inner
                    .sessions
                    .find(node.id, SessionKind::Tunnel)
                    .is_none()
            {
                ssh::open_tunnel(self, node);
            }
        }
    }

    /// Receives every [`Event`] sent after this call.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.inner.events.subscribe()
    }

    /// The directory holding the node files.
    pub fn tree_dir(&self) -> PathBuf {
        self.inner.tree().dir().to_path_buf()
    }

    /// The directory holding addon folders.
    pub fn addons_dir(&self) -> PathBuf {
        self.inner.addons.dir().to_path_buf()
    }
}

/// A [`Core`] that does not keep it alive, for tasks it owns.
#[derive(Clone)]
pub(crate) struct WeakCore(Weak<Inner>);

impl WeakCore {
    pub fn upgrade(&self) -> Option<Core> {
        self.0.upgrade().map(|inner| Core { inner })
    }
}

/// A value as clients were last told it, so they hear of it again only
/// when it changes.
struct Announced<T>(Mutex<T>);

impl<T: Clone + PartialEq> Announced<T> {
    fn new(value: T) -> Self {
        Announced(Mutex::new(value))
    }

    fn get(&self) -> T {
        self.0.lock().unwrap().clone()
    }

    /// Records `new`, and hands it back to announce if it differs from
    /// what was announced last.
    fn refresh(&self, new: T) -> Option<T> {
        let mut current = self.0.lock().unwrap();
        if *current == new {
            return None;
        }
        current.clone_from(&new);
        Some(new)
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
