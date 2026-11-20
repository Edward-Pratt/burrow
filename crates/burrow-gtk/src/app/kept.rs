//! Kept terminals, such as the phone's: they run in the daemon with no
//! client attached. The window doesn't open them, but marks their host as
//! running and lets the user close them, so a forgotten shell doesn't run
//! on unseen.

use std::collections::{HashMap, HashSet};

use burrow_core::{Command, Event, NodeId, SessionId, SessionInfo, SessionState};
use relm4::ComponentSender;

use super::App;
use super::daemon::{execute, list_sessions};

/// Kept terminals by session, with their host.
#[derive(Default)]
pub(super) struct Kept {
    running: HashMap<SessionId, NodeId>,
    /// Sessions seen closing since the last list, which a list asked for
    /// before they closed may still name.
    closed: HashSet<SessionId>,
}

impl Kept {
    /// Takes the core's list: the kept terminals in it are all there are.
    pub(super) fn take(&mut self, sessions: &[SessionInfo]) {
        let closed = std::mem::take(&mut self.closed);
        self.running = sessions
            .iter()
            .filter(|s| s.kept && !closed.contains(&s.session))
            .map(|s| (s.session, s.node))
            .collect();
    }

    /// The hosts that have one.
    pub(super) fn hosts(&self) -> HashSet<NodeId> {
        self.running.values().copied().collect()
    }

    pub(super) fn on(&self, node: NodeId) -> Vec<SessionId> {
        let on_node = self.running.iter().filter(|(_, n)| **n == node);
        on_node.map(|(s, _)| *s).collect()
    }
}

impl App {
    /// Keeps the list of kept terminals current from session events. Events
    /// don't say whether a session is kept, so one someone else opened is
    /// looked up once it says it is connected, which keeping it says again.
    pub(super) fn kept_event(&mut self, event: &Event, sender: &ComponentSender<Self>) {
        let Event::SessionChanged { session, state, .. } = event else {
            return;
        };
        match state {
            SessionState::Closed { .. } => {
                self.kept.running.remove(session);
                self.kept.closed.insert(*session);
            }
            SessionState::Connected if self.tab_of(*session).is_none() && !self.looking_up => {
                self.looking_up = true;
                list_sessions(&self.core, sender);
            }
            _ => {}
        }
    }

    pub(super) fn close_kept(&self, node: NodeId, sender: &ComponentSender<Self>) {
        for session in self.kept.on(node) {
            execute(&self.core, sender, Command::CloseSession { session });
        }
    }
}
