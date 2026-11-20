//! Terminal output: a session's stream, and attaching to a kept terminal.

use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use super::{Client, OUTPUT_BUFFER, call};
use crate::SessionId;
use crate::error::{Error, Result};

/// Where an attach's replay goes, and the stream that follows it.
pub(super) type Replay = (oneshot::Sender<Vec<u8>>, mpsc::Sender<Vec<u8>>);

impl Client {
    /// Takes a session's terminal output stream, as burrow-core's
    /// `Core::take_output` does. Dropping it ends the session once the
    /// next output finds nobody reading, as in-process.
    pub async fn take_output(&self, session: SessionId) -> Result<mpsc::Receiver<Vec<u8>>> {
        let (tx, rx) = mpsc::channel(OUTPUT_BUFFER);
        self.shared.outputs.lock().unwrap().insert(session, tx);
        let taken = call::<Value>(&self.shared, "take_output", json!({"session": session})).await;
        if let Err(e) = taken {
            self.shared.outputs.lock().unwrap().remove(&session);
            return Err(e);
        }
        Ok(rx)
    }

    /// Attaches to a terminal kept with
    /// [`Command::KeepTerminal`](crate::Command::KeepTerminal): returns
    /// what it kept while nobody read it, then a stream of its output from
    /// there on, with nothing repeated between them. Any reader attached
    /// before, here or in another client, is detached. Dropping the stream
    /// detaches this one; the terminal runs on.
    pub async fn attach_output(
        &self,
        session: SessionId,
    ) -> Result<(Vec<u8>, mpsc::Receiver<Vec<u8>>)> {
        let (tx, rx) = mpsc::channel(OUTPUT_BUFFER);
        let (replay_tx, replay_rx) = oneshot::channel();
        // An earlier stream of this session ends here, and its output still
        // on the way is dropped until the replay arrives.
        self.shared.outputs.lock().unwrap().remove(&session);
        let waiting = (replay_tx, tx);
        self.shared.replays.lock().unwrap().insert(session, waiting);
        let attached = call::<Value>(&self.shared, "attach_output", json!({"session": session}));
        if let Err(e) = attached.await {
            self.shared.replays.lock().unwrap().remove(&session);
            return Err(e);
        }
        let lost = || Error::Remote("lost the connection to burrowd".to_string());
        let replay = replay_rx.await.map_err(|_| lost())?;
        Ok((replay, rx))
    }
}
