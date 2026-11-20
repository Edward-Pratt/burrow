//! Terminal output, passed on to the client that took it.

use data_encoding::BASE64;
use serde_json::{Value, json};

use super::Conn;
use crate::api::SessionId;
use crate::error::Result;

impl Conn {
    /// Passes a terminal's output on as `output` notifications, then
    /// `output_ended`. Both go through the output queue so the end never
    /// overtakes the last bytes.
    ///
    /// A kept terminal's output may be taken again, by this client or
    /// another: the reader before is detached, and the answer holds what the
    /// terminal kept meanwhile, as `{ replay }` in base64. Otherwise the
    /// answer is null.
    pub(super) fn take_output(&self, session: SessionId) -> Result<Value> {
        // Taking it again on this connection: the earlier forwarder goes
        // quietly, since its `output_ended` would end the new stream too.
        if let Some(earlier) = self.forwarders.lock().unwrap().remove(&session) {
            earlier.abort();
        }
        let (answer, mut stream) = if self.core.sessions().is_kept(session) {
            let (replay, stream) = self.core.sessions().attach_output(session)?;
            (json!({"replay": BASE64.encode(&replay)}), stream)
        } else {
            (Value::Null, self.core.take_output(session)?)
        };
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
        Ok(answer)
    }
}
