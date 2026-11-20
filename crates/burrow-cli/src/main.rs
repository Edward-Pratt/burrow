//! `burrow`: Burrow on the command line. Like the window, it drives the core
//! in `burrowd`, and starts one when none is running.

mod api;
mod args;
mod daemon;
mod grants;
mod mcp;
mod nodes;
mod proxy;
mod query;
mod sessions;
mod terminal;
mod text;
mod tty;
mod tunnel;

use std::error::Error;
use std::process::ExitCode;
use std::time::Duration;

use args::{Cli, Cmd, DaemonCmd, McpCmd};
use burrow_api::Role;
use burrow_core::Response;
use burrow_core::ipc;
use clap::Parser;
use text::clean;

type Result<T, E = Box<dyn Error>> = std::result::Result<T, E>;

fn main() -> ExitCode {
    burrow_core::daemon::run_if_asked();
    let cli = Cli::parse();
    if let Cmd::Daemon {
        action: DaemonCmd::Run,
    } = cli.command
    {
        burrow_core::daemon::main();
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => return fail(&e),
    };
    let code = runtime.block_on(run(cli.command));
    // Reading stdin blocks a thread that nothing can wake; don't wait for it.
    runtime.shutdown_timeout(Duration::from_millis(100));
    code.unwrap_or_else(|e| fail(&*e))
}

/// Errors can quote a server, such as an SSH or Postgres message, so their
/// control characters are escaped like any other text from one.
fn fail(error: &dyn Error) -> ExitCode {
    eprintln!("burrow: {}", clean(&error.to_string()));
    ExitCode::FAILURE
}

async fn run(command: Cmd) -> Result<ExitCode> {
    match &command {
        Cmd::Daemon { action } => return daemon::run(action).await,
        Cmd::Doctor => return daemon::doctor().await,
        Cmd::Proxy { role } => return Ok(proxy::run(*role).await?),
        Cmd::Mcp {
            action: McpCmd::Serve,
        } => return mcp::serve(ipc::connect(Role::Mcp).await?).await,
        _ => {}
    }
    let client = ipc::connect(Role::Cli).await?;
    if client.info().build != burrow_api::BUILD {
        eprintln!(
            "burrow: burrowd {} is still running from before an update; \
             stop what runs in it, then run `burrow daemon stop`",
            clean(&client.info().build)
        );
    }
    match command {
        Cmd::Ls { json } => nodes::ls(&client, json).await,
        Cmd::Ssh { node } => terminal::ssh(&client, &node).await,
        Cmd::Exec {
            node,
            timeout,
            command,
        } => terminal::exec(&client, &node, timeout, command.join(" ")).await,
        Cmd::Query {
            node,
            sql,
            write,
            limit,
            json,
        } => query::run(&client, &node, sql, write, limit, json).await,
        Cmd::Tunnel { action } => tunnel::run(&client, action).await,
        Cmd::Call { json } => api::call(&client, &json).await,
        Cmd::Events => api::events(&client).await,
        Cmd::Mcp { action } => grants::run(&client, action).await,
        Cmd::Daemon { .. } | Cmd::Doctor | Cmd::Proxy { .. } => unreachable!("handled above"),
    }
}

pub(crate) fn unexpected(response: &Response) -> Box<dyn Error> {
    format!("burrowd sent an unexpected reply: {response:?}").into()
}
