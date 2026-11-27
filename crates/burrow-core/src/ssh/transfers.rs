//! Running file transfers, by id, so a client can cancel one.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;

use tokio::sync::watch;

use crate::api::{SessionId, TransferId};

/// Running transfers, so they can be cancelled.
#[derive(Default)]
pub(crate) struct Transfers {
    pub(super) next: AtomicU64,
    pub(super) map: Mutex<HashMap<TransferId, Running>>,
}

pub(super) struct Running {
    pub(super) session: SessionId,
    pub(super) cancel: watch::Sender<bool>,
}

impl Transfers {
    pub fn cancel(&self, id: TransferId) -> crate::error::Result<()> {
        let map = self.map.lock().unwrap();
        let running = map.get(&id).ok_or_else(|| {
            crate::error::Error::invalid("transfer", format!("{id} is not running"))
        })?;
        running.cancel.send_replace(true);
        Ok(())
    }

    /// Cancels every transfer of a session that ended.
    pub fn cancel_session(&self, session: SessionId) {
        for running in self.map.lock().unwrap().values() {
            if running.session == session {
                running.cancel.send_replace(true);
            }
        }
    }
}
