//! What the file panel, the monitor and the database tab share: a session
//! the core runs for the tab, and the page that stands in for the tab's
//! content until it connects, or after it ends.

use burrow_core::ipc::Client;
use burrow_core::{Command, Response, SessionId, SessionState};
use relm4::gtk::prelude::*;
use relm4::{ComponentSender, gtk};

use crate::app::{App, AppMsg};
use crate::files::printable;
use crate::runtime::runtime;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// The open command was sent; its session id isn't known yet.
    Starting,
    Running(SessionId),
    /// The last session ended. The user can start another.
    Closed,
}

/// A tab's messages about its session, wrapped in the tab's own.
#[derive(Clone, Debug)]
pub enum Msg {
    /// The core accepted the open command. The window takes it, since it
    /// knows whether the tab is still open, and the events that came first.
    Started(SessionId),
    /// The open command itself failed.
    StartFailed(String),
    /// The user asked for a new session after the last one ended.
    Reconnect,
}

/// A tab's session, and a stack that shows the tab's content once it is
/// connected and a message, with a Reconnect button once it has ended,
/// until then.
pub struct SessionPage {
    pub state: State,
    stack: gtk::Stack,
    message: gtk::Label,
    reconnect: gtk::Button,
}

impl SessionPage {
    /// The page around `content`. `on_reconnect` runs when the user clicks
    /// Reconnect, and should send [`Msg::Reconnect`].
    pub fn new(content: &impl IsA<gtk::Widget>, on_reconnect: impl Fn() + 'static) -> SessionPage {
        let message = gtk::Label::new(Some("Connecting…"));
        message.set_wrap(true);
        message.set_max_width_chars(60);
        message.set_selectable(true);
        let reconnect = gtk::Button::with_label("Reconnect");
        reconnect.connect_clicked(move |_| on_reconnect());
        reconnect.set_halign(gtk::Align::Center);
        reconnect.set_visible(false);
        let message_page = gtk::Box::new(gtk::Orientation::Vertical, 12);
        message_page.set_valign(gtk::Align::Center);
        message_page.append(&message);
        message_page.append(&reconnect);

        let stack = gtk::Stack::new();
        stack.add_named(content, Some("content"));
        stack.add_named(&message_page, Some("message"));
        stack.set_visible_child_name("message");
        SessionPage {
            state: State::Starting,
            stack,
            message,
            reconnect,
        }
    }

    /// The widget to put in the tab.
    pub fn widget(&self) -> &gtk::Stack {
        &self.stack
    }

    pub fn running(&self) -> Option<SessionId> {
        match self.state {
            State::Running(session) => Some(session),
            _ => None,
        }
    }

    /// Shows a message in place of the content.
    pub fn show_message(&self, text: &str, can_reconnect: bool) {
        // Where the content held the focus, hiding it would move the focus
        // to the selectable message, with all of it selected.
        let had_focus = self.stack.focus_child().is_some();
        self.message.set_label(text);
        self.reconnect.set_visible(can_reconnect);
        self.stack.set_visible_child_name("message");
        if can_reconnect && had_focus {
            self.reconnect.grab_focus();
        }
    }

    pub fn show_content(&self) {
        self.stack.set_visible_child_name("content");
    }

    /// Whether the content is shown, which it only is once connected.
    pub fn showing_content(&self) -> bool {
        self.stack.visible_child_name().as_deref() == Some("content")
    }

    /// Shows the session's progress, and its end with a way to reconnect.
    /// `ended` words the end, from its error. Returns whether the session
    /// connected, which each tab shows its own way.
    pub fn session_changed(
        &mut self,
        state: &SessionState,
        ended: impl FnOnce(Option<&str>) -> String,
    ) -> bool {
        match state {
            SessionState::Connecting { step } => {
                self.show_message(&format!("{step}…"), false);
                false
            }
            SessionState::Connected => true,
            SessionState::Closed { error } => {
                self.state = State::Closed;
                self.show_message(&ended(error.as_deref()), true);
                false
            }
        }
    }

    /// The open command failed, so no session is coming.
    pub fn start_failed(&mut self, error: &str) {
        self.state = State::Closed;
        self.show_message(&printable(error), true);
    }

    /// Gets ready for a new session, if the last one has ended. Returns
    /// whether the tab should send its open command again.
    pub fn reconnect(&mut self) -> bool {
        if self.state != State::Closed {
            return false;
        }
        self.state = State::Starting;
        self.show_message("Connecting…", false);
        true
    }
}

/// "The connection ended." or "The connection ended: why", for `what`
/// and the error a session closed with, which can come from the server.
pub fn ended(what: &str, error: Option<&str>) -> String {
    match error {
        Some(error) => format!("{what}: {}", printable(error)),
        None => format!("{what}."),
    }
}

/// Sends `command`, which opens a session, and hands its reply to `wrap`
/// as [`Msg::Started`] or [`Msg::StartFailed`].
pub fn start(
    core: &Client,
    command: Command,
    sender: &ComponentSender<App>,
    wrap: impl FnOnce(Msg) -> AppMsg + Send + 'static,
) {
    let (core, input) = (core.clone(), sender.input_sender().clone());
    runtime().spawn(async move {
        let msg = match core.execute(command).await {
            Ok(Response::Session { session }) => Msg::Started(session),
            Ok(other) => Msg::StartFailed(format!("unexpected reply {other:?}")),
            Err(e) => Msg::StartFailed(e.to_string()),
        };
        let _ = input.send(wrap(msg));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endings_say_why_without_control_characters() {
        assert_eq!(ended("The connection ended", None), "The connection ended.");
        assert_eq!(
            ended("The monitor stopped", Some("reset\x1b[2J")),
            "The monitor stopped: reset\u{fffd}[2J"
        );
    }
}
