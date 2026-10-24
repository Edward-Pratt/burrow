//! A running addon process and the JSON-RPC 2.0 connection to it: one
//! message per line on its stdin and stdout.
//!
//! Four tasks serve each process. The supervisor owns the child and kills
//! it when the [`Process`] is dropped. The writer is the only one touching
//! stdin. The reader dispatches what the addon sends. The event forwarder
//! passes core events on, filtered by the addon's permissions.

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{mpsc, oneshot, watch};

use super::manifest::{Manifest, Permission};
use crate::api::{API_VERSION, Command, Event, NodeId, Response, View};
use crate::engine::{Core, WeakCore};

/// The longest line accepted from an addon. A longer one stops it: a
/// runaway addon must not make the core buffer without limit.
pub(crate) const MAX_LINE: usize = 1 << 20;

/// How long the core waits for an addon to answer a request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Messages queued for the addon's stdin before senders wait.
const OUTBOX: usize = 64;

/// Environment variables an addon gets. Everything else is left out, so it
/// does not inherit pointers to the user's credentials such as
/// `SSH_AUTH_SOCK`. This is not a wall: the addon runs as the user and can
/// still find such sockets by path.
const KEPT_ENV: [&str; 9] = [
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "LANG",
    "LC_ALL",
    "TZ",
    "TMPDIR",
    "XDG_RUNTIME_DIR",
];

/// JSON-RPC error codes.
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const SERVER_ERROR: i64 = -32000;

type Pending = Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>;

/// The parts of a connection the tasks share.
struct Shared {
    id: String,
    outbox: mpsc::Sender<String>,
    pending: Pending,
    next: AtomicU64,
    /// Set once the addon's output has ended. Nothing more will be answered.
    closed: AtomicBool,
}

impl Shared {
    /// Fails every request still waiting, and any made later.
    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.pending.lock().unwrap().clear();
    }
}

/// A running addon. Dropping it stops the process.
pub(crate) struct Process {
    pub generation: u64,
    pub started: Instant,
    shared: Arc<Shared>,
    /// Never sent on; tasks watch for it being dropped.
    _alive: watch::Sender<()>,
}

impl Process {
    /// Sends a request and waits for the answer.
    pub async fn call<T: DeserializeOwned>(
        &self,
        method: &str,
        params: Value,
    ) -> Result<T, String> {
        let shared = &self.shared;
        let id = shared.next.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = oneshot::channel();
        shared.pending.lock().unwrap().insert(id, tx);
        if shared.closed.load(Ordering::SeqCst) {
            shared.pending.lock().unwrap().remove(&id);
            return Err("it stopped".to_string());
        }
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let exchange = async {
            shared
                .outbox
                .send(message.to_string())
                .await
                .map_err(|_| "it stopped".to_string())?;
            rx.await.map_err(|_| "it stopped".to_string())?
        };
        let result = tokio::time::timeout(REQUEST_TIMEOUT, exchange).await;
        shared.pending.lock().unwrap().remove(&id);
        let value = result.map_err(|_| {
            format!(
                "it did not answer {method} within {} s",
                REQUEST_TIMEOUT.as_secs()
            )
        })??;
        serde_json::from_value(value).map_err(|e| format!("bad answer to {method}: {e}"))
    }
}

/// Starts the addon's program and says hello. `generation` tells this run
/// apart from earlier ones in [`Host`](super::Host) callbacks.
pub(crate) async fn start(
    core: &Core,
    manifest: Arc<Manifest>,
    data: &Path,
    generation: u64,
) -> Result<Process, String> {
    let process = spawn(core, &manifest, data, generation)?;
    process
        .call::<Value>("initialize", json!({"api": API_VERSION, "id": manifest.id}))
        .await?;
    Ok(process)
}

/// Starts the program and its tasks.
///
/// Not `async` on purpose: the reader task runs commands through
/// `Core::execute`, which can itself start an addon. Spawning from inside an
/// `async fn` would make that future's type depend on itself.
///
/// It runs in `data`, its writable folder, rather than its own folder:
/// any file written there would change the fingerprint and withdraw the
/// approval. For the same reason Python is told to keep bytecode caches in
/// `data`.
fn spawn(
    core: &Core,
    manifest: &Arc<Manifest>,
    data: &Path,
    generation: u64,
) -> Result<Process, String> {
    let mut command = tokio::process::Command::new(&manifest.program);
    command
        .args(&manifest.args)
        .current_dir(data)
        .env_clear()
        .envs(
            KEPT_ENV
                .iter()
                .filter_map(|k| Some((k, std::env::var_os(k)?))),
        )
        .env("BURROW_ADDON_ID", &manifest.id)
        .env("BURROW_ADDON_DIR", &manifest.dir)
        .env("BURROW_ADDON_DATA", data)
        .env("BURROW_API", API_VERSION.to_string())
        .env("PYTHONPYCACHEPREFIX", data.join("pycache"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|e| format!("could not start {}: {e}", manifest.program.display()))?;
    let stdin = child.stdin.take().expect("stdin is piped");
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");

    let (outbox, outbox_rx) = mpsc::channel(OUTBOX);
    let (alive, _) = watch::channel(());
    let shared = Arc::new(Shared {
        id: manifest.id.clone(),
        outbox,
        pending: Mutex::default(),
        next: AtomicU64::new(0),
        closed: AtomicBool::new(false),
    });
    let weak = core.downgrade();

    tokio::spawn(supervise(
        child,
        alive.subscribe(),
        weak.clone(),
        manifest.id.clone(),
        generation,
    ));
    tokio::spawn(write(stdin, outbox_rx, alive.subscribe()));
    tokio::spawn(log_stderr(manifest.id.clone(), stderr));
    tokio::spawn(read(
        BufReader::new(stdout),
        shared.clone(),
        manifest.clone(),
        weak,
        generation,
    ));
    tokio::spawn(forward_events(
        core.subscribe(),
        shared.clone(),
        manifest.clone(),
        alive.subscribe(),
    ));

    Ok(Process {
        generation,
        started: Instant::now(),
        shared,
        _alive: alive,
    })
}

/// Waits for the child to exit, or kills it once its [`Process`] is gone.
async fn supervise(
    mut child: tokio::process::Child,
    mut alive: watch::Receiver<()>,
    core: WeakCore,
    id: String,
    generation: u64,
) {
    tokio::select! {
        status = child.wait() => {
            let error = match status {
                Ok(status) => format!("it stopped ({status})"),
                Err(e) => format!("waiting for it failed: {e}"),
            };
            if let Some(core) = core.upgrade() {
                core.addons().stopped(&core, &id, generation, error);
            }
        }
        _ = alive.changed() => {
            let _ = child.kill().await;
        }
    }
}

async fn write(
    mut stdin: tokio::process::ChildStdin,
    mut outbox: mpsc::Receiver<String>,
    mut alive: watch::Receiver<()>,
) {
    loop {
        let line = tokio::select! {
            line = outbox.recv() => line,
            _ = alive.changed() => None,
        };
        let Some(mut line) = line else { return };
        line.push('\n');
        if stdin.write_all(line.as_bytes()).await.is_err() || stdin.flush().await.is_err() {
            return;
        }
    }
}

async fn log_stderr(id: String, stderr: tokio::process::ChildStderr) {
    let mut lines = BufReader::new(stderr);
    let mut line = Vec::new();
    while let Ok(Some(())) = read_line(&mut lines, &mut line, MAX_LINE).await {
        tracing::info!(addon = %id, "{}", String::from_utf8_lossy(&line));
    }
}

async fn read(
    mut stdout: BufReader<tokio::process::ChildStdout>,
    shared: Arc<Shared>,
    manifest: Arc<Manifest>,
    core: WeakCore,
    generation: u64,
) {
    let mut line = Vec::new();
    loop {
        match read_line(&mut stdout, &mut line, MAX_LINE).await {
            Ok(Some(())) => {}
            // The supervisor reports the exit.
            Ok(None) => return shared.close(),
            Err(error) => {
                shared.close();
                if let Some(core) = core.upgrade() {
                    core.addons()
                        .stopped(&core, &manifest.id, generation, error);
                }
                return;
            }
        }
        let message: Value = match serde_json::from_slice(&line) {
            Ok(message) => message,
            Err(e) => {
                tracing::warn!(addon = %manifest.id, "ignoring a line that is not JSON: {e}");
                continue;
            }
        };
        match (message.get("method"), message.get("id")) {
            (Some(_), _) => {
                let Some(core) = core.upgrade() else { return };
                // Requests run on their own task, so a slow command does not
                // hold up answers the addon is waiting for.
                let (shared, manifest) = (shared.clone(), manifest.clone());
                tokio::spawn(async move { handle(core, shared, manifest, message).await });
            }
            (None, Some(id)) => {
                let Some(id) = id.as_u64() else { continue };
                let result = match message.get("error") {
                    Some(error) => Err(error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error")
                        .to_string()),
                    None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                };
                if let Some(waiting) = shared.pending.lock().unwrap().remove(&id) {
                    let _ = waiting.send(result);
                }
            }
            (None, None) => {
                tracing::warn!(addon = %manifest.id, "ignoring a message with no method or id");
            }
        }
    }
}

/// Runs one request or notification from the addon.
async fn handle(core: Core, shared: Arc<Shared>, manifest: Arc<Manifest>, message: Value) {
    let method = message["method"].as_str().unwrap_or_default().to_string();
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let result = match method.as_str() {
        "execute" => match serde_json::from_value::<Command>(params) {
            Ok(command) => core
                .execute_as_addon(manifest.clone(), command)
                .await
                .map(|response: Response| serde_json::to_value(response).unwrap_or_default())
                .map_err(|e| (SERVER_ERROR, e.to_string())),
            Err(e) => Err((INVALID_PARAMS, e.to_string())),
        },
        "view_changed" => match serde_json::from_value::<ViewChanged>(params) {
            Ok(changed) => core
                .view_changed(&manifest, changed.node, changed.view)
                .map(|()| Value::Null)
                .map_err(|e| (SERVER_ERROR, e.to_string())),
            Err(e) => Err((INVALID_PARAMS, e.to_string())),
        },
        _ => Err((METHOD_NOT_FOUND, format!("unknown method {method:?}"))),
    };
    // Notifications get no answer, not even an error.
    let Some(id) = message.get("id") else {
        if let Err((_, error)) = result {
            tracing::warn!(addon = %shared.id, "{method}: {error}");
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
    let _ = shared.outbox.send(reply.to_string()).await;
}

#[derive(serde::Deserialize)]
struct ViewChanged {
    node: NodeId,
    view: View,
}

/// Passes events on as `event` notifications. Each addon reads from its
/// own broadcast receiver, so a slow addon only falls behind itself; it is
/// then sent `resync` and should list nodes again.
async fn forward_events(
    mut events: tokio::sync::broadcast::Receiver<Event>,
    shared: Arc<Shared>,
    manifest: Arc<Manifest>,
    mut alive: watch::Receiver<()>,
) {
    loop {
        let message = tokio::select! {
            event = events.recv() => match event {
                Ok(event) if may_see(&manifest, &event) => {
                    json!({"jsonrpc": "2.0", "method": "event", "params": event})
                }
                Ok(_) => continue,
                Err(RecvError::Lagged(_)) => json!({"jsonrpc": "2.0", "method": "resync"}),
                Err(RecvError::Closed) => return,
            },
            _ = alive.changed() => return,
        };
        tokio::select! {
            sent = shared.outbox.send(message.to_string()) => if sent.is_err() { return },
            _ = alive.changed() => return,
        }
    }
}

/// Node events need `nodes:read`. Sessions, prompts, problems, other
/// addons and views are none of an addon's business.
fn may_see(manifest: &Manifest, event: &Event) -> bool {
    match event {
        Event::NodeCreated { .. } | Event::NodeUpdated { .. } | Event::NodeDeleted { .. } => {
            manifest.has(Permission::NodesRead)
        }
        _ => false,
    }
}

/// Reads one line, without its `\n`, into `line`. `Ok(None)` at the end of
/// input. A line longer than `max` is an error, found before it is all
/// buffered.
async fn read_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    line: &mut Vec<u8>,
    max: usize,
) -> Result<Option<()>, String> {
    line.clear();
    loop {
        let available = reader
            .fill_buf()
            .await
            .map_err(|e| format!("reading from it failed: {e}"))?;
        if available.is_empty() {
            return Ok(if line.is_empty() { None } else { Some(()) });
        }
        let (taken, done) = match available.iter().position(|&b| b == b'\n') {
            Some(i) => (i, true),
            None => (available.len(), false),
        };
        if line.len() + taken > max {
            return Err(format!("it sent a message over {} bytes", max));
        }
        line.extend_from_slice(&available[..taken]);
        reader.consume(if done { taken + 1 } else { taken });
        if done {
            return Ok(Some(()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reads_lines_up_to_the_limit() {
        let mut input: &[u8] = b"one\ntwo\nthree";
        let mut line = Vec::new();
        for expected in ["one", "two", "three"] {
            assert_eq!(read_line(&mut input, &mut line, 8).await, Ok(Some(())));
            assert_eq!(line, expected.as_bytes());
        }
        assert_eq!(read_line(&mut input, &mut line, 8).await, Ok(None));

        let mut input: &[u8] = b"123456789\n";
        assert!(read_line(&mut input, &mut line, 8).await.is_err());
    }
}
