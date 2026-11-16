//! Keys, the pointer and the wheel, turned into RFB input.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use relm4::gtk;
use relm4::gtk::glib::translate::IntoGlib;
use relm4::gtk::prelude::*;
use relm4::gtk::{gdk, glib};
use tokio::sync::mpsc;
use vnc::{ClientKeyEvent, ClientMouseEvent, X11Event};

/// What the input handlers need, which changes with each session.
#[derive(Default)]
pub(super) struct Shared {
    pub(super) input: RefCell<Option<mpsc::UnboundedSender<X11Event>>>,
    /// The screen's size, to map the pointer onto it.
    pub(super) size: Cell<(u16, u16)>,
    /// The mouse buttons held, as RFB's mask.
    buttons: Cell<u8>,
}

impl Shared {
    pub(super) fn send(&self, event: X11Event) {
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

/// Keys, the pointer and the wheel, sent to the desktop while `picture`
/// has the focus.
pub(super) fn add_input(picture: &gtk::Picture, shared: &Rc<Shared>) {
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

#[cfg(test)]
mod tests {
    use super::*;

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
