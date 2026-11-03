//! What the core's own code reaches on [`Core`] that clients never do: the
//! SSH, database, monitor, addon and IPC modules take sessions, secrets,
//! alerts, grants and the audit log through these, in process.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;

use crate::addon::{Host, Manifest};
use crate::api::{
    AlertKind, Capability, Command, Event, Node, NodeId, Response, SSH_HOST_KIND, SecretValue,
    SessionId, View,
};
use crate::db;
use crate::error::{Error, Result};
use crate::ssh::alerts::{Alerts, Rule};
use crate::ssh::trend::{self, Trends};
use crate::ssh::{self, Agents, Connections, Sessions, Transfers, Waits};

use super::{Core, WeakCore};

impl Core {
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
    pub(super) fn start_monitoring(&self, node: NodeId) {
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
        use std::time::Instant;
        /// Past this size the log moves to `mcp-audit.log.1`, and the older
        /// ones along to at most `.5`. A bridge flooding the log to push
        /// out what it did earlier has to write five times this.
        const MAX: u64 = 8 << 20;
        const KEPT: u32 = 5;
        /// Filling the log twice within this is unusual enough to say so.
        const BURST: Duration = Duration::from_secs(600);
        // Requests run on tasks of their own. Two that both found the log
        // full would both move it, and the second would push a log of one
        // line along in place of a full one. Holds when the log last moved.
        static WRITING: Mutex<Option<Instant>> = Mutex::new(None);
        let mut moved = WRITING.lock().unwrap_or_else(|e| e.into_inner());
        let path = &self.inner.audit_log;
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if std::fs::metadata(path).is_ok_and(|meta| meta.len() >= MAX) {
            if moved.is_some_and(|at| at.elapsed() < BURST) {
                tracing::warn!(
                    "the MCP audit log filled up twice within {} minutes; its oldest \
                     lines are being dropped",
                    BURST.as_secs() / 60
                );
            }
            *moved = Some(Instant::now());
            let old = |n: u32| path.with_extension(format!("log.{n}"));
            for n in (1..KEPT).rev() {
                // Most are missing until the log has filled that often.
                let _ = std::fs::rename(old(n), old(n + 1));
            }
            if let Err(e) = std::fs::rename(path, old(1)) {
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

    pub(crate) fn ssh_config(&self) -> Option<&Path> {
        self.inner.ssh_config.as_deref()
    }

    /// Re-reads the tree from disk and sends events for whatever changed.
    pub(crate) fn reload(&self) -> Result<()> {
        self.inner.reload()
    }
}
