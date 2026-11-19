//! What the daemon's socket speaks besides the API types: JSON-RPC 2.0,
//! one message per line, which opens with [`Initialize`] naming the
//! client's [`Role`]. The daemon's side is in burrow-core; the client's is
//! [`Client`](crate::Client).

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

/// The longest line either side accepts. Lines are long when a query
/// returns many rows, so this is generous; it only stops a runaway peer
/// from making the other buffer without limit.
pub const MAX_LINE: usize = 64 << 20;

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

impl Role {
    pub const ALL: [Role; 3] = [Role::Gui, Role::Cli, Role::Mcp];

    /// The role's name on the wire, as `initialize` sends it.
    pub fn name(self) -> &'static str {
        match self {
            Role::Gui => "gui",
            Role::Cli => "cli",
            Role::Mcp => "mcp",
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Role {
    type Err = String;

    fn from_str(name: &str) -> Result<Role, String> {
        Role::ALL
            .into_iter()
            .find(|role| role.name() == name)
            .ok_or_else(|| {
                let names: Vec<&str> = Role::ALL.iter().map(|role| role.name()).collect();
                format!("not one of {}", names.join(", "))
            })
    }
}

/// What a client sends first, as the params of `initialize`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Initialize {
    /// The [`API_VERSION`](crate::API_VERSION) the client speaks.
    pub api: u32,
    pub role: Role,
    /// The client's [`BUILD`].
    pub build: String,
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

/// This build's version, compared between client and daemon. Builds from
/// main all share the crate version, so the install script and the package
/// set `BURROW_BUILD` to one naming the commit, or an update would leave an
/// old daemon looking current.
pub const BUILD: &str = match option_env!("BURROW_BUILD") {
    Some(build) => build,
    None => env!("CARGO_PKG_VERSION"),
};

/// Reads one line, without its `\n`, into `line`. `Ok(None)` at the end of
/// input. A line longer than `max` is an error, found before it is all
/// buffered.
pub async fn read_line<R: AsyncBufRead + Unpin>(
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

    #[test]
    fn roles_are_named_as_on_the_wire() {
        for role in Role::ALL {
            let json = serde_json::to_value(role).unwrap();
            assert_eq!(json, role.name());
            assert_eq!(role.name().parse::<Role>(), Ok(role));
        }
        assert!(
            "root"
                .parse::<Role>()
                .unwrap_err()
                .contains("gui, cli, mcp")
        );
    }
}
