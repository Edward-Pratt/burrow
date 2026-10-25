//! A client's side of the socket. [`Client`] offers what [`Core`] offers
//! in-process, so a front end can drive either the same way.
//!
//! [`Core`]: crate::Core

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use data_encoding::BASE64;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, mpsc, oneshot};
use zeroize::Zeroize;

use super::{BUILD, Initialize, MAX_LINE, Role, ServerInfo, read_line};
use crate::api::{API_VERSION, Command, Event, Response, SessionId};
use crate::error::{Error, Result};

/// Requests queued for the socket before senders wait.
const OUTBOX: usize = 64;

/// Events a slow subscriber can fall behind, as in the core.
const EVENT_BUFFER: usize = 256;

/// Output chunks buffered for each terminal, as in the core.
const OUTPUT_BUFFER: usize = 64;

type Pending = Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>;

/// A connection to `burrowd`. Cheap to clone; all clones share it.
#[derive(Clone)]
pub struct Client {
    shared: Arc<Shared>,
    info: Arc<ServerInfo>,
    _reader: Arc<Reader>,
}

/// Stops the reader when the last [`Client`] goes. The reader holds the
/// shared state and with it the writer's queue, so the socket closes too.
struct Reader(tokio::task::AbortHandle);

impl Drop for Reader {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct Shared {
    outbox: mpsc::Sender<String>,
    pending: Pending,
    next: AtomicU64,
    events: broadcast::Sender<Signal>,
    outputs: Mutex<HashMap<SessionId, mpsc::Sender<Vec<u8>>>>,
    /// Set once the daemon's side has ended. Nothing more will be answered.
    closed: AtomicBool,
}

#[derive(Clone)]
enum Signal {
    Event(Event),
    /// The daemon dropped events for this client.
    Resync,
    Closed,
}

impl Client {
    /// Connects to the daemon at `path` and says hello.
    pub async fn connect(path: &Path, role: Role) -> Result<Client> {
        let stream = UnixStream::connect(path)
            .await
            .map_err(|e| Error::io(path, e))?;
        let (read, write) = stream.into_split();
        let (outbox, outbox_rx) = mpsc::channel(OUTBOX);
        let (events, _) = broadcast::channel(EVENT_BUFFER);
        let shared = Arc::new(Shared {
            outbox,
            pending: Mutex::default(),
            next: AtomicU64::new(0),
            events,
            outputs: Mutex::default(),
            closed: AtomicBool::new(false),
        });
        tokio::spawn(write_all(write, outbox_rx));
        let reader = Reader(tokio::spawn(read_all(read, shared.clone())).abort_handle());
        let hello = Initialize {
            api: API_VERSION,
            role,
            build: BUILD.to_string(),
        };
        let info: ServerInfo = call(&shared, "initialize", json!(hello)).await?;
        Ok(Client {
            shared,
            info: Arc::new(info),
            _reader: Arc::new(reader),
        })
    }

    /// What the daemon said about itself.
    pub fn info(&self) -> &ServerInfo {
        &self.info
    }

    /// Runs one command in the daemon.
    pub async fn execute(&self, command: Command) -> Result<Response> {
        let params = serde_json::to_value(&command).map_err(|e| Error::Remote(e.to_string()))?;
        call(&self.shared, "execute", params).await
    }

    /// Receives every [`Event`] sent after this call.
    pub fn subscribe(&self) -> Events {
        Events {
            signals: self.shared.events.subscribe(),
            closed: self.shared.closed.load(Ordering::SeqCst),
        }
    }

    /// Takes a session's terminal output stream, as
    /// [`Core::take_output`](crate::Core::take_output) does. Dropping it
    /// ends the session once the next output finds nobody reading, as
    /// in-process.
    pub async fn take_output(&self, session: SessionId) -> Result<mpsc::Receiver<Vec<u8>>> {
        let (tx, rx) = mpsc::channel(OUTPUT_BUFFER);
        self.shared.outputs.lock().unwrap().insert(session, tx);
        if let Err(e) =
            call::<Value>(&self.shared, "take_output", json!({"session": session})).await
        {
            self.shared.outputs.lock().unwrap().remove(&session);
            return Err(e);
        }
        Ok(rx)
    }
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("build", &self.info.build)
            .finish_non_exhaustive()
    }
}

/// A subscription to the daemon's events, read like a broadcast receiver.
pub struct Events {
    signals: broadcast::Receiver<Signal>,
    closed: bool,
}

impl Events {
    /// The next event. `Lagged` means some were missed, here or in the
    /// daemon, and everything should be listed again. `Closed` means the
    /// connection to the daemon is gone.
    pub async fn recv(&mut self) -> Result<Event, RecvError> {
        if self.closed {
            return Err(RecvError::Closed);
        }
        match self.signals.recv().await {
            Ok(Signal::Event(event)) => Ok(event),
            Ok(Signal::Resync) => Err(RecvError::Lagged(0)),
            Ok(Signal::Closed) | Err(RecvError::Closed) => {
                self.closed = true;
                Err(RecvError::Closed)
            }
            Err(lagged) => Err(lagged),
        }
    }
}

async fn call<T: DeserializeOwned>(shared: &Shared, method: &str, params: Value) -> Result<T> {
    let lost = || Error::Remote("lost the connection to burrowd".to_string());
    let id = shared.next.fetch_add(1, Ordering::Relaxed) + 1;
    let (tx, rx) = oneshot::channel();
    shared.pending.lock().unwrap().insert(id, tx);
    // Checked after inserting, so a close in between still fails this call.
    if shared.closed.load(Ordering::SeqCst) {
        shared.pending.lock().unwrap().remove(&id);
        return Err(lost());
    }
    let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
    if shared.outbox.send(message.to_string()).await.is_err() {
        shared.pending.lock().unwrap().remove(&id);
        return Err(lost());
    }
    let value = rx.await.map_err(|_| lost())?.map_err(Error::Remote)?;
    serde_json::from_value(value)
        .map_err(|e| Error::Remote(format!("burrowd sent a bad answer to {method}: {e}")))
}

async fn notify(shared: &Shared, method: &str, params: Value) {
    let message = json!({"jsonrpc": "2.0", "method": method, "params": params});
    let _ = shared.outbox.send(message.to_string()).await;
}

async fn write_all(mut write: OwnedWriteHalf, mut outbox: mpsc::Receiver<String>) {
    while let Some(mut line) = outbox.recv().await {
        line.push('\n');
        let written = write.write_all(line.as_bytes()).await;
        // Commands can carry secrets; this copy at least is not left behind.
        line.zeroize();
        if written.is_err() {
            return;
        }
    }
}

#[derive(Deserialize)]
struct Incoming {
    id: Option<u64>,
    method: Option<String>,
    #[serde(default)]
    params: Value,
    result: Option<Value>,
    error: Option<RpcError>,
}

#[derive(Deserialize)]
struct RpcError {
    message: String,
}

#[derive(Deserialize)]
struct Output {
    session: SessionId,
    #[serde(default)]
    data: String,
}

async fn read_all(read: OwnedReadHalf, shared: Arc<Shared>) {
    let mut read = BufReader::new(read);
    let mut line = Vec::new();
    while let Ok(Some(())) = read_line(&mut read, &mut line, MAX_LINE).await {
        let message: Incoming = match serde_json::from_slice(&line) {
            Ok(message) => message,
            Err(e) => {
                tracing::warn!("ignoring a line from burrowd that is not JSON-RPC: {e}");
                continue;
            }
        };
        match message.method.as_deref() {
            Some(method) => dispatch(&shared, method, message.params).await,
            None => {
                let Some(id) = message.id else { continue };
                let result = match message.error {
                    Some(error) => Err(error.message),
                    None => Ok(message.result.unwrap_or(Value::Null)),
                };
                if let Some(waiting) = shared.pending.lock().unwrap().remove(&id) {
                    let _ = waiting.send(result);
                }
            }
        }
    }
    shared.closed.store(true, Ordering::SeqCst);
    shared.pending.lock().unwrap().clear();
    shared.outputs.lock().unwrap().clear();
    let _ = shared.events.send(Signal::Closed);
}

/// Handles a notification from the daemon.
async fn dispatch(shared: &Shared, method: &str, params: Value) {
    match method {
        "event" => match serde_json::from_value(params) {
            Ok(event) => {
                let _ = shared.events.send(Signal::Event(event));
            }
            Err(e) => tracing::warn!("ignoring an event from burrowd: {e}"),
        },
        "resync" => {
            let _ = shared.events.send(Signal::Resync);
        }
        "output" => {
            let Ok(Output { session, data }) = serde_json::from_value(params) else {
                return;
            };
            let Ok(data) = BASE64.decode(data.as_bytes()) else {
                tracing::warn!("ignoring output from burrowd that is not base64");
                return;
            };
            let tx = shared.outputs.lock().unwrap().get(&session).cloned();
            // Waiting here stops reading the socket, which slows the
            // daemon's writer and so the session, rather than buffer
            // without limit.
            if let Some(tx) = tx
                && tx.send(data).await.is_err()
            {
                shared.outputs.lock().unwrap().remove(&session);
                notify(shared, "release_output", json!({"session": session})).await;
            }
        }
        "output_ended" => {
            if let Ok(Output { session, .. }) = serde_json::from_value(params) {
                shared.outputs.lock().unwrap().remove(&session);
            }
        }
        _ => tracing::debug!("ignoring {method} from burrowd"),
    }
}
