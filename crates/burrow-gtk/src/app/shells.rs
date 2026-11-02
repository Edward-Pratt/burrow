//! Shell tabs on SSH hosts: opening them, and their terminal traffic.

use std::collections::HashSet;

use burrow_core::{NodeId, SSH_HOST_KIND, SessionId, TerminalRun};
use relm4::gtk::prelude::*;
use relm4::{ComponentSender, gtk};

use crate::remote::{self, Remote};
use crate::sidebar;
use crate::terminal;

use super::chrome::send_on_click;
use super::message::AppMsg;
use super::tabs::{Panel, TabKey, shell_title};
use super::{App, Widgets};

use super::daemon::execute;
use super::tabs::Tab;
use crate::files;
use crate::session;
use burrow_core::Command;
use vte4::TerminalExt;
impl App {
    pub(super) fn shell(&mut self, key: TabKey) -> Option<&mut Remote> {
        match &mut self.tabs.get_mut(&key)?.panel {
            Panel::Shell(remote) => Some(remote),
            _ => None,
        }
    }

    /// The shell running `session`.
    pub(super) fn shell_running(&mut self, session: SessionId) -> Option<&mut Remote> {
        let key = self.tab_of(session)?;
        self.shell(key)
    }

    /// Opens a shell on the host `id`, or a tab running `run` there.
    pub(super) fn new_shell(
        &mut self,
        id: NodeId,
        run: Option<TerminalRun>,
        widgets: &Widgets,
        sender: &ComponentSender<Self>,
    ) {
        let Some(node) = self.nodes.get(&id).filter(|n| n.kind == SSH_HOST_KIND) else {
            return;
        };
        let tab = TabKey::Shell(id, self.next_tab);
        self.next_tab += 1;
        // The lowest number not shown on another of the host's shells.
        // Tabs that run something else are named after it instead.
        let taken: HashSet<u32> = (self.tabs.iter())
            .filter_map(|(key, t)| match (key, &t.panel) {
                (TabKey::Shell(node, _), Panel::Shell(remote)) if *node == id => {
                    Some(remote.number)
                }
                _ => None,
            })
            .collect();
        let mut number = if run.is_some() { 0 } else { 1 };
        while run.is_none() && taken.contains(&number) {
            number += 1;
        }
        let term = terminal::remote(
            {
                let sender = sender.clone();
                move |title| sender.input(AppMsg::RemoteTitle { tab, title })
            },
            {
                let sender = sender.clone();
                move |data| sender.input(AppMsg::RemoteInput { tab, data })
            },
            {
                let sender = sender.clone();
                move |cols, rows| sender.input(AppMsg::RemoteResize { tab, cols, rows })
            },
        );
        // The host's other tabs, a click away rather than in its context
        // menu. The buttons leave the focus in the terminal.
        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        bar.add_css_class("shell-bar");
        for (label, message) in [
            ("Files", AppMsg::OpenFiles(id)),
            ("Monitor", AppMsg::OpenMonitor(id)),
            ("New shell", AppMsg::NewShell(id)),
        ] {
            let button = gtk::Button::with_label(label);
            button.add_css_class("flat");
            button.set_focus_on_click(false);
            send_on_click(&button, sender, message);
            bar.append(&button);
        }
        let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
        page.append(&bar);
        page.append(&terminal::page(&term));
        terminal::set_font(&term, self.settings.terminal_font.as_deref());
        remote::start(&self.core, tab, id, &term, run.clone(), sender);
        term.grab_focus();
        let icon = sidebar::icon_name(node, false, &self.icons()).to_string();
        let remote = Remote::new(term, number, run);
        let title = shell_title(&node.name, number, remote.run.as_ref());
        let panel = Panel::Shell(remote);
        self.add_tab(tab, page.upcast(), panel, &icon, &title, widgets, sender);
    }

    /// The core accepted `open_terminal` for the shell tab `tab`.
    pub(super) fn session_started(
        &mut self,
        tab: TabKey,
        session: SessionId,
        window: &gtk::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        match self.tabs.get_mut(&tab) {
            Some(Tab {
                panel: Panel::Shell(remote),
                ..
            }) if remote.state == session::State::Starting => {
                remote.state = session::State::Running(session);
                // Sizes seen while starting had no session to go to.
                let (cols, rows) = terminal::size(&remote.terminal);
                execute(
                    &self.core,
                    sender,
                    Command::ResizeTerminal {
                        session,
                        cols,
                        rows,
                    },
                );
                for event in self.unclaimed.claim(session) {
                    self.session_event(event, window, sender);
                }
            }
            // The tab was closed while the session started.
            _ => execute(&self.core, sender, Command::CloseSession { session }),
        }
    }

    /// `open_terminal` itself failed for the shell tab `tab`.
    pub(super) fn session_failed(&mut self, tab: TabKey, error: &str) {
        if let Some(remote) = self.shell(tab) {
            remote.terminal.feed(remote::failure(error).as_bytes());
            remote.state = session::State::Closed;
        }
    }

    pub(super) fn session_output(&mut self, session: SessionId, data: &[u8]) {
        if let Some(remote) = self.shell_running(session) {
            remote.terminal.feed(data);
        }
    }

    pub(super) fn session_output_ended(&mut self, session: SessionId) {
        if let Some(remote) = self.shell_running(session) {
            remote.ending.output_ended = true;
            remote.finish_if_done();
        }
    }

    /// Typed into the shell tab `tab`. Enter in a shell that has ended
    /// starts it again.
    pub(super) fn remote_input(
        &mut self,
        tab: TabKey,
        data: Vec<u8>,
        sender: &ComponentSender<Self>,
    ) {
        let (
            Some(node),
            Some(Tab {
                panel: Panel::Shell(remote),
                ..
            }),
        ) = (tab.node(), self.tabs.get_mut(&tab))
        else {
            return;
        };
        match remote.state {
            session::State::Running(session) => {
                execute(&self.core, sender, Command::WriteTerminal { session, data })
            }
            session::State::Closed if data.contains(&b'\r') => {
                remote.terminal.feed(b"\r\n");
                remote.state = session::State::Starting;
                let run = remote.run.clone();
                remote::start(&self.core, tab, node, &remote.terminal, run, sender);
            }
            _ => {}
        }
    }

    pub(super) fn remote_resize(
        &self,
        tab: TabKey,
        cols: u16,
        rows: u16,
        sender: &ComponentSender<Self>,
    ) {
        let panel = self.tabs.get(&tab).map(|t| &t.panel);
        if let Some(Panel::Shell(Remote {
            state: session::State::Running(session),
            ..
        })) = panel
        {
            let command = Command::ResizeTerminal {
                session: *session,
                cols,
                rows,
            };
            execute(&self.core, sender, command);
        }
    }

    /// The title the shell on the SSH host set, as it arrived.
    pub(super) fn remote_title(&mut self, tab: TabKey, title: &str) {
        // VTE reports a title after the output that set it, which
        // can be after the shell has ended.
        if let Some(remote) = self.shell(tab)
            && matches!(remote.state, session::State::Running(_))
        {
            // The server sets it.
            remote.title = Some(files::printable(title));
        }
    }
}
