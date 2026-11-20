//! `burrow ssh` and `burrow exec`: a terminal on a host, and one command
//! run there.

use std::io::Write;
use std::process::ExitCode;
use std::time::Duration;

use burrow_api::Client;
use burrow_core::{Command, Response};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::nodes::find;
use crate::sessions::{answer_prompts, connected, session};
use crate::text::clean;
use crate::tty;
use crate::{Result, unexpected};

pub async fn ssh(client: &Client, node: &str) -> Result<ExitCode> {
    let node = find(client, node).await?;
    let (cols, rows) = tty::size();
    let mut events = client.subscribe();
    let prompts = answer_prompts(client, node.id);
    let command = Command::OpenTerminal {
        node: node.id,
        cols,
        rows,
        run: None,
    };
    let session = session(client, command).await?;
    let mut output = client.take_output(session).await?;
    let ready = connected(client, &mut events, session).await;
    prompts.abort();
    ready?;

    let raw = tty::Mode::raw();
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut resized =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?;
    let mut typed = [0u8; 4096];
    loop {
        tokio::select! {
            data = output.recv() => match data {
                Some(data) => {
                    stdout.write_all(&data).await?;
                    stdout.flush().await?;
                }
                None => break,
            },
            read = stdin.read(&mut typed) => match read {
                Ok(0) | Err(_) => {
                    let _ = client.execute(Command::CloseSession { session }).await;
                    break;
                }
                Ok(n) => {
                    let data = typed[..n].to_vec();
                    client.execute(Command::WriteTerminal { session, data }).await?;
                }
            },
            _ = resized.recv() => {
                let (cols, rows) = tty::size();
                let _ = client.execute(Command::ResizeTerminal { session, cols, rows }).await;
            }
        }
    }
    drop(raw);
    // The reason it ended, when it was an error.
    let ended = tokio::time::timeout(
        Duration::from_secs(1),
        connected(client, &mut events, session),
    )
    .await;
    match ended {
        Ok(Err(error)) if error.to_string() != "it closed" => Err(error),
        _ => Ok(ExitCode::SUCCESS),
    }
}

pub async fn exec(client: &Client, node: &str, timeout: u32, command: String) -> Result<ExitCode> {
    let node = find(client, node).await?;
    let prompts = answer_prompts(client, node.id);
    let command = Command::Exec {
        node: node.id,
        command,
        timeout: Some(timeout),
    };
    let result = client.execute(command).await;
    prompts.abort();
    let result = match result? {
        Response::Exec { result } => result,
        other => return Err(unexpected(&other)),
    };
    // The command's own output, passed on as `ssh host cmd` would.
    std::io::stdout().write_all(result.stdout.as_bytes())?;
    std::io::stderr().write_all(result.stderr.as_bytes())?;
    if result.truncated {
        eprintln!("burrow: output was cut at 1 MiB");
    }
    if result.timed_out {
        eprintln!("burrow: the command was still running after {timeout} s");
        // What timeout(1) exits with.
        return Ok(ExitCode::from(124));
    }
    Ok(match (result.status, result.signal) {
        (Some(status), _) => ExitCode::from(u8::try_from(status).unwrap_or(255)),
        (None, Some(signal)) => {
            eprintln!("burrow: the command was ended by {}", clean(&signal));
            ExitCode::from(255)
        }
        (None, None) => ExitCode::from(255),
    })
}
