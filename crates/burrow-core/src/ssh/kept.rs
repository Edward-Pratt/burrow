//! Kept terminals: ones that outlive the client that opened it, such as a
//! phone whose screen locked, so that any client can attach to them
//! again. See [`Keeper`] for where their output goes meanwhile.

use tokio::sync::mpsc;

use super::Sessions;
use super::keeper::Keeper;
use crate::api::{SessionId, SessionKind};
use crate::error::{Error, Result};

/// A kept terminal's replay, and its output stream from there on.
pub(crate) type AttachedOutput = (Vec<u8>, mpsc::Receiver<Vec<u8>>);

impl Sessions {
    /// Keeps a running terminal. It must be kept before any client takes
    /// its output, so that no output goes to a reader that isn't attached.
    pub fn keep(&self, id: SessionId) -> Result<()> {
        let mut map = self.map.lock().unwrap();
        let entry = map
            .get_mut(&id)
            .filter(|e| e.input.is_some())
            .ok_or(Error::SessionNotFound(id))?;
        if entry.kind != SessionKind::Terminal {
            return Err(Error::invalid("session", format!("{id} is not a terminal")));
        }
        if entry.keeper.is_some() {
            return Ok(());
        }
        let output = entry.output.take().ok_or_else(|| {
            Error::invalid(
                "session",
                format!("{id}'s output is already taken; keep a terminal before taking it"),
            )
        })?;
        entry.keeper = Some(Keeper::start(output));
        Ok(())
    }

    /// Whether `id` is a running kept terminal.
    pub fn is_kept(&self, id: SessionId) -> bool {
        let map = self.map.lock().unwrap();
        map.get(&id)
            .is_some_and(|e| e.input.is_some() && e.keeper.is_some())
    }

    /// Attaches to a kept terminal as its one reader: what it kept, and its
    /// output from there on. A reader attached before is detached.
    pub fn attach_output(&self, id: SessionId) -> Result<AttachedOutput> {
        let map = self.map.lock().unwrap();
        let entry = map
            .get(&id)
            .filter(|e| e.input.is_some())
            .ok_or(Error::SessionNotFound(id))?;
        let keeper = entry
            .keeper
            .as_ref()
            .ok_or_else(|| Error::invalid("session", format!("{id} is not a kept terminal")))?;
        Ok(keeper.attach())
    }
}
