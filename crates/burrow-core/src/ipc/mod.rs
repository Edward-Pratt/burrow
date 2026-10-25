//! The core's API for clients in other processes: the GTK window, the CLI
//! and the MCP bridge. They talk to `burrowd` over a Unix socket in JSON-RPC
//! 2.0, one message per line, as addons do over stdio.
//!
//! The socket is the trust boundary. Only processes running as the same
//! user may connect (checked with `SO_PEERCRED`), since they could read the
//! user's files anyway. Each client says which [`Role`] it plays when it
//! connects, and the server limits what it may do by that role.

mod client;
mod server;
mod start;

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

pub use client::{Client, Events};
pub use server::serve;
pub(crate) use start::{LOCK, SOCKET};
pub use start::{connect, ensure_private_dir, runtime_dir, stop};

/// The longest line either side accepts. Lines are long when a query
/// returns many rows, so this is generous; it only stops a runaway peer
/// from making the other buffer without limit.
const MAX_LINE: usize = 64 << 20;

/// JSON-RPC error codes.
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const SERVER_ERROR: i64 = -32000;

/// What a client is, which decides what it may do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// The GTK window.
    Gui,
    /// `burrow` on the command line, used by a person or a script.
    Cli,
    /// The MCP bridge, through which an AI assistant acts.
    Mcp,
}

/// What a client sends first.
#[derive(Debug, Serialize, Deserialize)]
struct Initialize {
    api: u32,
    role: Role,
    build: String,
}

/// What the server answers to [`Initialize`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServerInfo {
    pub api: u32,
    /// The daemon's version, so a client newer than a daemon still running
    /// from before an upgrade can tell.
    pub build: String,
    /// The folder holding addon folders, for clients that offer to open it.
    pub addons_dir: PathBuf,
}

/// This build's version, compared between client and daemon.
pub const BUILD: &str = env!("CARGO_PKG_VERSION");

pub(crate) fn own_uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

/// Reads one line, without its `\n`, into `line`. `Ok(None)` at the end of
/// input. A line longer than `max` is an error, found before it is all
/// buffered.
pub(crate) async fn read_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    line: &mut Vec<u8>,
    max: usize,
) -> Result<Option<()>, String> {
    line.clear();
    loop {
        let available = reader
            .fill_buf()
            .await
            .map_err(|e| format!("reading from it failed: {e}"))?;
        if available.is_empty() {
            return Ok(if line.is_empty() { None } else { Some(()) });
        }
        let (taken, done) = match available.iter().position(|&b| b == b'\n') {
            Some(i) => (i, true),
            None => (available.len(), false),
        };
        if line.len() + taken > max {
            return Err(format!("it sent a message over {} bytes", max));
        }
        line.extend_from_slice(&available[..taken]);
        reader.consume(if done { taken + 1 } else { taken });
        if done {
            return Ok(Some(()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reads_lines_up_to_the_limit() {
        let mut input: &[u8] = b"one\ntwo\nthree";
        let mut line = Vec::new();
        for expected in ["one", "two", "three"] {
            assert_eq!(read_line(&mut input, &mut line, 8).await, Ok(Some(())));
            assert_eq!(line, expected.as_bytes());
        }
        assert_eq!(read_line(&mut input, &mut line, 8).await, Ok(None));

        let mut input: &[u8] = b"123456789\n";
        assert!(read_line(&mut input, &mut line, 8).await.is_err());
    }
}
