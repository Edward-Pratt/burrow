//! SSH sessions. Each one runs as its own task: it reads the host's
//! settings from the OpenSSH config, connects with russh, checks the host
//! key, signs in and runs a shell, reporting progress as events.
//!
//! Terminal output does not go through the event broadcast. A burst of
//! output (`cat` of a big file) would push every subscriber past its event
//! buffer. Each session has its own bounded output stream instead, so a slow
//! reader slows the SSH channel down rather than losing output.

mod config;
mod known_hosts;
mod session;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, oneshot};

use crate::api::{Event, Node, Prompt, PromptAnswer, PromptKind, SessionId};
use crate::engine::Core;
use crate::error::{Error, Result};

/// Output chunks a session buffers before it waits for the reader.
const OUTPUT_BUFFER: usize = 64;

/// What clients send to a running session.
#[derive(Debug)]
pub(crate) enum Input {
    Write(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    Close,
}

/// Every open session, by id.
#[derive(Default)]
pub(crate) struct Sessions {
    next: AtomicU64,
    map: Mutex<HashMap<SessionId, Entry>>,
}

struct Entry {
    /// `None` once the session has ended.
    input: Option<mpsc::UnboundedSender<Input>>,
    prompts: Pending,
    /// Until a client takes it with [`Core::take_output`].
    output: Option<mpsc::Receiver<Vec<u8>>>,
}

/// Prompts waiting for an answer, by prompt id.
type Pending = Arc<Mutex<HashMap<u32, oneshot::Sender<PromptAnswer>>>>;

impl Sessions {
    pub fn send(&self, id: SessionId, input: Input) -> Result<()> {
        let map = self.map.lock().unwrap();
        let sender = map
            .get(&id)
            .and_then(|e| e.input.as_ref())
            .ok_or(Error::SessionNotFound(id))?;
        // A send error means the session is finishing; it reports that
        // itself.
        let _ = sender.send(input);
        Ok(())
    }

    pub fn answer(&self, id: SessionId, prompt: u32, answer: PromptAnswer) -> Result<()> {
        let map = self.map.lock().unwrap();
        let entry = map
            .get(&id)
            .filter(|e| e.input.is_some())
            .ok_or(Error::SessionNotFound(id))?;
        let waiting = entry.prompts.lock().unwrap().remove(&prompt);
        let sender =
            waiting.ok_or_else(|| Error::invalid("prompt", format!("{prompt} is not waiting")))?;
        let _ = sender.send(answer);
        Ok(())
    }

    pub fn take_output(&self, id: SessionId) -> Result<mpsc::Receiver<Vec<u8>>> {
        let mut map = self.map.lock().unwrap();
        let entry = map.get_mut(&id).ok_or(Error::SessionNotFound(id))?;
        let output = entry
            .output
            .take()
            .ok_or_else(|| Error::invalid("session", format!("{id}'s output is already taken")))?;
        if entry.input.is_none() {
            map.remove(&id);
        }
        Ok(output)
    }

    /// Ends a session's bookkeeping. Pending prompts are dropped, which
    /// their askers see as "cancelled".
    ///
    /// A session can fail before its client has taken the output stream,
    /// for example on a bad alias. The entry then stays until the stream is
    /// taken, so `take_output` straight after `open_terminal` always works;
    /// the stream just ends at once.
    fn finish(&self, id: SessionId) {
        let mut map = self.map.lock().unwrap();
        match map.get_mut(&id) {
            Some(entry) if entry.output.is_some() => {
                entry.input = None;
                entry.prompts.lock().unwrap().clear();
            }
            _ => {
                map.remove(&id);
            }
        }
    }
}

/// Starts a session for an SSH host node. It runs until the shell exits,
/// the connection fails, or a client closes it.
pub(crate) fn open(core: &Core, node: Node, cols: u16, rows: u16) -> SessionId {
    let id = SessionId(core.sessions().next.fetch_add(1, Ordering::Relaxed) + 1);
    let (input_tx, input_rx) = mpsc::unbounded_channel();
    let (output_tx, output_rx) = mpsc::channel(OUTPUT_BUFFER);
    let prompts = Pending::default();
    core.sessions().map.lock().unwrap().insert(
        id,
        Entry {
            input: Some(input_tx),
            prompts: prompts.clone(),
            output: Some(output_rx),
        },
    );
    let prompter = Prompter {
        core: core.clone(),
        session: id,
        pending: prompts,
        next: Arc::default(),
    };
    let core = core.clone();
    tokio::spawn(async move {
        let size = (cols, rows);
        let result = session::run(&core, &node, size, input_rx, output_tx, &prompter).await;
        core.sessions().finish(id);
        core.emit(Event::SessionChanged {
            session: id,
            node: node.id,
            state: crate::api::SessionState::Closed {
                error: result.err(),
            },
        });
    });
    id
}

/// Asks the session's clients a question and waits for the answer.
#[derive(Clone)]
pub(crate) struct Prompter {
    core: Core,
    session: SessionId,
    pending: Pending,
    next: Arc<AtomicU32>,
}

impl Prompter {
    /// [`PromptAnswer::Cancel`] if the session closes first.
    pub async fn ask(&self, kind: PromptKind, title: &str, message: &str) -> PromptAnswer {
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        self.core.emit(Event::PromptRequested {
            session: self.session,
            prompt: Prompt {
                id,
                kind,
                title: title.to_string(),
                message: message.to_string(),
            },
        });
        rx.await.unwrap_or(PromptAnswer::Cancel)
    }
}
