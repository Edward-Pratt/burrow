//! The core's API for clients in other processes: the GTK window, the CLI
//! and the MCP bridge. They talk to `burrowd` over a Unix socket in JSON-RPC
//! 2.0, one message per line, as addons do over stdio.
//!
//! The socket is the trust boundary. Only processes running as the same
//! user may connect (checked with `SO_PEERCRED`), since they could read the
//! user's files anyway. Each client says which [`Role`] it plays when it
//! connects, and the server limits what it may do by that role.
//!
//! The client and the protocol's own types are in burrow-api, so that a
//! client off the desktop can use them; the daemon's side is here.

mod audit;
mod events;
mod lock;
mod server;
mod start;

pub use burrow_api::rpc::{BUILD, Role, ServerInfo};
pub(crate) use burrow_api::rpc::{Initialize, MAX_LINE, read_line};
pub use burrow_api::{Client, Events};
pub use lock::stop;
pub(crate) use lock::{LOCK, take_lock};
pub use server::serve;
pub(crate) use start::SOCKET;
pub use start::{connect, ensure_private_dir, runtime_dir};

/// JSON-RPC error codes.
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const SERVER_ERROR: i64 = -32000;

pub(crate) fn own_uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}
