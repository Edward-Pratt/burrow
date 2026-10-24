//! SSH host tabs: a [`terminal::remote`] terminal showing a session the core
//! runs. The tab outlives its sessions: when one ends, Enter starts another.

use std::collections::HashMap;

use burrow_core::{Command, Core, Event, NodeId, Response, SessionId, SessionState};
use relm4::ComponentSender;
use vte4::TerminalExt;

use crate::app::{App, AppMsg};
use crate::runtime::runtime;
use crate::terminal;

/// Output is passed to the window in chunks of up to this many bytes, so a
/// burst becomes a few large feeds instead of thousands of small ones.
const OUTPUT_BATCH: usize = 64 * 1024;

/// Sessions whose events can wait for their tab before being dropped.
const MAX_UNCLAIMED: usize = 32;

pub struct Remote {
    pub terminal: vte4::Terminal,
    pub state: State,
    /// The running session's end, seen from both sides. Its `closed` event
    /// and the end of its output arrive on different tasks, so the "closed"
    /// message waits until the last output is shown.
    pub ending: Ending,
}

#[derive(Default)]
pub struct Ending {
    /// What to show once the output has ended too.
    pub status: Option<String>,
    pub output_ended: bool,
}

impl Remote {
    pub fn new(terminal: vte4::Terminal) -> Remote {
        Remote {
            terminal,
            state: State::Starting,
            ending: Ending::default(),
        }
    }

    /// Shows the closing message and allows a reconnect once the session
    /// has both reported `closed` and finished its output.
    pub fn finish_if_done(&mut self) {
        if self.ending.output_ended
            && let Some(status) = self.ending.status.take()
        {
            self.terminal.feed(status.as_bytes());
            self.state = State::Closed;
            self.ending = Ending::default();
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// `open_terminal` was sent; its session id isn't known yet.
    Starting,
    Running(SessionId),
    /// The last session ended. Enter starts a new one.
    Closed,
}

/// Session events that arrived before the tab knew which session was its
/// own. Events come from the core's broadcast, and the session id from the
/// `open_terminal` reply, on different tasks, so either can arrive first.
#[derive(Default)]
pub struct Unclaimed(HashMap<SessionId, Vec<Event>>);

impl Unclaimed {
    pub fn hold(&mut self, session: SessionId, event: Event) {
        // Sessions opened by some other client are never claimed.
        if self.0.len() >= MAX_UNCLAIMED && !self.0.contains_key(&session) {
            self.0.clear();
        }
        self.0.entry(session).or_default().push(event);
    }

    pub fn claim(&mut self, session: SessionId) -> Vec<Event> {
        self.0.remove(&session).unwrap_or_default()
    }
}

/// Asks the core for a session on `node`. The reply arrives as
/// [`AppMsg::SessionStarted`], followed by the session's output.
pub fn start(core: &Core, node: NodeId, terminal: &vte4::Terminal, sender: &ComponentSender<App>) {
    let (cols, rows) = terminal::size(terminal);
    let (core, input) = (core.clone(), sender.input_sender().clone());
    runtime().spawn(async move {
        let command = Command::OpenTerminal { node, cols, rows };
        let session = match core.execute(command).await {
            Ok(Response::Session { session }) => session,
            Ok(other) => {
                let error = format!("unexpected reply {other:?}");
                let _ = input.send(AppMsg::SessionFailed { node, error });
                return;
            }
            Err(e) => {
                let error = e.to_string();
                let _ = input.send(AppMsg::SessionFailed { node, error });
                return;
            }
        };
        if input
            .send(AppMsg::SessionStarted { node, session })
            .is_err()
        {
            return;
        }
        if let Ok(mut output) = core.take_output(session) {
            while let Some(mut data) = output.recv().await {
                while data.len() < OUTPUT_BATCH {
                    match output.try_recv() {
                        Ok(more) => data.extend(more),
                        Err(_) => break,
                    }
                }
                // The window is gone. Dropping `output` ends the session.
                if input.send(AppMsg::SessionOutput { session, data }).is_err() {
                    return;
                }
            }
        }
        let _ = input.send(AppMsg::SessionOutputEnded { session });
    });
}

/// What to show in the terminal when a session changes state.
pub fn status_text(state: &SessionState) -> Option<String> {
    match state {
        SessionState::Connecting { step } => Some(dim(&format!("{step}…"))),
        SessionState::Connected => None,
        SessionState::Closed { error: None } => Some(format!(
            "\r\n{}",
            dim("Connection closed. Press Enter to reconnect.")
        )),
        SessionState::Closed { error: Some(error) } => Some(failure(error)),
    }
}

pub fn failure(error: &str) -> String {
    format!(
        "\r\n\x1b[1;31m{}\x1b[0m\r\n{}",
        printable(error),
        dim("Press Enter to try again.")
    )
}

fn dim(text: &str) -> String {
    format!("\x1b[2m{}\x1b[0m\r\n", printable(text))
}

/// Error text can include what a server sent. Escape sequences in it must
/// not reach the terminal, and newlines need a carriage return.
fn printable(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || *c == '\n')
        .collect::<String>()
        .replace('\n', "\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_sequences_are_removed() {
        assert_eq!(
            printable("bad\x1b]0;title\x07 key\nnext"),
            "bad]0;title key\r\nnext"
        );
    }

    #[test]
    fn unclaimed_events_wait_for_their_session() {
        let mut unclaimed = Unclaimed::default();
        let event = |n| Event::SessionChanged {
            session: SessionId(n),
            node: NodeId::new(),
            state: SessionState::Connected,
        };
        unclaimed.hold(SessionId(1), event(1));
        unclaimed.hold(SessionId(1), event(1));
        unclaimed.hold(SessionId(2), event(2));
        assert_eq!(unclaimed.claim(SessionId(1)).len(), 2);
        assert!(unclaimed.claim(SessionId(1)).is_empty());
        assert_eq!(unclaimed.claim(SessionId(2)).len(), 1);
    }
}
