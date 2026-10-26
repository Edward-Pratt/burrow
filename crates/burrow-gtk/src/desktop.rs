//! VNC desktop tabs. The core signs in to the server and passes its RFB
//! stream through, as if from a server that needs no password, so the
//! window speaks RFB itself with vnc-rs and draws the screen.
//!
//! The server decides every size, and vnc-rs holds it to a screen of at
//! most 8192 pixels a side and about 8.3 million pixels, with every
//! rectangle inside it. [`Frame`] checks each update against its own size
//! again before copying.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use burrow_core::ipc::Client;
use burrow_core::{Command, Node, NodeId, SessionId, SessionState};
use relm4::gtk::glib::translate::IntoGlib;
use relm4::gtk::prelude::*;
use relm4::gtk::{gdk, glib};
use relm4::{ComponentSender, gtk};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use vnc::{
    ClientKeyEvent, ClientMouseEvent, PixelFormat, Rect, VncConnector, VncEncoding, VncEvent,
    X11Event,
};

use crate::app::{App, AppMsg};
use crate::runtime::runtime;
use crate::session::{self, SessionPage};

/// How often the server is asked for what changed.
const REFRESH: Duration = Duration::from_millis(50);

/// The least time between two redraws.
const REDRAW: Duration = Duration::from_millis(16);

/// Bytes a pixel, as asked of the server: blue, green, red, then unused.
const PIXEL: usize = 4;

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

/// What the input handlers need, which changes with each session.
#[derive(Default)]
struct Shared {
    input: RefCell<Option<mpsc::UnboundedSender<X11Event>>>,
    /// The screen's size, to map the pointer onto it.
    size: Cell<(u16, u16)>,
    /// The mouse buttons held, as RFB's mask.
    buttons: Cell<u8>,
}

impl Shared {
    fn send(&self, event: X11Event) {
        if let Some(input) = self.input.borrow().as_ref() {
            let _ = input.send(event);
        }
    }

    /// Sends the pointer at `(x, y)` in `picture`, with the buttons held.
    fn pointer(&self, picture: &gtk::Picture, x: f64, y: f64) {
        let (width, height) = self.size.get();
        let shown = (f64::from(picture.width()), f64::from(picture.height()));
        if let Some((x, y)) = screen_point((x, y), shown, (width, height)) {
            self.send(X11Event::PointerEvent(ClientMouseEvent {
                position_x: x,
                position_y: y,
                bottons: self.buttons.get(),
            }));
        }
    }
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

/// Speaks RFB over the core's session until either side ends. vnc-rs reads
/// and writes one end of a pipe; the core's output goes into the other,
/// and what comes out of it goes to the core.
async fn drive(
    core: &Client,
    session: SessionId,
    mut events: mpsc::UnboundedReceiver<X11Event>,
    sender: &relm4::Sender<AppMsg>,
    node: NodeId,
) -> Result<(), String> {
    let (ours, theirs) = tokio::io::duplex(1 << 20);
    let (mut from_client, mut to_client) = tokio::io::split(theirs);
    let mut output = core.take_output(session).await.map_err(|e| e.to_string())?;
    // Two tasks, so neither direction waits on the other.
    tokio::spawn(async move {
        while let Some(data) = output.recv().await {
            if to_client.write_all(&data).await.is_err() {
                break;
            }
        }
    });
    let writer = core.clone();
    tokio::spawn(async move {
        let mut buffer = vec![0; 64 << 10];
        while let Ok(n @ 1..) = from_client.read(&mut buffer).await {
            let data = buffer[..n].to_vec();
            if writer
                .execute(Command::WriteTerminal { session, data })
                .await
                .is_err()
            {
                break;
            }
        }
    });

    let text = |e: vnc::VncError| e.to_string();
    let vnc = VncConnector::new(ours)
        // The core has signed in, so the stand-in server asks for nothing.
        .set_auth_method(async { Ok(String::new()) })
        .add_encoding(VncEncoding::Zrle)
        .add_encoding(VncEncoding::CopyRect)
        .add_encoding(VncEncoding::Raw)
        .add_encoding(VncEncoding::DesktopSizePseudo)
        .allow_shared(true)
        .set_pixel_format(PixelFormat::bgra())
        .build()
        .map_err(text)?
        .try_start()
        .await
        .map_err(text)?
        .finish()
        .map_err(text)?;
    vnc.input(X11Event::FullRefresh).await.map_err(text)?;
    let mut refresh = tokio::time::interval(REFRESH);
    loop {
        tokio::select! {
            event = vnc.recv_event() => {
                let msg = Msg::Event(event.map_err(text)?);
                if sender.send(AppMsg::Desktop { node, msg }).is_err() {
                    return Ok(());
                }
            }
            Some(event) = events.recv() => vnc.input(event).await.map_err(text)?,
            _ = refresh.tick() => vnc.input(X11Event::Refresh).await.map_err(text)?,
        }
    }
}

/// Keys, the pointer and the wheel, sent to the desktop while `picture`
/// has the focus.
fn add_input(picture: &gtk::Picture, shared: &Rc<Shared>) {
    let keys = gtk::EventControllerKey::new();
    keys.connect_key_pressed({
        let shared = shared.clone();
        move |_, key, _, _| {
            shared.send(key_event(key, true));
            glib::Propagation::Stop
        }
    });
    keys.connect_key_released({
        let shared = shared.clone();
        move |_, key, _, _| shared.send(key_event(key, false))
    });
    picture.add_controller(keys);

    let motion = gtk::EventControllerMotion::new();
    motion.connect_motion({
        let shared = shared.clone();
        move |motion, x, y| {
            if let Some(picture) = motion.widget().and_downcast::<gtk::Picture>() {
                shared.pointer(&picture, x, y);
            }
        }
    });
    picture.add_controller(motion);

    let click = gtk::GestureClick::new();
    click.set_button(0);
    let press = {
        let shared = shared.clone();
        move |click: &gtk::GestureClick, x: f64, y: f64, down: bool| {
            let bit = button_bit(click.current_button());
            let held = shared.buttons.get();
            shared
                .buttons
                .set(if down { held | bit } else { held & !bit });
            if let Some(picture) = click.widget().and_downcast::<gtk::Picture>() {
                if down {
                    picture.grab_focus();
                }
                shared.pointer(&picture, x, y);
            }
        }
    };
    click.connect_pressed({
        let press = press.clone();
        move |click, _, x, y| press(click, x, y, true)
    });
    click.connect_released(move |click, _, x, y| press(click, x, y, false));
    picture.add_controller(click);

    let scroll = gtk::EventControllerScroll::new(
        gtk::EventControllerScrollFlags::VERTICAL | gtk::EventControllerScrollFlags::DISCRETE,
    );
    scroll.connect_scroll({
        let shared = shared.clone();
        move |scroll, _, dy| {
            // RFB's wheel is buttons 4 and 5, pressed and let go.
            let bit = if dy < 0.0 { 8 } else { 16 };
            let picture = scroll.widget().and_downcast::<gtk::Picture>();
            let at = scroll.current_event().and_then(|event| event.position());
            if let (Some(picture), Some((x, y))) = (picture, at) {
                let held = shared.buttons.get();
                shared.buttons.set(held | bit);
                shared.pointer(&picture, x, y);
                shared.buttons.set(held);
                shared.pointer(&picture, x, y);
            }
            glib::Propagation::Stop
        }
    });
    picture.add_controller(scroll);
}

/// GDK key values are X keysyms, which RFB sends as they are.
fn key_event(key: gdk::Key, down: bool) -> X11Event {
    X11Event::KeyEvent(ClientKeyEvent {
        keycode: key.into_glib(),
        down,
    })
}

/// RFB's button mask bit for a GDK button.
fn button_bit(button: u32) -> u8 {
    match button {
        gdk::BUTTON_PRIMARY => 1,
        gdk::BUTTON_MIDDLE => 2,
        gdk::BUTTON_SECONDARY => 4,
        _ => 0,
    }
}

/// Where `point` in a picture of size `shown` falls on a screen of `size`,
/// drawn as large as fits and centred. `None` before there is a screen.
fn screen_point(point: (f64, f64), shown: (f64, f64), size: (u16, u16)) -> Option<(u16, u16)> {
    let (width, height) = (f64::from(size.0), f64::from(size.1));
    if width == 0.0 || height == 0.0 || shown.0 <= 0.0 || shown.1 <= 0.0 {
        return None;
    }
    let scale = (shown.0 / width).min(shown.1 / height);
    let left = (shown.0 - width * scale) / 2.0;
    let top = (shown.1 - height * scale) / 2.0;
    let x = ((point.0 - left) / scale).clamp(0.0, width - 1.0);
    let y = ((point.1 - top) / scale).clamp(0.0, height - 1.0);
    Some((x as u16, y as u16))
}

/// The screen's pixels, kept here and drawn as a texture.
#[derive(Default)]
struct Frame {
    width: u16,
    height: u16,
    pixels: Vec<u8>,
}

impl Frame {
    fn resize(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
        self.pixels = vec![0; usize::from(width) * usize::from(height) * PIXEL];
    }

    /// Whether `rect` lies inside the screen.
    fn holds(&self, rect: &Rect) -> bool {
        u32::from(rect.x) + u32::from(rect.width) <= u32::from(self.width)
            && u32::from(rect.y) + u32::from(rect.height) <= u32::from(self.height)
    }

    /// Copies `data`, `rect`'s pixels row by row, into place. An update
    /// that doesn't fit is dropped.
    fn put(&mut self, rect: Rect, data: &[u8]) {
        let row = usize::from(rect.width) * PIXEL;
        if !self.holds(&rect) || data.len() != row * usize::from(rect.height) {
            return;
        }
        let stride = usize::from(self.width) * PIXEL;
        for (n, line) in data.chunks_exact(row.max(1)).enumerate() {
            let at = (usize::from(rect.y) + n) * stride + usize::from(rect.x) * PIXEL;
            self.pixels[at..at + row].copy_from_slice(line);
        }
    }

    /// Copies the pixels at `from` to `to`, which may overlap.
    fn copy(&mut self, to: Rect, from: Rect) {
        let fits = to.width == from.width && to.height == from.height;
        if !fits || !self.holds(&to) || !self.holds(&from) {
            return;
        }
        let row = usize::from(from.width) * PIXEL;
        let stride = usize::from(self.width) * PIXEL;
        let rows: Vec<u8> = (0..usize::from(from.height))
            .flat_map(|n| {
                let at = (usize::from(from.y) + n) * stride + usize::from(from.x) * PIXEL;
                self.pixels[at..at + row].to_vec()
            })
            .collect();
        self.put(to, &rows);
    }
}

/// The desktop dialog: new when `node` is `None`. `hosts` are the SSH hosts
/// a desktop can be reached through. `done` gets the name and props.
pub fn edit(
    parent: &impl IsA<gtk::Window>,
    node: Option<&Node>,
    hosts: Vec<(NodeId, String)>,
    done: impl Fn(String, toml::Table) + 'static,
) {
    let window = gtk::Window::builder()
        .title(if node.is_some() {
            "Edit desktop"
        } else {
            "New VNC desktop"
        })
        .modal(true)
        .transient_for(parent)
        .resizable(false)
        .default_width(440)
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_margin_top(18);
    content.set_margin_bottom(18);
    content.set_margin_start(18);
    content.set_margin_end(18);
    let props = node.map(|n| n.props.clone()).unwrap_or_default();
    let text = |key: &str| props.get(key).and_then(|v| v.as_str()).map(str::to_string);

    let grid = gtk::Grid::builder()
        .row_spacing(8)
        .column_spacing(12)
        .build();
    let label = |text: &str| {
        let label = gtk::Label::new(Some(text));
        label.set_xalign(0.0);
        label
    };
    let name = gtk::Entry::new();
    name.set_hexpand(true);
    name.set_text(node.map_or("", |n| n.name.as_str()));
    grid.attach(&label("Name"), 0, 0, 1, 1);
    grid.attach(&name, 1, 0, 1, 1);

    let names: Vec<&str> = hosts.iter().map(|(_, name)| name.as_str()).collect();
    let through = gtk::DropDown::from_strings(&names);
    let via = text("via").and_then(|via| via.parse().ok()).map(NodeId);
    if let Some(index) = hosts.iter().position(|(id, _)| Some(*id) == via) {
        through.set_selected(index as u32);
    }
    grid.attach(&label("Through"), 0, 1, 1, 1);
    grid.attach(&through, 1, 1, 1, 1);

    let host = gtk::Entry::new();
    host.set_text(&text("host").unwrap_or_default());
    host.set_placeholder_text(Some("localhost, as the SSH host sees it"));
    grid.attach(&label("Host"), 0, 2, 1, 1);
    grid.attach(&host, 1, 2, 1, 1);

    let port = gtk::SpinButton::with_range(1.0, 65535.0, 1.0);
    let saved_port = props.get("port").and_then(|p| p.as_integer());
    port.set_value(saved_port.unwrap_or(5900) as f64);
    port.set_numeric(true);
    grid.attach(&label("Port"), 0, 3, 1, 1);
    grid.attach(&port, 1, 3, 1, 1);
    content.append(&grid);

    let note = gtk::Label::new(Some(
        "Burrow reaches the desktop over the SSH host's connection, so the screen \
         never crosses the network in the clear and nothing listens on this \
         computer. It asks for the VNC password when the desktop wants one.",
    ));
    note.set_xalign(0.0);
    note.set_wrap(true);
    note.set_max_width_chars(54);
    note.add_css_class("dim");
    content.append(&note);

    let error = gtk::Label::new(None);
    error.set_xalign(0.0);
    error.set_wrap(true);
    error.add_css_class("error-text");
    error.set_visible(hosts.is_empty());
    error.set_label("Add an SSH host first: a desktop is reached through one.");
    content.append(&error);

    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::End);
    buttons.set_margin_top(6);
    let cancel = gtk::Button::with_label("Cancel");
    let ok = gtk::Button::with_label(if node.is_some() { "Save" } else { "Create" });
    ok.add_css_class("suggested-action");
    ok.set_sensitive(!hosts.is_empty());
    buttons.append(&cancel);
    buttons.append(&ok);
    content.append(&buttons);
    window.set_child(Some(&content));

    // The window is held weakly by everything inside it, or it would keep
    // itself alive after closing.
    let submit = Rc::new({
        let window = window.downgrade();
        let (name, host, port, error) = (name.clone(), host.clone(), port.clone(), error.clone());
        move || {
            let Some(window) = window.upgrade() else {
                return;
            };
            port.update();
            let title = name.text().trim().to_string();
            if title.is_empty() {
                error.set_label("Give the desktop a name.");
                error.set_visible(true);
                return;
            }
            let Some((via, _)) = hosts.get(through.selected() as usize) else {
                return;
            };
            let mut props = props.clone();
            props.insert("via".into(), via.0.to_string().into());
            let host = host.text().trim().to_string();
            if host.is_empty() {
                props.remove("host");
            } else {
                props.insert("host".into(), host.into());
            }
            props.insert("port".into(), (port.value() as i64).into());
            done(title, props);
            window.close();
        }
    });
    ok.connect_clicked({
        let submit = submit.clone();
        move |_| submit()
    });
    for entry in [&name, &host] {
        let submit = submit.clone();
        entry.connect_activate(move |_| submit());
    }
    let weak = window.downgrade();
    cancel.connect_clicked(move |_| {
        if let Some(window) = weak.upgrade() {
            window.close();
        }
    });
    crate::dialogs::close_on_escape(&window);
    window.present();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: u16, y: u16, width: u16, height: u16) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn updates_land_in_place_and_ones_that_dont_fit_are_dropped() {
        let mut frame = Frame::default();
        frame.resize(4, 3);
        let red = [0, 0, 255, 0].repeat(4);
        frame.put(rect(1, 1, 2, 2), &red);
        let pixel = |frame: &Frame, x: usize, y: usize| {
            let at = (y * 4 + x) * PIXEL;
            frame.pixels[at..at + PIXEL].to_vec()
        };
        assert_eq!(pixel(&frame, 1, 1), [0, 0, 255, 0]);
        assert_eq!(pixel(&frame, 2, 2), [0, 0, 255, 0]);
        assert_eq!(pixel(&frame, 0, 0), [0; 4]);
        // Past the edge, or the wrong amount of data.
        let before = frame.pixels.clone();
        frame.put(rect(3, 2, 2, 1), &[1; 8]);
        frame.put(rect(0, 0, 2, 2), &[1; 4]);
        frame.put(rect(u16::MAX, 0, 2, 1), &[1; 8]);
        assert_eq!(frame.pixels, before);
    }

    #[test]
    fn copies_may_overlap() {
        let mut frame = Frame::default();
        frame.resize(3, 1);
        frame.put(rect(0, 0, 3, 1), &[1, 1, 1, 0, 2, 2, 2, 0, 3, 3, 3, 0]);
        frame.copy(rect(1, 0, 2, 1), rect(0, 0, 2, 1));
        assert_eq!(frame.pixels, [1, 1, 1, 0, 1, 1, 1, 0, 2, 2, 2, 0]);
        let before = frame.pixels.clone();
        frame.copy(rect(2, 0, 2, 1), rect(0, 0, 2, 1));
        assert_eq!(frame.pixels, before);
    }

    #[test]
    fn the_pointer_maps_onto_a_scaled_screen() {
        // A 200×100 screen drawn at half size in a 100×100 picture sits
        // 25 pixels down.
        assert_eq!(
            screen_point((50.0, 50.0), (100.0, 100.0), (200, 100)),
            Some((100, 50))
        );
        assert_eq!(
            screen_point((0.0, 0.0), (100.0, 100.0), (200, 100)),
            Some((0, 0))
        );
        assert_eq!(
            screen_point((100.0, 100.0), (100.0, 100.0), (200, 100)),
            Some((199, 99))
        );
        assert_eq!(screen_point((10.0, 10.0), (100.0, 100.0), (0, 0)), None);
    }

    #[test]
    fn mouse_buttons_follow_rfb() {
        assert_eq!(button_bit(gdk::BUTTON_PRIMARY), 1);
        assert_eq!(button_bit(gdk::BUTTON_MIDDLE), 2);
        assert_eq!(button_bit(gdk::BUTTON_SECONDARY), 4);
        assert_eq!(button_bit(8), 0);
    }
}
