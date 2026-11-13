//! Joining a session that is already running. A background monitor is
//! one session that every client shares, and it may have said all it has
//! to say before a client arrives: a window opened after the daemon
//! started was not there when the monitor asked whether to install
//! burrow-agent, or said why it waits to try again. So when a client joins,
//! the session says where it stands once more.

use super::Sessions;
use crate::api::{Event, SessionId};

impl Sessions {
    /// Repeats where `id` stands: its state as last announced, then each
    /// question it still waits on, as the events that said so the first
    /// time. Clients that heard them already hear them twice.
    ///
    /// `send` is called with the sessions locked, so that what is repeated
    /// can't arrive after something newer the session said. It must not
    /// reach for the sessions itself.
    pub fn repeat(&self, id: SessionId, send: impl Fn(Event)) {
        let map = self.map.lock().unwrap();
        let Some(entry) = map.get(&id).filter(|e| e.input.is_some()) else {
            return;
        };
        send(Event::SessionChanged {
            session: id,
            node: entry.node,
            state: entry.state.clone(),
        });
        entry.prompts.each(|prompt| {
            send(Event::PromptRequested {
                session: id,
                prompt: prompt.clone(),
            })
        });
    }
}
