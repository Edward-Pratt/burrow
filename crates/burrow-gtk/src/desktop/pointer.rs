//! Pointer moves, batched. Each message to the server is a command to the
//! daemon, and a mouse reports far more moves than a screen can show, so
//! only the latest position is kept and sent once a frame.
//!
//! Anything else (a button, the wheel, a key, the clipboard) sends the
//! pending move first, so the server sees events in the order they
//! happened and a click lands where the pointer was.

use std::time::Duration;

use vnc::{ClientMouseEvent, X11Event};

/// The longest a move waits before it is sent.
pub(super) const FRAME: Duration = Duration::from_millis(16);

#[derive(Default)]
pub(super) struct Batch {
    /// The latest move not yet sent.
    pending: Option<ClientMouseEvent>,
    /// The buttons the server last heard were held.
    buttons: u8,
}

impl Batch {
    /// Takes `event` from the window, and returns what to send now. A move
    /// that changes no button waits for [`Batch::flush`].
    pub(super) fn push(&mut self, event: X11Event) -> Vec<X11Event> {
        if let X11Event::PointerEvent(pointer) = &event
            && pointer.bottons == self.buttons
        {
            self.pending = Some(pointer.clone());
            return Vec::new();
        }
        let mut now: Vec<X11Event> = self.flush().into_iter().collect();
        if let X11Event::PointerEvent(pointer) = &event {
            self.buttons = pointer.bottons;
        }
        now.push(event);
        now
    }

    /// The move waiting to be sent, if there is one.
    pub(super) fn flush(&mut self) -> Option<X11Event> {
        self.pending.take().map(X11Event::PointerEvent)
    }

    pub(super) fn is_pending(&self) -> bool {
        self.pending.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vnc::ClientKeyEvent;

    fn at(x: u16, buttons: u8) -> X11Event {
        X11Event::PointerEvent(ClientMouseEvent {
            position_x: x,
            position_y: 0,
            bottons: buttons,
        })
    }

    fn position(event: &X11Event) -> (u16, u8) {
        match event {
            X11Event::PointerEvent(p) => (p.position_x, p.bottons),
            _ => panic!("not a pointer event"),
        }
    }

    #[test]
    fn moves_wait_and_only_the_latest_is_sent() {
        let mut batch = Batch::default();
        for x in 0..100 {
            assert!(batch.push(at(x, 0)).is_empty());
        }
        assert!(batch.is_pending());
        assert_eq!(batch.flush().as_ref().map(position), Some((99, 0)));
        assert!(batch.flush().is_none());
    }

    #[test]
    fn a_button_sends_the_move_before_it() {
        let mut batch = Batch::default();
        batch.push(at(5, 0));
        let sent: Vec<_> = batch.push(at(6, 1)).iter().map(position).collect();
        assert_eq!(sent, [(5, 0), (6, 1)]);
        // Dragging with the button held batches again.
        assert!(batch.push(at(7, 1)).is_empty());
        let sent: Vec<_> = batch.push(at(8, 0)).iter().map(position).collect();
        assert_eq!(sent, [(7, 1), (8, 0)]);
    }

    #[test]
    fn the_wheel_is_never_swallowed() {
        // The wheel presses and lets go of button 4 at one spot.
        let mut batch = Batch::default();
        let sent: Vec<_> = [at(3, 8), at(3, 0)]
            .into_iter()
            .flat_map(|event| batch.push(event))
            .map(|event| position(&event))
            .collect();
        assert_eq!(sent, [(3, 8), (3, 0)]);
        assert!(!batch.is_pending());
    }

    #[test]
    fn keys_send_the_move_first() {
        let mut batch = Batch::default();
        batch.push(at(4, 0));
        let key = X11Event::KeyEvent(ClientKeyEvent {
            keycode: 0x61,
            down: true,
        });
        let sent = batch.push(key);
        assert_eq!(sent.len(), 2);
        assert_eq!(position(&sent[0]), (4, 0));
        assert!(matches!(sent[1], X11Event::KeyEvent(_)));
    }
}
