//! Which sessions a client may act on, beyond what its role may run.
//!
//! The window and the CLI may use any session. The MCP bridge uses only
//! the ones it opened (see `mcp.rs`). The phone uses any session except
//! another client's terminals: it is the one client on another device, so
//! it must not type into the window's shells or read them, and the daemon
//! decides that rather than the app.

use super::Conn;
use crate::api::rpc::Role;
use crate::api::{SessionId, SessionKind};
use crate::error::{Error, Result};

impl Conn {
    /// Whether this client opened `session`.
    pub(super) fn owns(&self, session: SessionId) -> bool {
        self.owned.lock().unwrap().contains(&session)
    }

    /// Refuses the phone a terminal it neither opened on this connection
    /// nor kept; other roles pass. A kept terminal is the phone's again
    /// after it reconnects, because the daemon recorded the role that kept
    /// it.
    pub(super) fn may_use(&self, session: SessionId) -> Result<()> {
        if self.role.get() != Some(&Role::Mobile) || self.owns(session) {
            return Ok(());
        }
        // A session that isn't running is refused here rather than by the
        // command, so an id can't become someone's terminal in between.
        let info = self
            .core
            .sessions()
            .list()
            .into_iter()
            .find(|i| i.session == session)
            .ok_or(Error::SessionNotFound(session))?;
        if info.kind != SessionKind::Terminal || info.kept_by == Some(Role::Mobile) {
            return Ok(());
        }
        Err(Error::PermissionDenied(
            "the phone may only use terminals it opened or kept".to_string(),
        ))
    }
}
