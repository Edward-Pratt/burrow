//! `burrow daemon` and `burrow doctor`, which work without a running
//! daemon.

use std::io::Write;
use std::process::ExitCode;

use burrow_api::{Client, Role};
use burrow_core::doctor::Check;
use burrow_core::ipc;
use burrow_core::{Command, Response};

use crate::Result;
use crate::args::DaemonCmd;
use crate::text::clean;

pub async fn run(action: &DaemonCmd) -> Result<ExitCode> {
    let dir = ipc::runtime_dir()?;
    let socket = dir.join("burrowd.sock");
    match action {
        DaemonCmd::Run => unreachable!("handled in main"),
        DaemonCmd::Status => match Client::connect(&socket, Role::Cli).await {
            Ok(client) => {
                let sessions = match client.execute(Command::ListSessions).await? {
                    Response::Sessions { sessions } => sessions.len(),
                    _ => 0,
                };
                println!(
                    "burrowd {} is running in {} with {sessions} sessions",
                    client.info().build,
                    dir.display()
                );
                Ok(ExitCode::SUCCESS)
            }
            Err(_) => {
                println!("burrowd is not running in {}", dir.display());
                Ok(ExitCode::from(3))
            }
        },
        DaemonCmd::Stop => {
            if !ipc::stop(&dir).await? {
                println!("burrowd is not running in {}", dir.display());
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// One line per check, and a second with the fix where one failed. Fails
/// when Burrow can't run as things are; a missing optional piece doesn't.
pub async fn doctor() -> Result<ExitCode> {
    let checks = burrow_core::doctor::run().await;
    let width = checks.iter().map(|c| c.name.len()).max().unwrap_or(0);
    let mut out = std::io::stdout().lock();
    for check in &checks {
        let status = match (check.ok(), check.optional) {
            (true, _) => "ok  ",
            (false, true) => "warn",
            (false, false) => "FAIL",
        };
        writeln!(
            out,
            "{status}  {:width$}  {}",
            check.name,
            clean(&check.found)
        )?;
        if let Some(fix) = &check.fix {
            writeln!(out, "      {:width$}  fix: {}", "", clean(fix))?;
        }
    }
    Ok(if checks.iter().any(Check::blocks) {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}
