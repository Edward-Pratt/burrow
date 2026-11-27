//! Terminal output, passed on to the client that took it or attached to it.

use data_encoding::BASE64;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::Conn;
use crate::api::SessionId;
use crate::api::rpc::Role;
use crate::error::{Error, Result};

impl Conn {
    /// Runs `take_output`, `attach_output` or `release_output`.
    pub(super) fn output_method(&self, method: &str, session: SessionId) -> Result<()> {
        match method {
            "take_output" => self.take_output(session),
            "attach_output" => self.attach_output(session),
            _ => {
                if let Some(forwarder) = self.forwarders.lock().unwrap().remove(&session) {
                    forwarder.abort();
                }
                Ok(())
            }
        }
    }

    /// Passes a terminal's output on as `output` notifications, then
    /// `output_ended`.
    fn take_output(&self, session: SessionId) -> Result<()> {
        self.reads_terminal(session)?;
        let stream = self.core.take_output(session)?;
        self.forward(session, None, stream);
        Ok(())
    }

    /// Attaches to a kept terminal: `output_replay` with what it kept, then
    /// its output as for [`Conn::take_output`]. The reader before, here or
    /// in another client, is detached.
    ///
    /// The replay goes through the output queue rather than in the answer:
    /// an earlier stream's output may still be queued, and the client
    /// drops whatever comes before the replay, so nothing arrives twice.
    fn attach_output(&self, session: SessionId) -> Result<()> {
        self.reads_terminal(session)?;
        // The earlier forwarder goes quietly: its `output_ended` would end
        // the new stream too.
        if let Some(earlier) = self.forwarders.lock().unwrap().remove(&session) {
            earlier.abort();
        }
        let (replay, stream) = self.core.attach_output(session)?;
        let replay = json!({
            "jsonrpc": "2.0",
            "method": "output_replay",
            "params": {"session": session, "data": BASE64.encode(&replay)},
        });
        self.forward(session, Some(replay), stream);
        Ok(())
    }

    /// The MCP bridge opens no terminals or desktops, so it has no output
    /// to take, and must not read or take over someone else's. The phone
    /// reads only its own.
    fn reads_terminal(&self, session: SessionId) -> Result<()> {
        match self.role.get() {
            Some(Role::Mcp) => Err(Error::PermissionDenied(
                "the MCP bridge may not read terminals".to_string(),
            )),
            _ => self.may_use(session),
        }
    }

    /// Sends `first`, then the stream's chunks, then the end. All of it goes
    /// through the output queue, so the end never overtakes the last bytes.
    fn forward(
        &self,
        session: SessionId,
        first: Option<Value>,
        mut stream: mpsc::Receiver<Vec<u8>>,
    ) {
        let output = self.output.clone();
        let mut alive = self.alive.clone();
        let task = tokio::spawn(async move {
            if let Some(first) = first
                && output.send(first.to_string()).await.is_err()
            {
                return;
            }
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
    }
}
