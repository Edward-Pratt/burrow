//! The daemon's side of the socket.
//!
//! Each connection has a reader, which dispatches requests on tasks of
//! their own, and a writer, the only task touching the socket's write half.
//! The writer takes replies and events before terminal output, so a busy
//! terminal cannot hold up the rest. Output waits instead, which slows the
//! SSH channel rather than losing bytes.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use data_encoding::BASE64;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{mpsc, watch};
use tokio::task::AbortHandle;
use zeroize::Zeroize;

use super::{
    BUILD, INVALID_PARAMS, Initialize, MAX_LINE, METHOD_NOT_FOUND, Role, SERVER_ERROR, ServerInfo,
    read_line,
};
use crate::api::{API_VERSION, Capability, Command, Event, NodeId, Response, SessionId};
use crate::class::McpRule;
use crate::engine::Core;
use crate::error::{Error, Result};

/// Replies and events queued for a client before senders wait.
const OUTBOX: usize = 64;

/// Terminal output queued for a client, across all its sessions.
const OUTPUT_QUEUE: usize = 16;

/// How often an idle daemon checks whether it can stop.
const IDLE_CHECK: Duration = Duration::from_secs(5);

/// Connections so far, which numbers each one.
static CONNECTIONS: AtomicU64 = AtomicU64::new(0);

/// The longest string the audit log keeps of a command or an error, so
/// a few lines can't fill the log and push out what came before.
const AUDIT_FIELD: usize = 4 << 10;

/// Serves clients on `listener`. With `idle`, it returns once no client
/// has been connected and no session running for that long; otherwise it
/// runs until the task is dropped.
pub async fn serve(core: Core, listener: UnixListener, idle: Option<Duration>) {
    let clients = Arc::new(AtomicUsize::new(0));
    let mut idle_since = Instant::now();
    let mut check = tokio::time::interval(IDLE_CHECK);
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let guard = Connected::new(&clients);
                    tokio::spawn(connection(core.clone(), stream, guard));
                }
                // Out of file descriptors, most likely. Waiting lets some close.
                Err(e) => {
                    tracing::warn!("could not accept a client: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
            _ = check.tick(), if idle.is_some() => {
                let busy = clients.load(Ordering::SeqCst) > 0 || !core.sessions().list().is_empty();
                if busy {
                    idle_since = Instant::now();
                } else if idle.is_some_and(|idle| idle_since.elapsed() >= idle) {
                    return;
                }
            }
        }
    }
}

/// Counts a client as connected while it is alive.
struct Connected(Arc<AtomicUsize>);

impl Connected {
    fn new(clients: &Arc<AtomicUsize>) -> Self {
        clients.fetch_add(1, Ordering::SeqCst);
        Connected(clients.clone())
    }
}

impl Drop for Connected {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The parts of a connection its tasks share.
struct Conn {
    core: Core,
    /// Tells this client's lines in the audit log from another bridge's.
    id: u64,
    role: OnceLock<Role>,
    control: mpsc::Sender<String>,
    output: mpsc::Sender<String>,
    /// The output forwarders, by session. Aborting one drops the core's
    /// stream, which ends the session.
    forwarders: Mutex<HashMap<SessionId, AbortHandle>>,
    /// Sessions this client opened, closed when it goes. Tunnels are left
    /// out: they belong to the daemon and outlive the window.
    owned: Mutex<Vec<SessionId>>,
    /// Never sent on; tasks watch for it being dropped.
    alive: watch::Receiver<()>,
}

async fn connection(core: Core, stream: UnixStream, _connected: Connected) {
    match stream.peer_cred() {
        Ok(peer) if peer.uid() == super::own_uid() => {}
        Ok(peer) => {
            tracing::warn!("refused a client running as uid {}", peer.uid());
            return;
        }
        Err(e) => {
            tracing::warn!("refused a client whose user is unknown: {e}");
            return;
        }
    }
    let (read, write) = stream.into_split();
    let (control, control_rx) = mpsc::channel(OUTBOX);
    let (output, output_rx) = mpsc::channel(OUTPUT_QUEUE);
    let (alive_tx, alive) = watch::channel(());
    tokio::spawn(write_all(write, control_rx, output_rx));
    let conn = Arc::new(Conn {
        core,
        id: CONNECTIONS.fetch_add(1, Ordering::Relaxed) + 1,
        role: OnceLock::new(),
        control,
        output,
        forwarders: Mutex::default(),
        owned: Mutex::default(),
        alive,
    });

    let mut read = BufReader::new(read);
    let mut line = Vec::new();
    loop {
        match read_line(&mut read, &mut line, MAX_LINE).await {
            Ok(Some(())) => {}
            Ok(None) => break,
            Err(e) => {
                tracing::warn!("dropped a client: {e}");
                break;
            }
        }
        let message = serde_json::from_slice::<Message>(&line);
        // Commands can carry secrets; this copy at least is not left behind.
        line.zeroize();
        match message {
            Ok(message) => {
                let conn = conn.clone();
                // Requests run on their own task, so a slow command does not
                // hold up the client's other requests.
                tokio::spawn(async move { conn.handle(message).await });
            }
            Err(e) => tracing::warn!("ignoring a line that is not a request: {e}"),
        }
    }
    for (_, forwarder) in conn.forwarders.lock().unwrap().drain() {
        forwarder.abort();
    }
    drop(alive_tx);
    let owned = std::mem::take(&mut *conn.owned.lock().unwrap());
    for session in owned {
        // Most have ended already; the rest end now.
        let _ = conn.core.execute(Command::CloseSession { session }).await;
    }
}

#[derive(Deserialize)]
struct Message {
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Deserialize)]
struct SessionParams {
    session: SessionId,
}

type Failure = (i64, String);

impl Conn {
    async fn handle(self: Arc<Self>, message: Message) {
        let result = self.dispatch(&message.method, message.params).await;
        // Notifications get no answer, not even an error.
        let Some(id) = message.id else {
            if let Err((_, error)) = result {
                tracing::warn!("{}: {error}", message.method);
            }
            return;
        };
        let reply = match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err((code, error)) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": code, "message": error},
            }),
        };
        let _ = self.control.send(reply.to_string()).await;
    }

    async fn dispatch(&self, method: &str, params: Value) -> Result<Value, Failure> {
        if method == "initialize" {
            return self.initialize(parse(params)?);
        }
        let Some(&role) = self.role.get() else {
            return Err((SERVER_ERROR, "send initialize first".to_string()));
        };
        match method {
            "execute" => {
                let command: Command = parse(params)?;
                let owns = command.class().opens_session;
                let response = match role {
                    Role::Gui | Role::Cli => self.core.execute(command).await,
                    Role::Mcp => self.execute_mcp(command).await,
                }
                .map_err(failed)?;
                if owns && let Response::Session { session } = response {
                    let running = self.core.sessions().list();
                    let mut owned = self.owned.lock().unwrap();
                    owned.retain(|s| running.iter().any(|r| r.session == *s));
                    owned.push(session);
                }
                Ok(serde_json::to_value(response).unwrap_or_default())
            }
            "take_output" => {
                let SessionParams { session } = parse(params)?;
                self.take_output(session).map_err(failed)?;
                Ok(Value::Null)
            }
            "release_output" => {
                let SessionParams { session } = parse(params)?;
                if let Some(forwarder) = self.forwarders.lock().unwrap().remove(&session) {
                    forwarder.abort();
                }
                Ok(Value::Null)
            }
            _ => Err((METHOD_NOT_FOUND, format!("unknown method {method:?}"))),
        }
    }

    fn initialize(&self, hello: Initialize) -> Result<Value, Failure> {
        if hello.api != API_VERSION {
            return Err((
                SERVER_ERROR,
                format!(
                    "this client needs API {}, but burrowd {BUILD} has API {API_VERSION}",
                    hello.api
                ),
            ));
        }
        if self.role.set(hello.role).is_err() {
            return Err((SERVER_ERROR, "already initialized".to_string()));
        }
        tokio::spawn(forward_events(
            self.core.subscribe(),
            self.control.clone(),
            hello.role,
            self.alive.clone(),
        ));
        let info = ServerInfo {
            api: API_VERSION,
            build: BUILD.to_string(),
            addons_dir: self.core.addons_dir(),
        };
        Ok(serde_json::to_value(info).unwrap_or_default())
    }

    /// Passes a terminal's output on as `output` notifications, then
    /// `output_ended`. Both go through the output queue so the end never
    /// overtakes the last bytes.
    fn take_output(&self, session: SessionId) -> Result<()> {
        let mut stream = self.core.take_output(session)?;
        let output = self.output.clone();
        let mut alive = self.alive.clone();
        let task = tokio::spawn(async move {
            loop {
                let message = tokio::select! {
                    data = stream.recv() => match data {
                        Some(data) => json!({
                            "jsonrpc": "2.0",
                            "method": "output",
                            "params": {"session": session, "data": BASE64.encode(&data)},
                        }),
                        None => break,
                    },
                    _ = alive.changed() => return,
                };
                if output.send(message.to_string()).await.is_err() {
                    return;
                }
            }
            let ended = json!({
                "jsonrpc": "2.0",
                "method": "output_ended",
                "params": {"session": session},
            });
            let _ = output.send(ended.to_string()).await;
        });
        self.forwarders
            .lock()
            .unwrap()
            .insert(session, task.abort_handle());
        Ok(())
    }
}

/// What the MCP bridge may do with a command.
enum Rule {
    Run,
    /// A query that must not write, whatever the session allows.
    ReadOnlyQuery,
    /// A monitor that runs only the agent the user put on the server.
    MonitorWithoutInstall,
    /// Only the alerts of hosts it may monitor.
    GrantedAlerts,
}

impl Rule {
    /// How the audit log names it.
    fn name(&self) -> &'static str {
        match self {
            Rule::Run => "run",
            Rule::ReadOnlyQuery => "read_only_query",
            Rule::MonitorWithoutInstall => "monitor_without_install",
            Rule::GrantedAlerts => "granted_alerts",
        }
    }
}

impl Conn {
    /// Runs a command for the MCP bridge if its grants allow it, and
    /// records it in the audit log either way.
    async fn execute_mcp(&self, command: Command) -> Result<Response> {
        let rule = self.mcp_rule(&command);
        let mut entry = audit_entry(&command, rule.as_ref().ok(), self.id);
        let result = match rule {
            Err(e) => Err(e),
            Ok(Rule::Run) => self.core.execute(command).await,
            Ok(Rule::ReadOnlyQuery) => match command {
                Command::RunQuery {
                    session,
                    sql,
                    limit,
                } => self.core.run_query(session, sql, limit, true).await,
                _ => unreachable!("only queries are read-only"),
            },
            Ok(Rule::MonitorWithoutInstall) => match command {
                Command::OpenMonitor { node } => self
                    .core
                    .open_monitor(node, false)
                    .map(|session| Response::Session { session }),
                _ => unreachable!("only monitors install"),
            },
            Ok(Rule::GrantedAlerts) => {
                self.core
                    .execute(command)
                    .await
                    .map(|response| match response {
                        Response::Alerts { alerts } => Response::Alerts {
                            alerts: alerts
                                .into_iter()
                                .filter(|alert| {
                                    self.core.granted(alert.node, Capability::MonitorRead)
                                })
                                .collect(),
                        },
                        other => other,
                    })
            }
        };
        entry["ok"] = json!(result.is_ok());
        match &result {
            // Commands on the session name only its id, so this ties them
            // to the node it was opened on.
            Ok(Response::Session { session }) => entry["session"] = json!(session),
            Ok(_) => {}
            Err(e) => entry["error"] = json!(e.to_string()),
        }
        cap_strings(&mut entry);
        self.core.audit(&entry);
        result
    }

    /// What the MCP bridge may run. The command's class says what it
    /// needs (see [`McpRule`]); this checks that against the grants and
    /// the sessions the bridge opened itself.
    fn mcp_rule(&self, command: &Command) -> Result<Rule> {
        match command.class().mcp {
            McpRule::Free => {}
            McpRule::Node(node, capability) => self.require(node, capability)?,
            McpRule::Session(session, capability) => {
                let node = self.own_session(session)?;
                self.require(node, capability)?;
            }
            McpRule::Query(session) => {
                let node = self.own_session(session)?;
                self.require(node, Capability::SqlRead)?;
                if !self.core.granted(node, Capability::SqlWrite) {
                    return Ok(Rule::ReadOnlyQuery);
                }
            }
            McpRule::Monitor(node) => {
                self.require(node, Capability::MonitorRead)?;
                return Ok(Rule::MonitorWithoutInstall);
            }
            McpRule::Alerts => return Ok(Rule::GrantedAlerts),
            McpRule::Close(session) => {
                if self.own_session(session).is_err() {
                    let tunnel = self.core.sessions().list().into_iter().find(|i| {
                        i.session == session && i.kind == crate::api::SessionKind::Tunnel
                    });
                    match tunnel {
                        Some(info) => self.require(info.node, Capability::Tunnel)?,
                        None => return Err(denied("close a session it did not open")),
                    }
                }
            }
            McpRule::Denied => return Err(denied(&format!("run {}", command_type(command)))),
        }
        Ok(Rule::Run)
    }

    fn require(&self, node: NodeId, capability: Capability) -> Result<()> {
        if self.core.granted(node, capability) {
            return Ok(());
        }
        Err(denied(&format!(
            "use {capability} on node {node}; the user can grant it in Burrow"
        )))
    }

    /// The node of a running session this client opened.
    fn own_session(&self, session: SessionId) -> Result<NodeId> {
        if !self.owned.lock().unwrap().contains(&session) {
            return Err(denied("use a session it did not open"));
        }
        self.core
            .sessions()
            .list()
            .into_iter()
            .find(|info| info.session == session)
            .map(|info| info.node)
            .ok_or(Error::SessionNotFound(session))
    }
}

fn denied(what: &str) -> Error {
    Error::PermissionDenied(format!("the MCP bridge may not {what}"))
}

fn command_type(command: &Command) -> String {
    serde_json::to_value(command)
        .ok()
        .and_then(|v| v["type"].as_str().map(str::to_string))
        .unwrap_or_default()
}

/// What the audit log keeps of a command, and the `rule` it runs under
/// when it may run at all. A refused one is kept by type only, since it
/// may carry a secret; file contents are kept by size.
fn audit_entry(command: &Command, rule: Option<&Rule>, connection: u64) -> Value {
    let time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    let mut logged = match rule {
        Some(_) => serde_json::to_value(command).unwrap_or_default(),
        None => json!({"type": command_type(command)}),
    };
    if let Command::WriteFile { data, .. } = command {
        logged["data"] = json!(format!("{} bytes", data.len()));
    }
    let mut entry = json!({"time": time, "connection": connection, "command": logged});
    if let Some(rule) = rule {
        entry["rule"] = json!(rule.name());
    }
    entry
}

/// Cuts every string in `value` that is longer than [`AUDIT_FIELD`], and
/// says how long it was.
fn cap_strings(value: &mut Value) {
    match value {
        Value::String(text) if text.len() > AUDIT_FIELD => {
            let mut end = AUDIT_FIELD;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            *text = format!("{}… ({} bytes)", &text[..end], text.len());
        }
        Value::Array(items) => items.iter_mut().for_each(cap_strings),
        Value::Object(fields) => fields.values_mut().for_each(cap_strings),
        _ => {}
    }
}

/// Passes events on as `event` notifications. A client that falls behind
/// is sent `resync` and should list everything again.
async fn forward_events(
    mut events: tokio::sync::broadcast::Receiver<Event>,
    control: mpsc::Sender<String>,
    role: Role,
    mut alive: watch::Receiver<()>,
) {
    loop {
        let message = tokio::select! {
            event = events.recv() => match event {
                Ok(event) if may_see(role, &event) => {
                    json!({"jsonrpc": "2.0", "method": "event", "params": event})
                }
                Ok(_) => continue,
                Err(RecvError::Lagged(_)) => json!({"jsonrpc": "2.0", "method": "resync"}),
                Err(RecvError::Closed) => return,
            },
            _ = alive.changed() => return,
        };
        tokio::select! {
            sent = control.send(message.to_string()) => if sent.is_err() { return },
            _ = alive.changed() => return,
        }
    }
}

/// The MCP bridge hears about the sidebar and its grants. It follows its
/// own sessions by listing them, and the rest is none of its business.
fn may_see(role: Role, event: &Event) -> bool {
    match role {
        Role::Gui | Role::Cli => true,
        Role::Mcp => matches!(
            event,
            Event::NodeCreated { .. }
                | Event::NodeUpdated { .. }
                | Event::NodeDeleted { .. }
                | Event::GrantsChanged { .. }
        ),
    }
}

/// Writes replies and events first, and terminal output when there are
/// none waiting.
async fn write_all(
    mut write: OwnedWriteHalf,
    mut control: mpsc::Receiver<String>,
    mut output: mpsc::Receiver<String>,
) {
    loop {
        let mut line = tokio::select! {
            biased;
            Some(line) = control.recv() => line,
            Some(line) = output.recv() => line,
            else => return,
        };
        line.push('\n');
        if write.write_all(line.as_bytes()).await.is_err() {
            return;
        }
    }
}

fn parse<T: serde::de::DeserializeOwned>(params: Value) -> Result<T, Failure> {
    serde_json::from_value(params).map_err(|e| (INVALID_PARAMS, e.to_string()))
}

fn failed(error: Error) -> Failure {
    (SERVER_ERROR, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_audit_line_keeps_only_the_start_of_long_strings() {
        let command = Command::RunQuery {
            session: SessionId(1),
            sql: format!("select '{}'", "é".repeat(1 << 20)),
            limit: None,
        };
        let mut entry = audit_entry(&command, Some(&Rule::ReadOnlyQuery), 1);
        entry["error"] = json!("x".repeat(AUDIT_FIELD + 1));
        cap_strings(&mut entry);
        assert!(entry.to_string().len() < 3 * AUDIT_FIELD);
        let sql = entry["command"]["sql"].as_str().unwrap();
        assert!(sql.starts_with("select 'éé"));
        assert!(
            sql.ends_with(&format!("… ({} bytes)", 9 + (2 << 20))),
            "{sql}"
        );
        assert!(entry["error"].as_str().unwrap().ends_with("… (4097 bytes)"));

        // Short ones, and everything else, are left as they were.
        let mut entry = audit_entry(&Command::ListNodes, Some(&Rule::Run), 2);
        let before = entry.clone();
        cap_strings(&mut entry);
        assert_eq!(entry, before);
    }
}
