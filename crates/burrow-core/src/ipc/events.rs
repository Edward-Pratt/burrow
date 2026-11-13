//! Which events each client hears, and passing them on.

use std::sync::{Arc, Mutex};

use serde_json::json;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{mpsc, watch};

use super::Role;
use crate::api::{Event, SessionId};

/// Passes events on as `event` notifications. A client that falls behind
/// is sent `resync` and should list everything again.
pub(super) async fn forward_events(
    mut events: tokio::sync::broadcast::Receiver<Event>,
    control: mpsc::Sender<String>,
    role: Role,
    owned: Arc<Mutex<Vec<SessionId>>>,
    mut alive: watch::Receiver<()>,
) {
    loop {
        let message = tokio::select! {
            event = events.recv() => match event {
                Ok(event) if may_see(role, &event, &owned.lock().unwrap()) => {
                    json!({"jsonrpc": "2.0", "method": "event", "params": event})
                }
                Ok(_) => continue,
                Err(RecvError::Lagged(_)) => json!({"jsonrpc": "2.0", "method": "resync"}),
                Err(RecvError::Closed) => return,
            },
            _ = alive.changed() => return,
        };
        tokio::select! {
            sent = control.send(message.to_string()) => if sent.is_err() { return },
            _ = alive.changed() => return,
        }
    }
}

/// The MCP bridge hears about the sidebar, its grants and how the sessions
/// it opened are doing, so it can tell why one closed. The rest is none of
/// its business. A session that ends before its opening command has
/// answered isn't counted as the bridge's yet; the bridge then only sees
/// it gone.
fn may_see(role: Role, event: &Event, owned: &[SessionId]) -> bool {
    match role {
        Role::Gui | Role::Cli => true,
        Role::Mcp => match event {
            Event::SessionChanged { session, .. } => owned.contains(session),
            _ => matches!(
                event,
                Event::NodeCreated { .. }
                    | Event::NodeUpdated { .. }
                    | Event::NodeDeleted { .. }
                    | Event::GrantsChanged { .. }
            ),
        },
    }
}
