//! Session events and prompts, routed to the tab that owns the session.

use burrow_core::{
    Command, Event, Prompt, SSH_TUNNEL_KIND, SessionId, SessionInfo, SessionKind, SessionState,
};
use relm4::{ComponentSender, gtk};
use vte4::TerminalExt;

use crate::dialogs;
use crate::remote::{self};

use super::App;
use super::daemon::{execute, list_sessions};
use super::tabs::Panel;

impl App {
    pub(super) fn session_event(
        &mut self,
        event: Event,
        window: &gtk::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        let session = match &event {
            Event::SessionChanged { session, .. } | Event::PromptRequested { session, .. } => {
                *session
            }
            _ => return,
        };
        // A tunnel's events name the tunnel, so they need no claiming.
        if let Event::SessionChanged { node, state, .. } = &event
            && self
                .nodes
                .get(node)
                .is_some_and(|n| n.kind == SSH_TUNNEL_KIND)
        {
            self.tunnel_changed(*node, session, state.clone(), sender);
            return;
        }
        if let Event::PromptRequested { prompt, .. } = &event
            && let Some(node) = self.tunnel_of(session)
        {
            let prompt = self.for_tunnel(node, prompt);
            self.ask(session, &prompt, window, sender);
            return;
        }
        let Some(key) = self.tab_of(session) else {
            // A prompt from a session nobody here started, such as a
            // tunnel that started with Burrow before its first event
            // arrived. Look up whose it is.
            if matches!(event, Event::PromptRequested { .. }) && !self.looking_up {
                self.looking_up = true;
                list_sessions(&self.core, sender);
            }
            self.unclaimed.hold(session, event);
            return;
        };
        let state = match event {
            Event::SessionChanged { state, .. } => state,
            Event::PromptRequested { prompt, .. } => {
                return self.ask(session, &prompt, window, sender);
            }
            _ => return,
        };
        let Some(tab) = self.tabs.get_mut(&key) else {
            return;
        };
        match &mut tab.panel {
            Panel::Shell(remote) => {
                let text = remote::status_text(&state);
                if matches!(state, SessionState::Closed { .. }) {
                    remote.ending.status = Some(text.unwrap_or_default());
                    remote.finish_if_done();
                } else if let Some(text) = text {
                    remote.terminal.feed(text.as_bytes());
                }
            }
            Panel::Files(panel) => panel.session_changed(&state, &self.core, sender),
            Panel::Monitor(panel) => panel.session_changed(&state, &self.core, sender),
            Panel::Database(panel) => panel.session_changed(&state, &self.core, sender),
            Panel::Desktop(panel) => panel.session_changed(&state),
            Panel::Tunnel(_) | Panel::View(_) | Panel::Placeholder(_) | Panel::Terminal { .. } => {}
        }
    }

    pub(super) fn ask(
        &self,
        session: SessionId,
        prompt: &Prompt,
        window: &gtk::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        let (core, sender) = (self.core.clone(), sender.clone());
        let id = prompt.id;
        let dialog = dialogs::answer(window, prompt, move |answer| {
            let command = Command::AnswerPrompt {
                session,
                prompt: id,
                answer,
            };
            execute(&core, &sender, command);
        });
        self.prompts.borrow_mut().insert((session, id), dialog);
    }

    /// Takes the core's list of sessions: tunnels found there are tracked,
    /// and prompts they asked before we knew them are shown.
    pub(super) fn take_sessions(
        &mut self,
        sessions: Vec<SessionInfo>,
        window: &gtk::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        for info in sessions {
            if info.kind != SessionKind::Tunnel {
                continue;
            }
            let status = self.tunnels.entry(info.node).or_default();
            // Events already seen for this session are newer than the list.
            if status
                .session
                .as_ref()
                .is_none_or(|(s, _)| *s < info.session)
            {
                status.session = Some((info.session, info.state));
                status.stale = false;
            }
            for event in self.unclaimed.claim(info.session) {
                if let Event::PromptRequested { prompt, .. } = event {
                    let prompt = self.for_tunnel(info.node, &prompt);
                    self.ask(info.session, &prompt, window, sender);
                }
            }
        }
        self.refresh_tunnels(sender);
    }
}
