//! Which sessions a client may act on, beyond what its role may run.
//!
//! The window and the CLI may use any session. The MCP bridge uses only
//! the ones it opened (see `mcp.rs`). The phone, too, uses only the
//! sessions it opened, and the terminals it kept so that it gets them back
//! after reconnecting: it is the one client on another device, so it must
//! not type into the window's shells, read its desktops or browse its file
//! panels, and the daemon decides that rather than the app. It guards
//! live sessions, not hosts: the phone may open its own on any host.

use super::Conn;
use crate::api::rpc::Role;
use crate::api::{SessionId, SessionKind, TransferId};
use crate::error::{Error, Result};

impl Conn {
    /// Whether this client opened `session`.
    pub(super) fn owns(&self, session: SessionId) -> bool {
        self.owned.lock().unwrap().contains(&session)
    }

    /// Refuses the phone a session it didn't open on this connection,
    /// unless it is a terminal the phone kept; other roles pass.
    pub(super) fn may_use(&self, session: SessionId) -> Result<()> {
        if self.role.get() != Some(&Role::Mobile) || self.owns(session) {
            return Ok(());
        }
        // A session that isn't running is refused here rather than by the
        // command, so an id can't become someone else's session in between.
        let info = self
            .core
            .sessions()
            .list()
            .into_iter()
            .find(|i| i.session == session)
            .ok_or(Error::SessionNotFound(session))?;
        if info.kind == SessionKind::Terminal && info.kept_by == Some(Role::Mobile) {
            return Ok(());
        }
        Err(Error::PermissionDenied(
            "the phone may only use sessions it opened and terminals it kept".to_string(),
        ))
    }

    /// Refuses the phone a transfer of a session it may not use. One that
    /// isn't running is refused too, so its id can't become another
    /// client's transfer in between.
    pub(super) fn may_cancel(&self, transfer: TransferId) -> Result<()> {
        if self.role.get() != Some(&Role::Mobile) {
            return Ok(());
        }
        let session = self
            .core
            .transfers()
            .session_of(transfer)
            .ok_or_else(|| Error::invalid("transfer", format!("{transfer} is not running")))?;
        self.may_use(session)
    }
}
