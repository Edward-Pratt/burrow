//! Burrow's core: the sidebar tree stored as TOML files, secrets kept in the
//! system keyring, and the command/event API that every client (the GTK UI,
//! addons, the CLI, the MCP bridge) uses to drive it.
//!
//! This crate must not depend on any UI toolkit. Everything here is `Send`
//! so it can run behind a GTK main loop today and inside `burrowd` later.

mod api;
mod engine;
mod error;
mod secrets;
mod tree;
mod watch;

pub use api::{
    API_VERSION, Command, Event, FOLDER_KIND, Node, NodeId, NodePatch, Problem, Response,
    SecretValue,
};
pub use engine::{Core, CoreOptions};
pub use error::{Error, Result};
pub use secrets::{KeyringStore, MemoryStore, SecretKey, SecretStore};
pub use watch::Watcher;
