//! VNC desktop tabs. The core signs in to the server and passes its RFB
//! stream through, as if from a server that needs no password, so the
//! window speaks RFB itself with vnc-rs ([`rfb`]) and draws the screen
//! ([`frame`]).

mod dialog;
mod frame;
mod input;
mod pointer;
mod rfb;

use std::rc::Rc;
use std::time::Duration;

use burrow_core::ipc::Client;
use burrow_core::{Command, NodeId, SessionId, SessionState};
use relm4::gtk::prelude::*;
use relm4::gtk::{gdk, glib};
use relm4::{ComponentSender, gtk};
use tokio::sync::mpsc;
use vnc::VncEvent;

use crate::app::{App, AppMsg};
use crate::runtime::runtime;
use crate::session::{self, SessionPage};

pub use dialog::edit;
use frame::{Frame, PIXEL};
use input::{Shared, add_input};
use rfb::drive;

/// The least time between two redraws.
const REDRAW: Duration = Duration::from_millis(16);

#[derive(Clone, Debug)]
pub enum Msg {
    Session(session::Msg),
    /// What the server sent.
    Event(VncEvent),
    /// The RFB side ended, with why if it failed.
    Ended(Option<String>),
    /// Time to draw what changed.
    Draw,
}

/// One VNC desktop's tab.
pub struct Desktop {
    node: NodeId,
    pub page: gtk::Box,
    pub session: SessionPage,
    picture: gtk::Picture,
    frame: Frame,
    /// Whether a redraw is due.
    dirty: bool,
    shared: Rc<Shared>,
    sender: ComponentSender<App>,
}

impl Desktop {
    pub fn new(node: NodeId, sender: &ComponentSender<App>) -> Desktop {
        let picture = gtk::Picture::new();
        picture.set_content_fit(gtk::ContentFit::Contain);
        picture.set_can_shrink(true);
        picture.set_hexpand(true);
        picture.set_vexpand(true);
        picture.set_focusable(true);
        picture.add_css_class("desktop");
        let shared = Rc::new(Shared::default());
        add_input(&picture, &shared);

        let reconnect = {
            let sender = sender.clone();
            move || {
                let msg = Msg::Session(session::Msg::Reconnect);
                sender.input(AppMsg::Desktop { node, msg });
            }
        };
        let session = SessionPage::new(&picture, reconnect);
        let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
        page.append(session.widget());
        Desktop {
            node,
            page,
            session,
            picture,
            frame: Frame::default(),
            dirty: false,
            shared,
            sender: sender.clone(),
        }
    }

    pub fn session_changed(&mut self, state: &SessionState) {
        let was_connected = self.session.showing_content();
        let ended = |error: Option<&str>| match (error, was_connected) {
            (Some(_), false) => session::ended("Couldn't connect", error),
            _ => session::ended("The desktop closed", error),
        };
        if self.session.session_changed(state, ended) {
            self.session.show_content();
            self.picture.grab_focus();
        }
        if matches!(state, SessionState::Closed { .. }) {
            self.shared.input.replace(None);
        }
    }

    /// Starts speaking RFB over `session`, once the core has it.
    pub fn attach(&mut self, core: &Client, session: SessionId) {
        let (input, events) = mpsc::unbounded_channel();
        self.shared.input.replace(Some(input));
        let (core, sender, node) = (core.clone(), self.sender.input_sender().clone(), self.node);
        runtime().spawn(async move {
            let error = drive(&core, session, events, &sender, node).await.err();
            let _ = sender.send(AppMsg::Desktop {
                node,
                msg: Msg::Ended(error),
            });
        });
    }

    pub fn update(&mut self, msg: Msg, core: &Client) {
        match msg {
            // Handled by the window, which knows the other sessions.
            Msg::Session(session::Msg::Started(_)) => {}
            Msg::Session(session::Msg::StartFailed(error)) => self.session.start_failed(&error),
            Msg::Session(session::Msg::Reconnect) => {
                if self.session.reconnect() {
                    start(core, self.node, &self.sender);
                }
            }
            Msg::Event(event) => self.event(event),
            Msg::Ended(error) => {
                // The core's session closing ends this too, and says why.
                // Ending first, the RFB side failed: say so, and close it.
                if let (Some(session), Some(error)) = (self.session.running(), error) {
                    self.session.state = session::State::Closed;
                    let text = session::ended("The desktop stopped", Some(&error));
                    self.session.show_message(&text, true);
                    self.shared.input.replace(None);
                    let core = core.clone();
                    runtime().spawn(async move {
                        let _ = core.execute(Command::CloseSession { session }).await;
                    });
                }
            }
            Msg::Draw => {
                self.dirty = false;
                self.draw();
            }
        }
    }

    fn event(&mut self, event: VncEvent) {
        match event {
            VncEvent::SetResolution(screen) => {
                self.frame.resize(screen.width, screen.height);
                self.shared.size.set((screen.width, screen.height));
            }
            VncEvent::RawImage(rect, data) => self.frame.put(rect, &data),
            VncEvent::Copy(to, from) => self.frame.copy(to, from),
            VncEvent::Bell => self.picture.error_bell(),
            _ => return,
        }
        if !self.dirty {
            self.dirty = true;
            let (sender, node) = (self.sender.clone(), self.node);
            glib::timeout_add_local_once(REDRAW, move || {
                sender.input(AppMsg::Desktop {
                    node,
                    msg: Msg::Draw,
                });
            });
        }
    }

    fn draw(&self) {
        let (width, height) = (self.frame.width, self.frame.height);
        if width == 0 || height == 0 {
            return;
        }
        let bytes = glib::Bytes::from(&self.frame.pixels[..]);
        let texture = gdk::MemoryTexture::new(
            width.into(),
            height.into(),
            gdk::MemoryFormat::B8g8r8x8,
            &bytes,
            usize::from(width) * PIXEL,
        );
        self.picture.set_paintable(Some(&texture));
    }
}

/// Asks the core for a desktop session on `node`.
pub fn start(core: &Client, node: NodeId, sender: &ComponentSender<App>) {
    let wrap = move |msg| AppMsg::Desktop {
        node,
        msg: Msg::Session(msg),
    };
    session::start(core, Command::OpenVnc { node }, sender, wrap);
}
