//! `burrow tunnel`: list, start and stop SSH tunnels.

use std::process::ExitCode;

use burrow_api::Client;
use burrow_core::{Command, Response, SSH_TUNNEL_KIND, SessionKind, SessionState};

use crate::args::TunnelCmd;
use crate::nodes::{find, nodes};
use crate::sessions::{answer_prompts, connected, session};
use crate::text::clean;
use crate::{Result, unexpected};

pub async fn run(client: &Client, action: TunnelCmd) -> Result<ExitCode> {
    match action {
        TunnelCmd::Ls => {
            let sessions = match client.execute(Command::ListSessions).await? {
                Response::Sessions { sessions } => sessions,
                other => return Err(unexpected(&other)),
            };
            for node in nodes(client).await? {
                if node.kind != SSH_TUNNEL_KIND {
                    continue;
                }
                let running = sessions
                    .iter()
                    .find(|s| s.node == node.id && s.kind == SessionKind::Tunnel);
                let state = match running.map(|s| &s.state) {
                    Some(SessionState::Connected) => "up".to_string(),
                    Some(SessionState::Connecting { step }) => {
                        format!("connecting: {}", clean(step))
                    }
                    Some(SessionState::Closed { .. }) | None => "stopped".to_string(),
                };
                println!("{}\t{state}\t{}", clean(&node.name), node.id);
            }
            Ok(ExitCode::SUCCESS)
        }
        TunnelCmd::Start { node } => {
            let node = find(client, &node).await?;
            let mut events = client.subscribe();
            let prompts = answer_prompts(client, node.id);
            let session = session(client, Command::StartTunnel { node: node.id }).await?;
            // Already up: starting it again returned the running session.
            let up = matches!(
                client.execute(Command::ListSessions).await?,
                Response::Sessions { sessions } if sessions.iter().any(|s| s.session == session && s.state == SessionState::Connected)
            );
            let ready = if up {
                Ok(())
            } else {
                connected(client, &mut events, session).await
            };
            prompts.abort();
            ready?;
            eprintln!("{} is up", clean(&node.name));
            Ok(ExitCode::SUCCESS)
        }
        TunnelCmd::Stop { node } => {
            let node = find(client, &node).await?;
            let sessions = match client.execute(Command::ListSessions).await? {
                Response::Sessions { sessions } => sessions,
                other => return Err(unexpected(&other)),
            };
            let Some(running) = sessions
                .iter()
                .find(|s| s.node == node.id && s.kind == SessionKind::Tunnel)
            else {
                return Err(format!("{} is not running", clean(&node.name)).into());
            };
            let command = Command::CloseSession {
                session: running.session,
            };
            client.execute(command).await?;
            Ok(ExitCode::SUCCESS)
        }
    }
}
