//! Opening a session and waiting for it, while answering on the terminal
//! what it asks, such as a host key or a password.

use burrow_api::{Client, Events};
use burrow_core::{Command, Event, NodeId, Response, SessionId, SessionState};
use tokio::sync::broadcast::error::RecvError;

use crate::tty;
use crate::{Result, unexpected};

/// Answers, on the terminal, the questions sessions on `node` ask, until
/// the returned task is aborted.
pub fn answer_prompts(client: &Client, node: NodeId) -> tokio::task::JoinHandle<()> {
    let mut events = client.subscribe();
    let client = client.clone();
    tokio::spawn(async move {
        loop {
            let (session, prompt) = match events.recv().await {
                Ok(Event::PromptRequested { session, prompt }) => (session, prompt),
                Ok(_) | Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => return,
            };
            let ours = match client.execute(Command::ListSessions).await {
                Ok(Response::Sessions { sessions }) => sessions
                    .iter()
                    .any(|s| s.session == session && s.node == node),
                _ => false,
            };
            if !ours {
                continue;
            }
            let id = prompt.id;
            let asked = tokio::task::spawn_blocking(move || tty::ask(&prompt));
            if let Ok(Some(answer)) = asked.await {
                // Fails when another client answered first, which is fine.
                let command = Command::AnswerPrompt {
                    session,
                    prompt: id,
                    answer,
                };
                let _ = client.execute(command).await;
            }
        }
    })
}

/// Waits for `session` to connect. Its error when it closes instead.
pub async fn connected(client: &Client, events: &mut Events, session: SessionId) -> Result<()> {
    loop {
        match events.recv().await {
            Ok(Event::SessionChanged {
                session: s, state, ..
            }) if s == session => match state {
                SessionState::Connected => return Ok(()),
                SessionState::Closed { error } => {
                    return Err(error.unwrap_or_else(|| "it closed".to_string()).into());
                }
                SessionState::Connecting { .. } => {}
            },
            Ok(_) => {}
            // A missed event may have been ours: ask how the session is.
            Err(RecvError::Lagged(_)) => {
                let sessions = match client.execute(Command::ListSessions).await? {
                    Response::Sessions { sessions } => sessions,
                    other => return Err(unexpected(&other)),
                };
                match sessions.iter().find(|s| s.session == session) {
                    Some(info) if info.state == SessionState::Connected => return Ok(()),
                    Some(_) => {}
                    None => return Err("it closed".into()),
                }
            }
            Err(RecvError::Closed) => return Err("lost the connection to burrowd".into()),
        }
    }
}

pub async fn session(client: &Client, command: Command) -> Result<SessionId> {
    match client.execute(command).await? {
        Response::Session { session } => Ok(session),
        other => Err(unexpected(&other)),
    }
}
