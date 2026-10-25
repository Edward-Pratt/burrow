use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use tokio::sync::{broadcast, mpsc};

use crate::addon::{Host, Manifest, Permission};
use crate::api::{
    API_VERSION, AlertKind, Capability, Command, Event, Grant, Node, NodeId, POSTGRES_KIND,
    Problem, Response, SSH_HOST_KIND, SSH_TUNNEL_KIND, SecretValue, SessionId, SessionKind, View,
};
use crate::class::AddonRule;
use crate::db;
use crate::error::{Error, Result};
use crate::grants::{self, Grants};
use crate::pins::Pins;
use crate::secrets::{KeyringStore, SecretKey, SecretStore};
use crate::ssh::alerts::{Alerts, Rule};
use crate::ssh::trend::{self, Trends};
use crate::ssh::{
    self, Agents, Autostart, Connections, Input, Sessions, Transfers, Waits, files, hosts,
};
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

    /// Runs one command. File and keyring I/O happen on a blocking thread so
    /// callers on an async runtime are never stalled. Session commands are
    /// only messages to a running session and are handled directly.
    pub async fn execute(&self, command: Command) -> Result<Response> {
        let sessions = &self.inner.sessions;
        let (session, input) = match command {
            Command::OpenTerminal { node, cols, rows } => {
                let node = self.node_of_kind(node, SSH_HOST_KIND)?;
                let session = ssh::open_terminal(self, node, cols, rows);
                return Ok(Response::Session { session });
            }
            Command::StartTunnel { node } => {
                let node = self.node_of_kind(node, SSH_TUNNEL_KIND)?;
                let session = match sessions.find(node.id, SessionKind::Tunnel) {
                    Some(session) => session,
                    None => ssh::open_tunnel(self, node),
                };
                return Ok(Response::Session { session });
            }
            Command::Exec {
                node,
                command,
                timeout,
            } => {
                let node = self.node_of_kind(node, SSH_HOST_KIND)?;
                let result = ssh::exec::start(self, node, command, timeout)?
                    .await
                    .map_err(|_| Error::Remote("the command was stopped".to_string()))?
                    .map_err(Error::Remote)?;
                return Ok(Response::Exec { result });
            }
            Command::OpenFiles { node } => {
                let node = self.node_of_kind(node, SSH_HOST_KIND)?;
                let session = ssh::open_files(self, node);
                return Ok(Response::Session { session });
            }
            Command::OpenMonitor { node } => {
                let session = self.open_monitor(node, true)?;
                return Ok(Response::Session { session });
            }
            Command::SetMonitoring {
                node,
                enabled,
                expect,
            } => {
                let command = Command::SetMonitoring {
                    node,
                    enabled,
                    expect,
                };
                let response = self.blocking(command).await?;
                if enabled {
                    self.start_monitoring(node);
                }
                return Ok(response);
            }
            Command::MonitorHistory { session } => {
                let history = sessions.monitor(session)?.history();
                return Ok(Response::Monitor { history });
            }
            Command::ListDir { session, path } => {
                let sftp = sessions.sftp(session)?;
                let (path, entries) = files::list_dir(&sftp, &path).await.map_err(Error::Remote)?;
                return Ok(Response::Dir { path, entries });
            }
            Command::ReadFile {
                session,
                path,
                limit,
            } => {
                let sftp = sessions.sftp(session)?;
                let limit = limit.unwrap_or(1 << 20);
                let (data, size) = files::read_file(&sftp, &path, limit)
                    .await
                    .map_err(Error::Remote)?;
                return Ok(Response::File { data, size });
            }
            Command::WriteFile {
                session,
                path,
                data,
                overwrite,
            } => {
                let sftp = sessions.sftp(session)?;
                files::write_file(&sftp, &path, &data, overwrite)
                    .await
                    .map_err(Error::Remote)?;
                return Ok(Response::Ok);
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
                let node = self.node_of_kind(node, POSTGRES_KIND)?;
                let session = db::open(self, node);
                return Ok(Response::Session { session });
            }
            Command::RunQuery {
                session,
                sql,
                limit,
            } => return self.run_query(session, sql, limit, false).await,
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
            Command::CheckSshInclude { alias } => {
                let status = hosts::include_status(self.ssh_config(), &alias).await?;
                return Ok(Response::SshInclude {
                    included: status.included,
                    config: status.config,
                    line: status.line,
                });
            }
            Command::AddSshHost {
                parent,
                alias,
                hostname,
                user,
                port,
                identity_file,
                add_include,
            } => {
                let host = hosts::NewHost {
                    alias,
                    hostname,
                    user,
                    port,
                    identity_file,
                };
                return self.add_ssh_host(parent, host, add_include).await;
            }
            command => return self.blocking(command).await,
        };
        sessions.send(session, input)?;
        Ok(Response::Ok)
    }

    async fn blocking(&self, command: Command) -> Result<Response> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || inner.execute(command))
            .await
            .map_err(|e| Error::Task(e.to_string()))?
    }

    /// Starts a monitor on an SSH host node. Without `install` it runs only
    /// an agent the user already let Burrow put there, which is how the MCP
    /// bridge opens one.
    pub(crate) fn open_monitor(&self, node: NodeId, install: bool) -> Result<SessionId> {
        let node = self.node_of_kind(node, SSH_HOST_KIND)?;
        if let Some(session) = self.inner.sessions.background(node.id) {
            return Ok(session);
        }
        Ok(ssh::open_monitor(self, node, install))
    }

    /// Starts `node`'s background monitor unless it has one.
    ///
    /// The host is read again here, after the choice was checked, so it
    /// may have changed in between, as with a `git pull`. It only starts
    /// as the host it was chosen for, and the check after it stops it if a
    /// change landed while it started, which that change's own check
    /// would have missed.
    fn start_monitoring(&self, node: NodeId) {
        let lookup = self.inner.lookup();
        let Some(node) = lookup(node) else { return };
        if !self.inner.monitoring.has(&lookup, &node) {
            return;
        }
        ssh::open_background_monitor(self, node);
        self.inner.check_monitoring();
    }

    /// For tests: read burrow-agent builds from `local`, and put the agent
    /// in `remote` on servers instead of `~/.local/share/burrow`.
    #[doc(hidden)]
    pub fn set_agent_dirs(&self, local: PathBuf, remote: String) {
        self.inner.agents.set_dirs(local, remote);
    }

    /// For tests: how long background monitors wait before they reconnect,
    /// and before they report a host unreachable.
    #[doc(hidden)]
    pub fn set_monitor_waits(&self, retry: Duration, unreachable: Duration) {
        *self.inner.monitor_waits.lock().unwrap() = Waits {
            retry,
            retry_max: retry,
            unreachable,
        };
    }

    pub(crate) fn monitor_waits(&self) -> Waits {
        *self.inner.monitor_waits.lock().unwrap()
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

    /// Runs SQL on a database session and keeps it in the node's history.
    /// With `read_only`, the SQL can't write whatever the session allows;
    /// see [`db::Handle::query_read_only`].
    pub(crate) async fn run_query(
        &self,
        session: SessionId,
        sql: String,
        limit: Option<u32>,
        read_only: bool,
    ) -> Result<Response> {
        let (handle, node) = self.inner.sessions.database(session)?;
        let limit = limit.unwrap_or(db::DEFAULT_LIMIT).clamp(1, db::MAX_LIMIT) as usize;
        let result = if read_only {
            handle.query_read_only(&sql, limit).await
        } else {
            handle.query(&sql, limit).await
        };
        let inner = self.inner.clone();
        let ok = result.error.is_none();
        let recorded =
            tokio::task::spawn_blocking(move || inner.history.record(node, &sql, ok)).await;
        if let Ok(Err(e)) = recorded {
            tracing::warn!("could not keep the query in the history: {e}");
        }
        Ok(Response::Query { result })
    }

    /// Appends one line to the MCP bridge's audit log. A failure is only
    /// logged: the command already ran or was refused.
    pub(crate) fn audit(&self, entry: &serde_json::Value) {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        /// Past this size the log moves to `mcp-audit.log.1`, replacing
        /// the one there, so the two never pass twice this.
        const MAX: u64 = 8 << 20;
        // Requests run on tasks of their own. Two that both found the log
        // full would both move it, and the second would put a log of one
        // line in place of the old one.
        static WRITING: Mutex<()> = Mutex::new(());
        let _writing = WRITING.lock().unwrap_or_else(|e| e.into_inner());
        let path = &self.inner.audit_log;
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if std::fs::metadata(path).is_ok_and(|meta| meta.len() >= MAX) {
            let old = path.with_extension("log.1");
            if let Err(e) = std::fs::rename(path, &old) {
                tracing::warn!("could not move {} aside: {e}", path.display());
            }
        }
        let written = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(path)
            .and_then(|mut file| writeln!(file, "{entry}"));
        if let Err(e) = written {
            tracing::warn!("could not write {}: {e}", path.display());
        }
    }

    /// Whether the MCP bridge may do `capability` on `node`.
    pub(crate) fn granted(&self, node: NodeId, capability: Capability) -> bool {
        self.inner
            .grants
            .allows(&self.inner.lookup(), node, capability)
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

    /// The node `id`, which must be of `kind`.
    pub(crate) fn node_of_kind(&self, id: NodeId, kind: &str) -> Result<Node> {
        self.inner.node_of_kind(id, kind)
    }

    pub(crate) fn transfers(&self) -> &Transfers {
        &self.inner.transfers
    }

    pub(crate) fn agents(&self) -> &Agents {
        &self.inner.agents
    }

    pub(crate) fn trends(&self) -> &Trends {
        &self.inner.trends
    }

    pub(crate) fn alerts(&self) -> &Alerts {
        &self.inner.alerts
    }

    /// Sets what `rule` finds on `node` now, announcing what changed.
    pub(crate) fn set_alerts(&self, node: NodeId, rule: Rule, found: Vec<AlertKind>) {
        for event in self.inner.alerts.set(node, rule, found, trend::now()) {
            self.emit(event);
        }
    }

    /// Clears `node`'s alerts, as when its monitor stops.
    pub(crate) fn clear_alerts(&self, node: NodeId) {
        for event in self.inner.alerts.clear(node) {
            self.emit(event);
        }
    }

    pub(crate) fn connections(&self) -> &Connections {
        &self.inner.connections
    }

    pub(crate) fn emit(&self, event: Event) {
        self.inner.emit(event);
    }

    /// Writes the host to the OpenSSH config, then makes its node. The
    /// block is taken back if the node can't be made, so a failure leaves
    /// both as they were.
    async fn add_ssh_host(
        &self,
        parent: Option<NodeId>,
        host: hosts::NewHost,
        add_include: bool,
    ) -> Result<Response> {
        let _adding = self.inner.adding_host.lock().await;
        // Checked first too, so the common mistake writes nothing.
        if let Some(parent) = parent
            && !self.node(parent)?.is_folder()
        {
            return Err(Error::NotAFolder(parent));
        }
        let added = hosts::add(self.ssh_config(), &host, add_include).await?;
        let mut props = toml::Table::new();
        props.insert("alias".into(), host.alias.clone().into());
        let command = Command::CreateNode {
            parent,
            kind: SSH_HOST_KIND.into(),
            name: host.alias,
            color: None,
            props,
        };
        let inner = self.inner.clone();
        let result = tokio::task::spawn_blocking(move || inner.execute(command))
            .await
            .map_err(|e| Error::Task(e.to_string()))
            .flatten();
        if result.is_err() {
            added.undo();
        }
        result
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
        if let Some(nodes) = self.autostart_nodes.refresh(self.autostart_enabled()) {
            self.emit(Event::AutostartChanged { nodes });
        }
    }

    fn monitoring_enabled(&self) -> Vec<NodeId> {
        self.monitoring.nodes(&self.lookup())
    }

    /// Announces a change in which hosts are monitored in the background,
    /// and stops the monitors of hosts that left. One that comes back,
    /// such as a host file put back as it was, starts with Burrow.
    fn check_monitoring(&self) {
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

    fn execute(&self, command: Command) -> Result<Response> {
        let changes_tree = command.class().changes_tree;
        let response = self.execute_inner(command);
        if changes_tree {
            self.check_autostart();
            self.check_monitoring();
            self.check_grants();
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
                self.agents.forget(&ids);
                self.monitoring.forget(&ids);
                self.trends.forget(&ids);
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
                nodes: self.autostart_nodes.get(),
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
                let snapshot = self.snapshot_with_via(node)?;
                let node = &snapshot[0];
                check_kind(node, SSH_TUNNEL_KIND)?;
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
            Command::MonitorTrend { node, range } => {
                let node = self.node_of_kind(node, SSH_HOST_KIND)?;
                Ok(Response::Trend {
                    points: self.trends.points(node.id, range, trend::now()),
                })
            }
            Command::ListAlerts => Ok(Response::Alerts {
                alerts: self.alerts.list(),
            }),
            Command::ListMonitoring => Ok(Response::Monitoring {
                nodes: self.monitoring_nodes.get(),
            }),
            Command::SetMonitoring {
                node,
                enabled,
                expect,
            } => {
                // Read under one lock, as for grants, so the choice covers
                // a host and a `via` that existed together.
                let snapshot = self.snapshot_with_via(node)?;
                let node = &snapshot[0];
                check_kind(node, SSH_HOST_KIND)?;
                let lookup = |id| snapshot.iter().find(|n| n.id == id).cloned();
                if enabled {
                    let expect = expect.ok_or_else(|| {
                        Error::invalid("expect", "turning it on needs the host the user was shown")
                    })?;
                    if !grants::matches(&lookup, node, &expect) {
                        return Err(Error::invalid(
                            "expect",
                            format!("{} changed after it was shown; show it again", node.name),
                        ));
                    }
                }
                self.monitoring.set(&lookup, node, enabled)?;
                Ok(Response::Ok)
            }
            Command::ListGrants => Ok(Response::Grants {
                grants: self.grants_shown.get(),
            }),
            Command::SetGrant {
                node,
                capabilities,
                expect,
            } => {
                // Read under one lock, as for autostart, so the grant
                // covers a node and a `via` that existed together.
                let snapshot = self.snapshot_with_via(node)?;
                let lookup = |id| snapshot.iter().find(|n| n.id == id).cloned();
                self.grants
                    .set(&lookup, &snapshot[0], capabilities, expect.as_ref())?;
                Ok(Response::Ok)
            }
            Command::ListProblems => Ok(Response::Problems {
                problems: self.problems.get(),
            }),
            Command::ReloadConfig => {
                self.reload()?;
                Ok(Response::Ok)
            }
            Command::ListHistory { node } => {
                self.node_of_kind(node, POSTGRES_KIND)?;
                Ok(Response::History {
                    entries: self.history.list(node),
                })
            }
            Command::ClearHistory { node } => {
                self.node_of_kind(node, POSTGRES_KIND)?;
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
            | Command::Exec { .. }
            | Command::ListDir { .. }
            | Command::ReadFile { .. }
            | Command::WriteFile { .. }
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
            | Command::OpenMonitor { .. }
            | Command::MonitorHistory { .. }
            | Command::InvokeAction { .. }
            | Command::CheckSshInclude { .. }
            | Command::AddSshHost { .. } => unreachable!("async command on the blocking path"),
        }
    }

    /// Whether an addon may send `command`, by what its class says it
    /// needs (see [`AddonRule`]) and the addon's permissions and kinds.
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
        match command.class().addon {
            AddonRule::Free => Ok(()),
            AddonRule::ReadNodes => need(Permission::NodesRead),
            AddonRule::CreateNode(kind) => {
                need(Permission::NodesWrite)?;
                own(kind)
            }
            AddonRule::ChangeNode(id) => {
                need(Permission::NodesWrite)?;
                own(&self.tree().get(id)?.node.kind)
            }
            AddonRule::Denied => Err(Error::PermissionDenied(format!(
                "addons may not send {}",
                command_name(command)
            ))),
        }
    }

    /// The node `id`, which must be of `kind`.
    fn node_of_kind(&self, id: NodeId, kind: &str) -> Result<Node> {
        let node = self.tree().get(id)?.node.clone();
        check_kind(&node, kind)?;
        Ok(node)
    }

    /// `id`'s node, then the one its `props.via` names if that exists,
    /// read under one lock so a choice pinned to both covers settings
    /// that existed together.
    fn snapshot_with_via(&self, id: NodeId) -> Result<Vec<Node>> {
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

/// Fails unless `node` is of `kind`, saying what it should have been.
fn check_kind(node: &Node, kind: &str) -> Result<()> {
    if node.kind == kind {
        return Ok(());
    }
    let wanted = match kind {
        SSH_HOST_KIND => "an SSH host",
        SSH_TUNNEL_KIND => "an SSH tunnel",
        POSTGRES_KIND => "a database",
        _ => unreachable!("no command needs a {kind} node"),
    };
    Err(Error::invalid(
        "node",
        format!("{} is a {}, not {wanted}", node.name, node.kind),
    ))
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

    /// A core with one addon that may read and write nodes.
    fn core_with_addon() -> (tempfile::TempDir, Core, Manifest) {
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
        (dir, core, manifest)
    }

    #[test]
    fn addons_may_not_reach_databases() {
        let (_dir, core, manifest) = core_with_addon();
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

    #[test]
    fn addons_may_not_add_ssh_hosts() {
        let (_dir, core, manifest) = core_with_addon();
        for command in [
            Command::CheckSshInclude {
                alias: "web".into(),
            },
            Command::AddSshHost {
                parent: None,
                alias: "web".into(),
                hostname: "10.0.0.5".into(),
                user: None,
                port: None,
                identity_file: None,
                add_include: true,
            },
        ] {
            let error = core.inner.permit(&manifest, &command).unwrap_err();
            assert!(matches!(error, Error::PermissionDenied(_)), "{error}");
        }
    }
}
