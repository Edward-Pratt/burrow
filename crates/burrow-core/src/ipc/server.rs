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

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, watch};
use tokio::task::AbortHandle;
use zeroize::Zeroize;

mod hello;
mod mcp;
mod output;
use super::{INVALID_PARAMS, MAX_LINE, METHOD_NOT_FOUND, Role, SERVER_ERROR, read_line};
use crate::api::{Command, Response, SessionId};
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
    owned: Arc<Mutex<Vec<SessionId>>>,
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
        owned: Arc::default(),
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
            // Answered before the next line is read, so no request races
            // the role being set. A client gets one try: a role refused,
            // or one asked for with the wrong API, ends the connection
            // rather than leave it open for another, such as a phone's
            // behind a `burrow proxy` fixed to one role.
            Ok(message) if message.method == "initialize" => {
                conn.clone().handle(message).await;
                if conn.role.get().is_none() {
                    break;
                }
            }
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
        // A kept terminal outlives its client; its reader just detached.
        if conn.core.sessions().is_kept(session) {
            continue;
        }
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
                let owns = crate::class::of(&command).opens_session;
                let response = match role {
                    Role::Gui | Role::Cli | Role::Mobile => self.core.execute(command).await,
                    // Nobody at the bridge can answer a session's questions. The
                    // window shows a tunnel's, whoever started it.
                    Role::Mcp if matches!(command, Command::StartTunnel { .. }) => {
                        self.execute_mcp(command).await
                    }
                    Role::Mcp => crate::ssh::unattended(self.execute_mcp(command)).await,
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
            "take_output" | "attach_output" | "release_output" => {
                let SessionParams { session } = parse(params)?;
                self.output_method(method, session).map_err(failed)?;
                Ok(Value::Null)
            }
            _ => Err((METHOD_NOT_FOUND, format!("unknown method {method:?}"))),
        }
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
