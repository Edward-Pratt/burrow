//! Burrow's core: the sidebar tree stored as TOML files, secrets kept in the
//! system keyring, and the command/event API that every client (the GTK UI,
//! addons, the CLI, the MCP bridge) uses to drive it.
//!
//! This crate must not depend on any UI toolkit. Everything here is `Send`
//! so it can run behind a GTK main loop today and inside `burrowd` later.

mod addon;
mod class;
pub mod daemon;
mod db;
pub mod doctor;
mod engine;
mod error;
mod grants;
pub mod ipc;
mod net;
mod notify;
mod password;
mod pins;
mod secrets;
mod ssh;
mod tree;
mod vnc;
mod watch;

/// The API types live in burrow-api, which clients off the desktop can
/// build. The core reaches them as `crate::api`, as before they moved.
use burrow_api as api;

pub use api::{
    API_VERSION, AddonAction, AddonCommand, AddonInfo, AddonKind, AddonState, Alert, AlertId,
    AlertKind, Block, Button, Capability, Command, DbColumn, DbSchema, DbTable, DbTableKind, Event,
    ExecResult, FOLDER_KIND, Field, FileEntry, FileKind, FormField, FormInput, FormValue, Grant,
    GrantShown, HistoryEntry, MonitorHistory, Node, NodeId, NodePatch, POSTGRES_KIND, Problem,
    Prompt, PromptAnswer, PromptKind, QueryError, QueryResult, Response, SSH_HOST_KIND,
    SSH_TUNNEL_KIND, SecretValue, Service, ServiceAction, SessionId, SessionInfo, SessionKind,
    SessionState, StatementResult, TerminalRun, TransferId, TransferState, TrendRange, TunnelShown,
    VNC_KIND, View, monitor, sha256_hex,
};
pub use engine::{Core, CoreOptions, Paths};
pub use error::{Error, Result};
pub use secrets::{KeyringStore, MemoryStore, SecretKey, SecretStore};
pub use watch::Watcher;
