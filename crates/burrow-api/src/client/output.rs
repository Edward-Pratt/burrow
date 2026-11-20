//! Terminal output: a session's stream, and attaching to a kept terminal.

use data_encoding::BASE64;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::{Client, OUTPUT_BUFFER, call};
use crate::SessionId;
use crate::error::{Error, Result};

#[derive(Deserialize)]
struct Attached {
    replay: String,
}

impl Client {
    /// Takes a session's terminal output stream, as burrow-core's
    /// `Core::take_output` does. Dropping it ends the session once the
    /// next output finds nobody reading, as in-process.
    pub async fn take_output(&self, session: SessionId) -> Result<mpsc::Receiver<Vec<u8>>> {
        Ok(self.output(session).await?.1)
    }

    /// Attaches to a terminal kept with
    /// [`Command::KeepTerminal`](crate::Command::KeepTerminal): returns
    /// what it kept while nobody read it, then a stream of its output from
    /// there on. Any reader attached before, here or in another client, is
    /// detached. Dropping the stream detaches this one; the terminal runs on.
    pub async fn attach_output(
        &self,
        session: SessionId,
    ) -> Result<(Vec<u8>, mpsc::Receiver<Vec<u8>>)> {
        let (answer, rx) = self.output(session).await?;
        let not_kept = || Error::Remote(format!("session {session} is not a kept terminal"));
        let Attached { replay } = serde_json::from_value(answer).map_err(|_| not_kept())?;
        let replay = BASE64
            .decode(replay.as_bytes())
            .map_err(|e| Error::Remote(format!("burrowd sent a replay that is not base64: {e}")))?;
        Ok((replay, rx))
    }

    async fn output(&self, session: SessionId) -> Result<(Value, mpsc::Receiver<Vec<u8>>)> {
        let (tx, rx) = mpsc::channel(OUTPUT_BUFFER);
        self.shared.outputs.lock().unwrap().insert(session, tx);
        match call::<Value>(&self.shared, "take_output", json!({"session": session})).await {
            Ok(answer) => Ok((answer, rx)),
            Err(e) => {
                self.shared.outputs.lock().unwrap().remove(&session);
                Err(e)
            }
        }
    }
}
