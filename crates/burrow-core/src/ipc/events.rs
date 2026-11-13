//! Which events each client hears, and passing them on.

use serde_json::json;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{mpsc, watch};

use super::Role;
use crate::api::Event;

/// Passes events on as `event` notifications. A client that falls behind
/// is sent `resync` and should list everything again.
pub(super) async fn forward_events(
    mut events: tokio::sync::broadcast::Receiver<Event>,
    control: mpsc::Sender<String>,
    role: Role,
    mut alive: watch::Receiver<()>,
) {
    loop {
        let message = tokio::select! {
            event = events.recv() => match event {
                Ok(event) if may_see(role, &event) => {
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

/// The MCP bridge hears about the sidebar and its grants. It follows its
/// own sessions by listing them, and the rest is none of its business.
fn may_see(role: Role, event: &Event) -> bool {
    match role {
        Role::Gui | Role::Cli => true,
        Role::Mcp => matches!(
            event,
            Event::NodeCreated { .. }
                | Event::NodeUpdated { .. }
                | Event::NodeDeleted { .. }
                | Event::GrantsChanged { .. }
        ),
    }
}
