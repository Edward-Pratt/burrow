//! What a [`Client`](super::Client)'s subscribers hear.

use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;

use crate::Event;

/// What the reader passes to subscribers.
#[derive(Clone)]
pub(super) enum Signal {
    Event(Event),
    /// The daemon dropped events for this client.
    Resync,
    Closed,
}

/// A subscription to the daemon's events, read like a broadcast receiver.
pub struct Events {
    signals: broadcast::Receiver<Signal>,
    closed: bool,
}

impl Events {
    pub(super) fn new(signals: broadcast::Receiver<Signal>, closed: bool) -> Events {
        Events { signals, closed }
    }

    /// The next event. `Lagged` means some were missed, here or in the
    /// daemon, and everything should be listed again. `Closed` means the
    /// connection to the daemon is gone.
    pub async fn recv(&mut self) -> Result<Event, RecvError> {
        if self.closed {
            return Err(RecvError::Closed);
        }
        match self.signals.recv().await {
            Ok(Signal::Event(event)) => Ok(event),
            Ok(Signal::Resync) => Err(RecvError::Lagged(0)),
            Ok(Signal::Closed) | Err(RecvError::Closed) => {
                self.closed = true;
                Err(RecvError::Closed)
            }
            Err(lagged) => Err(lagged),
        }
    }
}
