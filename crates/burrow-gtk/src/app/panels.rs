//! A tab's panel: messages forwarded to it, and the start of its session.

use burrow_core::{Command, NodeId, SessionId};
use relm4::{ComponentSender, gtk};

use crate::{database, desktop, files, monitor, session};

use super::App;
use super::daemon::execute;
use super::tabs::{Panel, Tab, TabKey};

impl App {
    /// The core accepted the file panel's session.
    pub(super) fn files_started(
        &mut self,
        node: NodeId,
        session: SessionId,
        window: &gtk::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        match self.tabs.get_mut(&TabKey::Files(node)) {
            Some(Tab {
                panel: Panel::Files(panel),
                ..
            }) if panel.session.state == session::State::Starting => {
                panel.session.state = session::State::Running(session);
                for event in self.unclaimed.claim(session) {
                    self.session_event(event, window, sender);
                }
            }
            // The tab was closed while the session started.
            _ => execute(&self.core, sender, Command::CloseSession { session }),
        }
    }

    pub(super) fn files_msg(
        &mut self,
        node: NodeId,
        msg: files::Msg,
        window: &gtk::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        if let Some(Tab {
            panel: Panel::Files(panel),
            ..
        }) = self.tabs.get_mut(&TabKey::Files(node))
        {
            panel.update(msg, &self.core, window, sender);
        }
    }

    /// The core accepted the monitor's session.
    pub(super) fn monitor_started(
        &mut self,
        node: NodeId,
        session: SessionId,
        window: &gtk::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        match self.tabs.get_mut(&TabKey::Monitor(node)) {
            Some(Tab {
                panel: Panel::Monitor(panel),
                ..
            }) if panel.session.state == session::State::Starting => {
                panel.session.state = session::State::Running(session);
                // A background monitor may have connected long ago,
                // so no "connected" event is coming.
                panel.fetch_history(&self.core, sender);
                for event in self.unclaimed.claim(session) {
                    self.session_event(event, window, sender);
                }
            }
            // The tab was closed while the session started.
            _ => execute(&self.core, sender, Command::CloseSession { session }),
        }
    }

    pub(super) fn monitor_msg(
        &mut self,
        node: NodeId,
        msg: monitor::Msg,
        sender: &ComponentSender<Self>,
    ) {
        if let Some(Tab {
            panel: Panel::Monitor(panel),
            ..
        }) = self.tabs.get_mut(&TabKey::Monitor(node))
        {
            panel.update(msg, &self.core, sender);
        }
    }

    /// The core accepted the database tab's session.
    pub(super) fn database_started(
        &mut self,
        node: NodeId,
        session: SessionId,
        window: &gtk::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        match self.tabs.get_mut(&TabKey::Node(node)) {
            Some(Tab {
                panel: Panel::Database(panel),
                ..
            }) if panel.session.state == session::State::Starting => {
                panel.session.state = session::State::Running(session);
                for event in self.unclaimed.claim(session) {
                    self.session_event(event, window, sender);
                }
            }
            // The tab was closed while the session started.
            _ => execute(&self.core, sender, Command::CloseSession { session }),
        }
    }

    pub(super) fn database_msg(
        &mut self,
        node: NodeId,
        msg: database::Msg,
        window: &gtk::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        if let Some(Tab {
            panel: Panel::Database(panel),
            ..
        }) = self.tabs.get_mut(&TabKey::Node(node))
        {
            panel.update(msg, &self.core, window, sender);
        }
    }

    /// The core accepted the desktop tab's session.
    pub(super) fn desktop_started(
        &mut self,
        node: NodeId,
        session: SessionId,
        window: &gtk::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        match self.tabs.get_mut(&TabKey::Node(node)) {
            Some(Tab {
                panel: Panel::Desktop(panel),
                ..
            }) if panel.session.state == session::State::Starting => {
                panel.session.state = session::State::Running(session);
                panel.attach(&self.core, session);
                for event in self.unclaimed.claim(session) {
                    self.session_event(event, window, sender);
                }
            }
            // The tab was closed while the session started.
            _ => execute(&self.core, sender, Command::CloseSession { session }),
        }
    }

    pub(super) fn desktop_msg(&mut self, node: NodeId, msg: desktop::Msg) {
        if let Some(Tab {
            panel: Panel::Desktop(panel),
            ..
        }) = self.tabs.get_mut(&TabKey::Node(node))
        {
            panel.update(msg, &self.core);
        }
    }
}
