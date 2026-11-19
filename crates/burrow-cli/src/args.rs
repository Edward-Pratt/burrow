//! What `burrow` takes on its command line.

use burrow_api::Role;
use burrow_core::Capability;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "burrow", version, about = "Burrow on the command line")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Cmd,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// List the nodes in the sidebar.
    Ls {
        /// Print them as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Open a terminal on an SSH host.
    Ssh {
        /// The node's name or id.
        node: String,
    },
    /// Run a command on an SSH host and print its output. Exits with the
    /// command's status.
    Exec {
        /// The node's name or id.
        node: String,
        /// Seconds to wait, connecting included.
        #[arg(long, default_value_t = 60)]
        timeout: u32,
        /// The command, run by the remote user's shell as `ssh host cmd`
        /// runs it.
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
    },
    /// Run SQL on a database node and print the rows as tab-separated
    /// values.
    Query {
        /// The node's name or id.
        node: String,
        sql: String,
        /// Let the SQL write. Sessions start read-only.
        #[arg(long)]
        write: bool,
        /// Keep at most this many rows of each statement.
        #[arg(long)]
        limit: Option<u32>,
        /// Print the whole result as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Start, stop and list tunnels.
    Tunnel {
        #[command(subcommand)]
        action: TunnelCmd,
    },
    /// Run one API command, given as JSON, and print the response.
    Call {
        /// For example '{"type": "list_sessions"}'. See docs/API.md.
        json: String,
    },
    /// Print events as JSON lines until interrupted.
    Events,
    /// What an AI assistant may do through `burrow mcp serve`.
    Mcp {
        #[command(subcommand)]
        action: McpCmd,
    },
    /// The daemon.
    Daemon {
        #[command(subcommand)]
        action: DaemonCmd,
    },
    /// Check that this machine has what Burrow needs, and say what is
    /// missing and how to get it.
    Doctor,
    /// Pass the daemon's protocol between stdin and stdout, as an SSH key's
    /// forced command runs it for the phone. The first line must be
    /// `initialize` with this role.
    Proxy {
        /// gui, cli, mcp or mobile.
        #[arg(long)]
        role: Role,
    },
}

#[derive(Subcommand)]
pub enum TunnelCmd {
    /// List tunnels and whether they run.
    Ls,
    /// Start a tunnel and wait until it is up.
    Start { node: String },
    /// Stop a running tunnel.
    Stop { node: String },
}

#[derive(Subcommand)]
pub enum McpCmd {
    /// Serve MCP on stdin and stdout, for an assistant to start. For Claude
    /// Code: `claude mcp add burrow -- burrow mcp serve`.
    Serve,
    /// List what has been granted.
    Ls,
    /// Let the assistant do more on a node. Shows the node first: the
    /// grant only holds while it stays as shown.
    Grant {
        node: String,
        /// sql-read, sql-write, exec, files-read, files-write, tunnel or monitor-read.
        #[arg(required = true, value_parser = capability)]
        capabilities: Vec<Capability>,
        /// Don't ask.
        #[arg(long, short)]
        yes: bool,
    },
    /// Take some or all of it away.
    Revoke {
        node: String,
        /// What to take away; everything when none are given.
        #[arg(value_parser = capability)]
        capabilities: Vec<Capability>,
    },
}

fn capability(name: &str) -> std::result::Result<Capability, String> {
    Capability::ALL
        .into_iter()
        .find(|c| c.to_string() == name)
        .ok_or_else(|| {
            let names: Vec<String> = Capability::ALL.iter().map(|c| c.to_string()).collect();
            format!("not one of {}", names.join(", "))
        })
}

#[derive(Subcommand)]
pub enum DaemonCmd {
    /// Run the daemon in the foreground, as the systemd unit does.
    Run,
    /// Say whether a daemon is running.
    Status,
    /// Stop the running daemon. Its tunnels stop with it.
    Stop,
}
