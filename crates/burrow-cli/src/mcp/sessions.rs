//! The sessions the bridge opens for tools, and waiting for them.

use std::time::Duration;

use burrow_core::ipc::Events;
use burrow_core::{Command, Event, Response, SessionId, SessionInfo, SessionState};
use tokio::sync::broadcast::error::RecvError;

use super::{Bridge, Called, Kind, pretty};

/// How long a database or file session may take to connect.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the bridge waits for the event saying why a session closed.
const CLOSE_EVENT: Duration = Duration::from_secs(1);

impl Bridge {
    pub(super) async fn sessions_list(&self) -> Result<Vec<SessionInfo>, Called> {
        match self.execute(Command::ListSessions).await? {
            Response::Sessions { sessions } => Ok(sessions),
            _ => Ok(Vec::new()),
        }
    }

    /// A connected session of `kind` on the node, opened on first use.
    pub(super) async fn session(&mut self, arg: &str, kind: Kind) -> Result<SessionId, Called> {
        let node = self.node(arg).await?;
        if let Some(&session) = self.sessions.get(&(node.id, kind)) {
            let running = self.sessions_list().await?;
            if running
                .iter()
                .any(|s| s.session == session && s.state == SessionState::Connected)
            {
                return Ok(session);
            }
            self.sessions.remove(&(node.id, kind));
        }
        let command = match kind {
            Kind::Database => Command::OpenDatabase { node: node.id },
            Kind::Files => Command::OpenFiles { node: node.id },
            Kind::Monitor => Command::OpenMonitor { node: node.id },
        };
        // Subscribed first, so the event saying why it closed can't be missed.
        let mut events = self.client.subscribe();
        let session = match self.execute(command).await? {
            Response::Session { session } => session,
            other => return Err(Called::Failed(pretty(&other))),
        };
        self.wait(session, &mut events).await?;
        self.sessions.insert((node.id, kind), session);
        Ok(session)
    }

    /// Waits for a session to connect, by asking: the daemon only tells the
    /// bridge about sessions it already counts as the bridge's. `events`
    /// was subscribed before the session opened, and says why it closed.
    pub(super) async fn wait(&self, session: SessionId, events: &mut Events) -> Result<(), Called> {
        let waited = tokio::time::timeout(CONNECT_TIMEOUT, async {
            loop {
                let running = self.sessions_list().await?;
                match running
                    .iter()
                    .find(|s| s.session == session)
                    .map(|s| &s.state)
                {
                    Some(SessionState::Connected) => return Ok(()),
                    Some(SessionState::Connecting { .. }) => {}
                    _ => return Err(Called::Failed(closed_reason(events, session).await)),
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
        match waited.await {
            Ok(result) => result,
            Err(_) => {
                let _ = self.execute(Command::CloseSession { session }).await;
                Err(Called::Failed(
                    "it did not connect in time. It may be waiting for a password, a \
                     passphrase or a host key that only the user can give: ask them to \
                     connect to it once in Burrow."
                        .into(),
                ))
            }
        }
    }
}

/// Why a session closed while connecting, from its last event. It may
/// trail the listing that showed it gone, so this waits a moment for it.
async fn closed_reason(events: &mut Events, session: SessionId) -> String {
    let found = tokio::time::timeout(CLOSE_EVENT, async {
        loop {
            match events.recv().await {
                Ok(Event::SessionChanged {
                    session: s,
                    state: SessionState::Closed { error },
                    ..
                }) if s == session => return error,
                Ok(_) | Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => return None,
            }
        }
    });
    let error = found.await.ok().flatten();
    match error {
        Some(error) => format!("it closed while connecting: {error}"),
        None => "it closed while connecting; open it in Burrow to see why".into(),
    }
}
