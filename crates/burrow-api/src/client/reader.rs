//! The client's reader task: answers to its calls, and what the daemon
//! sends unasked.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use data_encoding::BASE64;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, BufReader};

use super::events::Signal;
use super::{Shared, notify};
use crate::SessionId;
use crate::rpc::{MAX_LINE, read_line};

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

pub(super) async fn read_all<R: AsyncRead + Unpin>(read: R, shared: Arc<Shared>) {
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
    shared.replays.lock().unwrap().clear();
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
        // Output before it was for an earlier stream, already dropped.
        "output_replay" => {
            let Ok(Output { session, data }) = serde_json::from_value(params) else {
                return;
            };
            let Some((replay, tx)) = shared.replays.lock().unwrap().remove(&session) else {
                return;
            };
            let data = BASE64.decode(data.as_bytes()).unwrap_or_default();
            shared.outputs.lock().unwrap().insert(session, tx);
            let _ = replay.send(data);
        }
        "output_ended" => {
            if let Ok(Output { session, .. }) = serde_json::from_value(params) {
                shared.outputs.lock().unwrap().remove(&session);
            }
        }
        _ => tracing::debug!("ignoring {method} from burrowd"),
    }
}
