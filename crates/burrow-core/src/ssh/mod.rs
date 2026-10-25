//! SSH sessions: terminals, tunnels and file panels. Each one runs as its own task: it
//! reads the host's settings from the OpenSSH config, connects with russh,
//! checks the host key, signs in, then runs a shell, forwards ports or
//! speaks SFTP, reporting progress as events.
//!
//! Terminal output does not go through the event broadcast. A burst of
//! output (`cat` of a big file) would push every subscriber past its event
//! buffer. Each session has its own bounded output stream instead, so a slow
//! reader slows the SSH channel down rather than losing output.
//!
//! Sessions on the same host share one connection ([`Connections`]).

mod config;
mod connection;
pub(crate) mod files;
mod known_hosts;
mod session;
mod socks;
mod tunnel;

pub(crate) use connection::{Connection, Connections, alias};
pub(crate) use files::Transfers;
pub(crate) use tunnel::Autostart;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use russh_sftp::client::SftpSession;
use tokio::sync::{mpsc, oneshot};

use crate::api::{
    Event, Node, NodeId, Prompt, PromptAnswer, PromptKind, SessionId, SessionInfo, SessionKind,
    SessionState,
};
use crate::db;
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
    node: NodeId,
    kind: SessionKind,
    /// As last announced.
    state: SessionState,
    /// `None` once the session has ended.
    input: Option<mpsc::UnboundedSender<Input>>,
    prompts: Pending,
    /// Until a client takes it with [`Core::take_output`].
    output: Option<mpsc::Receiver<Vec<u8>>>,
    /// What a connected file or database session serves requests with.
    attached: Option<Attached>,
}

/// A connected session's client, which commands on it use.
#[derive(Clone)]
pub(crate) enum Attached {
    Sftp(Arc<SftpSession>),
    Database(Arc<db::Handle>),
}

/// Prompts waiting for an answer, by prompt id.
type Pending = Arc<Mutex<HashMap<u32, oneshot::Sender<PromptAnswer>>>>;

impl Sessions {
    pub fn send(&self, id: SessionId, input: Input) -> Result<()> {
        let map = self.map.lock().unwrap();
        let entry = map
            .get(&id)
            .filter(|e| e.input.is_some())
            .ok_or(Error::SessionNotFound(id))?;
        if entry.kind != SessionKind::Terminal && !matches!(input, Input::Close) {
            return Err(Error::invalid("session", format!("{id} is not a terminal")));
        }
        let sender = entry.input.as_ref().ok_or(Error::SessionNotFound(id))?;
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

    /// Every running session, oldest first.
    pub fn list(&self) -> Vec<SessionInfo> {
        let map = self.map.lock().unwrap();
        let mut sessions: Vec<SessionInfo> = map
            .iter()
            .filter(|(_, e)| e.input.is_some())
            .map(|(&session, e)| SessionInfo {
                session,
                node: e.node,
                kind: e.kind,
                state: e.state.clone(),
            })
            .collect();
        sessions.sort_by_key(|s| s.session.0);
        sessions
    }

    /// The running session of `kind` on `node`, if there is one.
    pub fn find(&self, node: NodeId, kind: SessionKind) -> Option<SessionId> {
        self.list()
            .into_iter()
            .find(|s| s.node == node && s.kind == kind)
            .map(|s| s.session)
    }

    /// The client of a connected session of `kind`, and the session's node.
    fn attached(&self, id: SessionId, kind: SessionKind) -> Result<(Attached, NodeId)> {
        let map = self.map.lock().unwrap();
        let entry = map
            .get(&id)
            .filter(|e| e.input.is_some())
            .ok_or(Error::SessionNotFound(id))?;
        if entry.kind != kind {
            let what = match kind {
                SessionKind::Files => "a file session",
                SessionKind::Database => "a database session",
                SessionKind::Terminal => "a terminal",
                SessionKind::Tunnel => "a tunnel",
            };
            return Err(Error::invalid("session", format!("{id} is not {what}")));
        }
        let attached = entry
            .attached
            .clone()
            .ok_or_else(|| Error::invalid("session", format!("{id} has not connected yet")))?;
        Ok((attached, entry.node))
    }

    /// The SFTP client of a connected file session.
    pub fn sftp(&self, id: SessionId) -> Result<Arc<SftpSession>> {
        match self.attached(id, SessionKind::Files)? {
            (Attached::Sftp(sftp), _) => Ok(sftp),
            _ => unreachable!("file sessions attach SFTP"),
        }
    }

    /// The client of a connected database session, and its node.
    pub fn database(&self, id: SessionId) -> Result<(Arc<db::Handle>, NodeId)> {
        match self.attached(id, SessionKind::Database)? {
            (Attached::Database(handle), node) => Ok((handle, node)),
            _ => unreachable!("database sessions attach a database"),
        }
    }

    pub(crate) fn attach(&self, id: SessionId, attached: Attached) {
        if let Some(entry) = self.map.lock().unwrap().get_mut(&id) {
            entry.attached = Some(attached);
        }
    }

    /// Records a session's new state as it is announced.
    pub fn record(&self, id: SessionId, state: &SessionState) {
        if let Some(entry) = self.map.lock().unwrap().get_mut(&id) {
            entry.state = state.clone();
        }
    }

    /// Closes every session on `node`, as when the node is deleted.
    pub fn close_node(&self, node: NodeId) {
        for entry in self.map.lock().unwrap().values() {
            if entry.node == node
                && let Some(input) = &entry.input
            {
                let _ = input.send(Input::Close);
            }
        }
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
                entry.attached = None;
                entry.prompts.lock().unwrap().clear();
            }
            _ => {
                map.remove(&id);
            }
        }
    }
}

/// Starts a terminal session for an SSH host node. It runs until the shell
/// exits, the connection fails, or a client closes it.
pub(crate) fn open_terminal(core: &Core, node: Node, cols: u16, rows: u16) -> SessionId {
    let (output_tx, output_rx) = mpsc::channel(OUTPUT_BUFFER);
    spawn(
        core,
        node,
        SessionKind::Terminal,
        Some(output_rx),
        move |core, node, input, prompter| async move {
            let size = (cols, rows);
            session::run(&core, &node, size, input, output_tx, &prompter).await
        },
    )
}

/// Starts a tunnel node. It runs until a client closes it or the
/// connection fails.
pub(crate) fn open_tunnel(core: &Core, node: Node) -> SessionId {
    spawn(
        core,
        node,
        SessionKind::Tunnel,
        None,
        |core, node, input, prompter| async move { tunnel::run(&core, &node, input, &prompter).await },
    )
}

/// Opens an SFTP session for an SSH host node's file panel. It runs until a
/// client closes it or the connection fails.
pub(crate) fn open_files(core: &Core, node: Node) -> SessionId {
    spawn(
        core,
        node,
        SessionKind::Files,
        None,
        |core, node, input, prompter| async move { files::run(&core, &node, input, &prompter).await },
    )
}

/// Registers a session and runs `body` as its task. Whatever `body`
/// returns becomes the session's [`SessionState::Closed`].
pub(crate) fn spawn<F, Fut>(
    core: &Core,
    node: Node,
    kind: SessionKind,
    output: Option<mpsc::Receiver<Vec<u8>>>,
    body: F,
) -> SessionId
where
    F: FnOnce(Core, Node, mpsc::UnboundedReceiver<Input>, Prompter) -> Fut + Send + 'static,
    Fut: Future<Output = std::result::Result<(), String>> + Send,
{
    let id = SessionId(core.sessions().next.fetch_add(1, Ordering::Relaxed) + 1);
    let (input_tx, input_rx) = mpsc::unbounded_channel();
    let prompts = Pending::default();
    core.sessions().map.lock().unwrap().insert(
        id,
        Entry {
            node: node.id,
            kind,
            state: SessionState::Connecting {
                step: "Starting".into(),
            },
            input: Some(input_tx),
            prompts: prompts.clone(),
            output,
            attached: None,
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
        let node_id = node.id;
        let result = body(core.clone(), node, input_rx, prompter).await;
        core.sessions().finish(id);
        core.transfers().cancel_session(id);
        core.emit(Event::SessionChanged {
            session: id,
            node: node_id,
            state: SessionState::Closed {
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
    /// The session questions go to.
    pub fn session(&self) -> SessionId {
        self.session
    }

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
