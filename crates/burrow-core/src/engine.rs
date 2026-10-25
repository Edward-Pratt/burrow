use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use tokio::sync::{broadcast, mpsc};

use crate::addon::{Host, Manifest, Permission};
use crate::api::{
    API_VERSION, Command, Event, Node, NodeId, POSTGRES_KIND, Problem, Response, SSH_HOST_KIND,
    SSH_TUNNEL_KIND, SecretValue, SessionId, SessionKind, View,
};
use crate::db;
use crate::error::{Error, Result};
use crate::secrets::{KeyringStore, SecretKey, SecretStore};
use crate::ssh::{self, Autostart, Connections, Input, Sessions, Transfers, files};
use crate::tree::{Entry, Tree};

/// How many events a slow subscriber can fall behind before it starts
/// missing them (it then gets `RecvError::Lagged` and should re-list).
const EVENT_BUFFER: usize = 256;

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

impl CoreOptions {
    /// The real setup: `$XDG_CONFIG_HOME/burrow` and the desktop keyring.
    pub fn system() -> Result<Self> {
        let dirs = directories::ProjectDirs::from("", "", "burrow")
            .ok_or_else(|| Error::invalid("config dir", "could not find the home directory"))?;
        let state_dir = dirs
            .state_dir()
            .unwrap_or_else(|| dirs.data_local_dir())
            .to_path_buf();
        Ok(CoreOptions {
            config_dir: dirs.config_dir().to_path_buf(),
            state_dir,
            secrets: Arc::new(KeyringStore::new()?),
            ssh_config: None,
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
    problems: Mutex<Vec<Problem>>,
    addons: Host,
    events: broadcast::Sender<Event>,
    secrets: Arc<dyn SecretStore>,
    sessions: Sessions,
    connections: Connections,
    transfers: Transfers,
    autostart: Autostart,
    /// The tunnels starting with Burrow, as last announced.
    autostart_nodes: Mutex<Vec<NodeId>>,
    history: db::History,
    ssh_config: Option<PathBuf>,
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
                problems: Mutex::new(problems),
                addons,
                events,
                secrets: options.secrets,
                sessions: Sessions::default(),
                connections: Connections::default(),
                transfers: Transfers::default(),
                autostart: Autostart::load(options.state_dir.join("autostart.toml")),
                autostart_nodes: Mutex::default(),
                history: db::History::new(options.state_dir.join("history")),
                ssh_config: options.ssh_config,
            }),
        };
        *core.inner.autostart_nodes.lock().unwrap() = core.inner.autostart_enabled();
        Ok(core)
    }

    /// Starts the tunnels the user chose to start with Burrow. The process
    /// that hosts the core calls this once, on its runtime.
    pub fn autostart(&self) {
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

    /// Runs one command. File and keyring I/O happen on a blocking thread so
    /// callers on an async runtime are never stalled. Session commands are
    /// only messages to a running session and are handled directly.
    pub async fn execute(&self, command: Command) -> Result<Response> {
        let sessions = &self.inner.sessions;
        let (session, input) = match command {
            Command::OpenTerminal { node, cols, rows } => {
                let node = self.inner.tree().get(node)?.node.clone();
                if node.kind != SSH_HOST_KIND {
                    return Err(Error::invalid(
                        "node",
                        format!("{} is a {}, not an SSH host", node.name, node.kind),
                    ));
                }
                let session = ssh::open_terminal(self, node, cols, rows);
                return Ok(Response::Session { session });
            }
            Command::StartTunnel { node } => {
                let node = self.node(node)?;
                if node.kind != SSH_TUNNEL_KIND {
                    return Err(Error::invalid(
                        "node",
                        format!("{} is a {}, not an SSH tunnel", node.name, node.kind),
                    ));
                }
                let session = match sessions.find(node.id, SessionKind::Tunnel) {
                    Some(session) => session,
                    None => ssh::open_tunnel(self, node),
                };
                return Ok(Response::Session { session });
            }
            Command::OpenFiles { node } => {
                let node = self.node(node)?;
                if node.kind != SSH_HOST_KIND {
                    return Err(Error::invalid(
                        "node",
                        format!("{} is a {}, not an SSH host", node.name, node.kind),
                    ));
                }
                let session = ssh::open_files(self, node);
                return Ok(Response::Session { session });
            }
            Command::ListDir { session, path } => {
                let sftp = sessions.sftp(session)?;
                let (path, entries) = files::list_dir(&sftp, &path).await.map_err(Error::Remote)?;
                return Ok(Response::Dir { path, entries });
            }
            Command::MakeDir { session, path } => {
                let sftp = sessions.sftp(session)?;
                files::make_dir(&sftp, &path).await.map_err(Error::Remote)?;
                return Ok(Response::Ok);
            }
            Command::RenamePath { session, from, to } => {
                let sftp = sessions.sftp(session)?;
                files::rename(&sftp, &from, &to)
                    .await
                    .map_err(Error::Remote)?;
                return Ok(Response::Ok);
            }
            Command::RemovePath { session, path } => {
                let sftp = sessions.sftp(session)?;
                files::remove(&sftp, &path).await.map_err(Error::Remote)?;
                return Ok(Response::Ok);
            }
            Command::Download {
                session,
                remote,
                local,
                overwrite,
            } => {
                let sftp = sessions.sftp(session)?;
                let direction = files::Direction::Download { remote, local };
                let transfer = files::start(self, session, sftp, direction, overwrite);
                return Ok(Response::Transfer { transfer });
            }
            Command::Upload {
                session,
                local,
                remote,
                overwrite,
            } => {
                let sftp = sessions.sftp(session)?;
                let direction = files::Direction::Upload { local, remote };
                let transfer = files::start(self, session, sftp, direction, overwrite);
                return Ok(Response::Transfer { transfer });
            }
            Command::CancelTransfer { transfer } => {
                self.inner.transfers.cancel(transfer)?;
                return Ok(Response::Ok);
            }
            Command::OpenDatabase { node } => {
                let node = self.node(node)?;
                if node.kind != POSTGRES_KIND {
                    return Err(Error::invalid(
                        "node",
                        format!("{} is a {}, not a database", node.name, node.kind),
                    ));
                }
                let session = db::open(self, node);
                return Ok(Response::Session { session });
            }
            Command::RunQuery {
                session,
                sql,
                limit,
            } => {
                let (handle, node) = sessions.database(session)?;
                let limit = limit.unwrap_or(db::DEFAULT_LIMIT).clamp(1, db::MAX_LIMIT);
                let result = handle.query(&sql, limit as usize).await;
                let inner = self.inner.clone();
                let ok = result.error.is_none();
                let recorded =
                    tokio::task::spawn_blocking(move || inner.history.record(node, &sql, ok)).await;
                if let Ok(Err(e)) = recorded {
                    tracing::warn!("could not keep the query in the history: {e}");
                }
                return Ok(Response::Query { result });
            }
            Command::CancelQuery { session } => {
                let (handle, _) = sessions.database(session)?;
                handle.cancel().await.map_err(Error::Remote)?;
                return Ok(Response::Ok);
            }
            Command::SetReadOnly { session, read_only } => {
                let (handle, _) = sessions.database(session)?;
                handle
                    .set_read_only(read_only)
                    .await
                    .map_err(Error::Remote)?;
                return Ok(Response::Ok);
            }
            Command::ListSchema { session } => {
                let (handle, _) = sessions.database(session)?;
                let schemas = handle.schema().await.map_err(Error::Remote)?;
                return Ok(Response::Schema { schemas });
            }
            Command::ExportQuery { session, sql, path } => {
                let (handle, _) = sessions.database(session)?;
                let rows = handle.export(&sql, &path).await.map_err(Error::Remote)?;
                return Ok(Response::Exported { rows });
            }
            Command::ListSessions => {
                return Ok(Response::Sessions {
                    sessions: sessions.list(),
                });
            }
            Command::AnswerPrompt {
                session,
                prompt,
                answer,
            } => {
                sessions.answer(session, prompt, answer)?;
                return Ok(Response::Ok);
            }
            Command::WriteTerminal { session, data } => (session, Input::Write(data)),
            Command::ResizeTerminal {
                session,
                cols,
                rows,
            } => (session, Input::Resize { cols, rows }),
            Command::CloseSession { session } => (session, Input::Close),
            Command::OpenView { node } => {
                let node = self.inner.tree().get(node)?.node.clone();
                let manifest = self.inner.addons.owner(&node.kind).ok_or_else(|| {
                    Error::invalid("node", format!("no addon handles {} nodes", node.kind))
                })?;
                let view = self.inner.addons.open_view(self, &manifest, node).await?;
                return Ok(Response::View { view });
            }
            Command::InvokeAction {
                addon,
                action,
                node,
                values,
            } => {
                let manifest = self.inner.addons.manifest(&addon)?;
                let node = match node {
                    Some(id) => Some(self.inner.tree().get(id)?.node.clone()),
                    None => None,
                };
                let offered = match &node {
                    // Buttons and forms in its own views can send anything.
                    Some(node) if manifest.owns(&node.kind) => true,
                    Some(node) => manifest
                        .actions
                        .iter()
                        .any(|a| a.id == action && a.kinds.contains(&node.kind)),
                    None => manifest.commands.iter().any(|c| c.id == action),
                };
                if !offered {
                    return Err(Error::invalid(
                        "action",
                        format!("addon {addon} offers no {action:?} here"),
                    ));
                }
                let message = self
                    .inner
                    .addons
                    .invoke(self, &manifest, &action, node, values)
                    .await?;
                return Ok(Response::Invoked { message });
            }
            command => {
                let inner = self.inner.clone();
                return tokio::task::spawn_blocking(move || inner.execute(command))
                    .await
                    .map_err(|e| Error::Task(e.to_string()))?;
            }
        };
        sessions.send(session, input)?;
        Ok(Response::Ok)
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

    /// Takes a session's terminal output stream: the bytes the remote shell
    /// writes, in order. It can be taken once. The session ends when the
    /// stream is dropped, so a client that is done with a terminal can just
    /// let go of it.
    pub fn take_output(&self, session: SessionId) -> Result<mpsc::Receiver<Vec<u8>>> {
        self.inner.sessions.take_output(session)
    }

    /// Runs a command an addon sent, if its permissions allow it.
    pub(crate) async fn execute_as_addon(
        &self,
        manifest: Arc<Manifest>,
        command: Command,
    ) -> Result<Response> {
        self.inner.permit(&manifest, &command)?;
        self.execute(command).await
    }

    /// An addon redrew the view of one of its own nodes.
    pub(crate) fn view_changed(&self, manifest: &Manifest, node: NodeId, view: View) -> Result<()> {
        let kind = self.inner.tree().get(node)?.node.kind.clone();
        if !manifest.owns(&kind) {
            return Err(Error::PermissionDenied(format!(
                "{node} is a {kind} node, which addon {} does not own",
                manifest.id
            )));
        }
        self.emit(Event::ViewChanged { node, view });
        Ok(())
    }

    pub(crate) fn downgrade(&self) -> WeakCore {
        WeakCore(Arc::downgrade(&self.inner))
    }

    pub(crate) fn addons(&self) -> &Host {
        &self.inner.addons
    }

    pub(crate) fn sessions(&self) -> &Sessions {
        &self.inner.sessions
    }

    pub(crate) fn node(&self, id: NodeId) -> Result<Node> {
        Ok(self.inner.tree().get(id)?.node.clone())
    }

    pub(crate) fn transfers(&self) -> &Transfers {
        &self.inner.transfers
    }

    pub(crate) fn connections(&self) -> &Connections {
        &self.inner.connections
    }

    pub(crate) fn emit(&self, event: Event) {
        self.inner.emit(event);
    }

    pub(crate) fn ssh_config(&self) -> Option<&Path> {
        self.inner.ssh_config.as_deref()
    }

    /// Re-reads the tree from disk and sends events for whatever changed.
    pub(crate) fn reload(&self) -> Result<()> {
        self.inner.reload()
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

impl Inner {
    fn tree(&self) -> MutexGuard<'_, Tree> {
        // A panic while holding the lock leaves the tree as it was before the
        // failed command's disk writes, which is still usable.
        self.tree.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn emit(&self, event: Event) {
        match &event {
            Event::SessionChanged { session, state, .. } => self.sessions.record(*session, state),
            Event::NodeDeleted { id } => self.sessions.close_node(*id),
            _ => {}
        }
        // Fails only when nobody is subscribed, which is fine.
        let _ = self.events.send(event);
    }

    fn lookup(&self) -> impl Fn(NodeId) -> Option<Node> + '_ {
        |id| self.tree().get(id).ok().map(|e| e.node.clone())
    }

    fn autostart_enabled(&self) -> Vec<NodeId> {
        self.autostart.enabled(&self.lookup())
    }

    /// Announces a change in which tunnels start with Burrow, such as an
    /// edit that withdrew a consent. Called with the tree unlocked.
    fn check_autostart(&self) {
        let nodes = self.autostart_enabled();
        let mut current = self.autostart_nodes.lock().unwrap();
        if *current != nodes {
            current.clone_from(&nodes);
            drop(current);
            self.emit(Event::AutostartChanged { nodes });
        }
    }

    fn execute(&self, command: Command) -> Result<Response> {
        let changes_tree = matches!(
            command,
            Command::CreateNode { .. }
                | Command::UpdateNode { .. }
                | Command::MoveNode { .. }
                | Command::DeleteNode { .. }
                | Command::SetAutostart { .. }
        );
        let response = self.execute_inner(command);
        if changes_tree {
            self.check_autostart();
        }
        response
    }

    fn execute_inner(&self, command: Command) -> Result<Response> {
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
                let ids: Vec<NodeId> = removed.iter().map(|e| e.node.id).collect();
                self.autostart.forget(&ids);
                self.history.forget(&ids);
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
            Command::ListAddons => Ok(Response::Addons {
                addons: self.addons.infos(),
            }),
            Command::ApproveAddon { id, fingerprint } => {
                self.addons.approve(&id, &fingerprint)?;
                self.emit(Event::AddonsChanged {
                    addons: self.addons.infos(),
                });
                Ok(Response::Ok)
            }
            Command::RevokeAddon { id } => {
                self.addons.revoke(&id)?;
                self.emit(Event::AddonsChanged {
                    addons: self.addons.infos(),
                });
                Ok(Response::Ok)
            }
            Command::ListAutostart => Ok(Response::Autostart {
                nodes: self.autostart_nodes.lock().unwrap().clone(),
            }),
            Command::SetAutostart {
                node,
                enabled,
                expect,
            } => {
                // The tunnel and its host are read under one lock, so the
                // consent covers settings that existed together. Holding
                // the lock while saving would invert the autostart lock's
                // order, so the consent is pinned to this snapshot instead:
                // a change after it withdraws the consent as usual.
                let snapshot: Vec<Node> = {
                    let tree = self.tree();
                    let node = tree.get(node)?.node.clone();
                    let via = node.props.get("via").and_then(|v| v.as_str());
                    let host = via
                        .and_then(|via| via.parse().ok())
                        .and_then(|id| tree.get(NodeId(id)).ok())
                        .map(|e| e.node.clone());
                    std::iter::once(node).chain(host).collect()
                };
                let node = &snapshot[0];
                if node.kind != SSH_TUNNEL_KIND {
                    return Err(Error::invalid(
                        "node",
                        format!("{} is a {}, not an SSH tunnel", node.name, node.kind),
                    ));
                }
                if enabled {
                    let expect = expect.ok_or_else(|| {
                        Error::invalid("expect", "enabling needs the settings the user was shown")
                    })?;
                    let host = snapshot.get(1);
                    let same = expect.props == node.props
                        && host.is_some_and(|host| {
                            expect.host_name == host.name && expect.host_props == host.props
                        });
                    if !same {
                        return Err(Error::invalid(
                            "tunnel",
                            "its settings changed since they were shown; look again",
                        ));
                    }
                }
                let lookup = |id| snapshot.iter().find(|n| n.id == id).cloned();
                self.autostart.set(&lookup, node, enabled)?;
                Ok(Response::Ok)
            }
            Command::ListProblems => Ok(Response::Problems {
                problems: self.problems.lock().unwrap().clone(),
            }),
            Command::ReloadConfig => {
                self.reload()?;
                Ok(Response::Ok)
            }
            Command::ListHistory { node } => {
                self.database_node(node)?;
                Ok(Response::History {
                    entries: self.history.list(node),
                })
            }
            Command::ClearHistory { node } => {
                self.database_node(node)?;
                self.history.forget(&[node]);
                Ok(Response::Ok)
            }
            // Handled by `Core::execute` before it gets here.
            Command::OpenTerminal { .. }
            | Command::WriteTerminal { .. }
            | Command::ResizeTerminal { .. }
            | Command::AnswerPrompt { .. }
            | Command::CloseSession { .. }
            | Command::ListSessions
            | Command::StartTunnel { .. }
            | Command::OpenFiles { .. }
            | Command::ListDir { .. }
            | Command::MakeDir { .. }
            | Command::RenamePath { .. }
            | Command::RemovePath { .. }
            | Command::Download { .. }
            | Command::Upload { .. }
            | Command::CancelTransfer { .. }
            | Command::OpenDatabase { .. }
            | Command::RunQuery { .. }
            | Command::CancelQuery { .. }
            | Command::SetReadOnly { .. }
            | Command::ListSchema { .. }
            | Command::ExportQuery { .. }
            | Command::OpenView { .. }
            | Command::InvokeAction { .. } => unreachable!("async command on the blocking path"),
        }
    }

    /// The permission table for commands sent by addons. Anything not
    /// listed, such as secrets, sessions and managing addons, is refused.
    fn permit(&self, manifest: &Manifest, command: &Command) -> Result<()> {
        let need = |permission: Permission| {
            if manifest.has(permission) {
                Ok(())
            } else {
                Err(Error::PermissionDenied(format!(
                    "addon {} lacks {permission}",
                    manifest.id
                )))
            }
        };
        let own = |kind: &str| {
            if manifest.owns(kind) {
                Ok(())
            } else {
                Err(Error::PermissionDenied(format!(
                    "addon {} may only change its own kinds of node, not {kind}",
                    manifest.id
                )))
            }
        };
        match command {
            Command::Version => Ok(()),
            Command::ListNodes | Command::GetNode { .. } => need(Permission::NodesRead),
            Command::CreateNode { kind, .. } => {
                need(Permission::NodesWrite)?;
                own(kind)
            }
            Command::UpdateNode { id, .. }
            | Command::MoveNode { id, .. }
            | Command::DeleteNode { id, .. } => {
                need(Permission::NodesWrite)?;
                own(&self.tree().get(*id)?.node.kind)
            }
            _ => Err(Error::PermissionDenied(format!(
                "addons may not send {}",
                command_name(command)
            ))),
        }
    }

    /// Checks that `id` is a database node.
    fn database_node(&self, id: NodeId) -> Result<()> {
        let tree = self.tree();
        let node = &tree.get(id)?.node;
        if node.kind != POSTGRES_KIND {
            return Err(Error::invalid(
                "node",
                format!("{} is a {}, not a database", node.name, node.kind),
            ));
        }
        Ok(())
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
        let (new, tree_problems) = Tree::load(tree.dir())?;
        let events = diff(&tree, &new);
        *tree = new;
        drop(tree);

        for event in events {
            self.emit(event);
        }
        self.check_autostart();
        if self.addons.rescan() {
            self.emit(Event::AddonsChanged {
                addons: self.addons.infos(),
            });
        }
        *self.tree_problems.lock().unwrap() = tree_problems.clone();
        let problems = [tree_problems, self.addons.problems()].concat();
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

/// The command's `type`, as it appears in JSON.
fn command_name(command: &Command) -> String {
    serde_json::to_value(command)
        .ok()
        .and_then(|v| v.get("type")?.as_str().map(str::to_string))
        .unwrap_or_default()
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

    #[test]
    fn addons_may_not_reach_databases() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let folder = dir.path().join("config/addons/probe");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join("addon.toml"),
            "id = \"probe\"\nname = \"Probe\"\nversion = \"0.1.0\"\napi = \"1\"\n\
             permissions = [\"nodes:read\", \"nodes:write\"]\n\
             [backend]\ncommand = \"probe\"\nprotocol = \"jsonrpc-stdio\"\n",
        )
        .unwrap();
        std::fs::write(folder.join("probe"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(folder.join("probe"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        let manifest = crate::addon::load_manifest(&folder).unwrap();
        let core = Core::open(CoreOptions {
            config_dir: dir.path().join("config"),
            state_dir: dir.path().join("state"),
            secrets: Arc::new(crate::secrets::MemoryStore::new()),
            ssh_config: None,
        })
        .unwrap();
        let session = SessionId(1);
        let node = NodeId::new();
        for command in [
            Command::OpenDatabase { node },
            Command::RunQuery {
                session,
                sql: "select 1".into(),
                limit: None,
            },
            Command::CancelQuery { session },
            Command::SetReadOnly {
                session,
                read_only: false,
            },
            Command::ListSchema { session },
            Command::ExportQuery {
                session,
                sql: "select 1".into(),
                path: "/tmp/out.csv".into(),
            },
            Command::ListHistory { node },
            Command::ClearHistory { node },
        ] {
            let error = core.inner.permit(&manifest, &command).unwrap_err();
            assert!(matches!(error, Error::PermissionDenied(_)), "{error}");
        }
    }
}
