//! A client's side of the socket. [`Client`] offers what burrow-core's
//! `Core` offers in-process, so a front end can drive either the same way.
//!
//! It works over any pair of byte streams: the daemon's Unix socket on the
//! same computer, or an SSH channel running `burrow proxy` from a phone.

mod events;
mod output;
mod reader;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::{broadcast, mpsc, oneshot};
use zeroize::Zeroize;

pub use events::Events;
use events::Signal;

use crate::error::{Error, Result};
use crate::rpc::{BUILD, Initialize, Role, ServerInfo};
use crate::{API_VERSION, Command, Response, SessionId};

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

impl Client {
    /// Connects to the daemon's socket at `path` and says hello.
    #[cfg(unix)]
    pub async fn connect(path: &std::path::Path, role: Role) -> Result<Client> {
        let stream = tokio::net::UnixStream::connect(path)
            .await
            .map_err(|source| Error::Io {
                path: path.to_path_buf(),
                source,
            })?;
        let (read, write) = stream.into_split();
        Client::over(read, write, role).await
    }

    /// Talks to the daemon over `read` and `write`, and says hello as
    /// `role`. They can be any stream's two halves, such as an SSH
    /// channel's from `tokio::io::split`. The client owns them from here:
    /// a reader and a writer task each take one.
    pub async fn over<R, W>(read: R, write: W, role: Role) -> Result<Client>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
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
        let reader = tokio::spawn(reader::read_all(read, shared.clone()));
        let reader = Reader(reader.abort_handle());
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

    /// Receives every [`Event`](crate::Event) sent after this call.
    pub fn subscribe(&self) -> Events {
        Events::new(
            self.shared.events.subscribe(),
            self.shared.closed.load(Ordering::SeqCst),
        )
    }
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("build", &self.info.build)
            .finish_non_exhaustive()
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

async fn write_all<W: AsyncWrite + Unpin>(mut write: W, mut outbox: mpsc::Receiver<String>) {
    while let Some(mut line) = outbox.recv().await {
        line.push('\n');
        // A stream with a buffer of its own, such as an SSH channel's, only
        // sends once flushed.
        let written = match write.write_all(line.as_bytes()).await {
            Ok(()) => write.flush().await,
            Err(e) => Err(e),
        };
        // Commands can carry secrets; this copy at least is not left behind.
        line.zeroize();
        if written.is_err() {
            return;
        }
    }
}
