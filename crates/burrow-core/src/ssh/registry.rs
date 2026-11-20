//! The registry of open sessions: what each one is, where it stands, and
//! how clients reach it. The sessions themselves run as tasks started in
//! the parent module.

use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use russh_sftp::client::SftpSession;
use tokio::sync::mpsc;

use super::keeper::Keeper;
use super::monitor;
use super::prompter::Pending;
use crate::api::{NodeId, PromptAnswer, SessionId, SessionInfo, SessionKind, SessionState};
use crate::db;
use crate::error::{Error, Result};

/// Output chunks a session buffers before it waits for the reader.
pub(crate) const OUTPUT_BUFFER: usize = 64;

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
    pub(super) next: AtomicU64,
    pub(super) map: Mutex<HashMap<SessionId, Entry>>,
}

pub(super) struct Entry {
    pub(super) node: NodeId,
    pub(super) kind: SessionKind,
    /// As last announced.
    pub(super) state: SessionState,
    /// `None` once the session has ended.
    pub(super) input: Option<mpsc::UnboundedSender<Input>>,
    pub(super) prompts: Pending,
    /// Until a client takes it with [`Core::take_output`].
    pub(super) output: Option<mpsc::Receiver<Vec<u8>>>,
    /// What a connected file or database session serves requests with.
    pub(super) attached: Option<Attached>,
    /// A background monitor, which clients may use but not close.
    pub(super) background: bool,
    /// A kept terminal's output, once [`Sessions::keep`] took it.
    pub(super) keeper: Option<Arc<Keeper>>,
}

/// A connected session's client, which commands on it use.
#[derive(Clone)]
pub(crate) enum Attached {
    Sftp(Arc<SftpSession>),
    Database(Arc<db::Handle>),
    Monitor(Arc<monitor::Monitor>),
}

impl Sessions {
    pub fn send(&self, id: SessionId, input: Input) -> Result<()> {
        let map = self.map.lock().unwrap();
        let entry = map
            .get(&id)
            .filter(|e| e.input.is_some())
            .ok_or(Error::SessionNotFound(id))?;
        let writable = matches!(entry.kind, SessionKind::Terminal | SessionKind::Vnc);
        if !writable && !matches!(input, Input::Close) {
            return Err(Error::invalid("session", format!("{id} is not a terminal")));
        }
        if entry.background {
            return Err(Error::invalid(
                "session",
                format!("{id} monitors its host in the background; turn that off to stop it"),
            ));
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
        if !entry.prompts.answer(prompt, answer) {
            return Err(Error::invalid("prompt", format!("{prompt} is not waiting")));
        }
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
                background: e.background,
                kept: e.keeper.is_some(),
                attached: e.keeper.as_ref().is_some_and(|k| k.attached()),
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

    /// The background monitor of `node`, if it has one.
    pub fn background(&self, node: NodeId) -> Option<SessionId> {
        self.list()
            .into_iter()
            .find(|s| s.node == node && s.background)
            .map(|s| s.session)
    }

    /// Stops the background monitors of hosts not in `keep`.
    pub fn stop_background(&self, keep: &[NodeId]) {
        for entry in self.map.lock().unwrap().values() {
            if entry.background
                && !keep.contains(&entry.node)
                && let Some(input) = &entry.input
            {
                let _ = input.send(Input::Close);
            }
        }
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
                SessionKind::Exec => "a command",
                SessionKind::Monitor => "a monitor",
                SessionKind::Vnc => "a desktop",
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

    /// What a connected monitor session has seen.
    pub fn monitor(&self, id: SessionId) -> Result<Arc<monitor::Monitor>> {
        match self.attached(id, SessionKind::Monitor)? {
            (Attached::Monitor(monitor), _) => Ok(monitor),
            _ => unreachable!("monitor sessions attach a monitor"),
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
    pub(super) fn finish(&self, id: SessionId) {
        let mut map = self.map.lock().unwrap();
        match map.get_mut(&id) {
            Some(entry) if entry.output.is_some() => {
                entry.input = None;
                entry.attached = None;
                entry.prompts.clear();
            }
            _ => {
                map.remove(&id);
            }
        }
    }
}
