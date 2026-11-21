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

pub(crate) mod agent;
pub(crate) mod alerts;
pub(crate) mod config;
mod connection;
pub(crate) mod exec;
pub(crate) mod files;
pub(crate) mod hosts;
mod join;
mod keeper;
mod kept;
mod known_hosts;
mod monitor;
mod prompter;
mod registry;
pub(crate) mod services;
mod session;
mod socks;
pub(crate) mod trend;
mod tunnel;

pub(crate) use agent::Agents;
pub(crate) use connection::{Connection, Connections, alias};
pub(crate) use files::Transfers;
pub(crate) use kept::AttachedOutput;
pub(crate) use monitor::Waits;
pub(crate) use prompter::{Prompter, unattended};
use registry::Entry;
pub(crate) use registry::{Attached, Input, OUTPUT_BUFFER, Sessions};
pub(crate) use tunnel::Autostart;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use prompter::Pending;
use tokio::sync::mpsc;

use crate::api::{Event, Node, SessionId, SessionKind, SessionState};
use crate::engine::Core;

/// Starts a terminal session for an SSH host node, running `command` or
/// else the login shell. It runs until that exits, the connection fails,
/// or a client closes it.
pub(crate) fn open_terminal(
    core: &Core,
    node: Node,
    cols: u16,
    rows: u16,
    command: Option<String>,
) -> SessionId {
    let (output_tx, output_rx) = mpsc::channel(OUTPUT_BUFFER);
    spawn(
        core,
        session::for_shell(node),
        SessionKind::Terminal,
        Some(output_rx),
        move |core, node, input, prompter| async move {
            let size = (cols, rows);
            let command = command.as_deref();
            session::run(&core, &node, size, command, input, output_tx, &prompter).await
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

/// Starts burrow-agent on an SSH host node. Without `install` it only runs
/// an agent that is already there. It runs until a client closes it, the
/// agent stops, or the connection fails.
pub(crate) fn open_monitor(core: &Core, node: Node, install: bool) -> SessionId {
    spawn(
        core,
        node,
        SessionKind::Monitor,
        None,
        move |core, node, mut input, prompter| async move {
            let monitor = Arc::default();
            monitor::run(&core, &node, &mut input, &prompter, install, &monitor).await
        },
    )
}

/// Starts the background monitor of an SSH host node, or returns the one
/// it already has. It reconnects whenever it stops, until the node's
/// monitoring is turned off or the node is deleted.
pub(crate) fn open_background_monitor(core: &Core, node: Node) -> SessionId {
    spawn_session(
        core,
        node,
        SessionKind::Monitor,
        None,
        true,
        |core, node, input, prompter| async move {
            monitor::run_background(&core, &node, input, &prompter).await
        },
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
    spawn_session(core, node, kind, output, false, body)
}

/// [`spawn`], for a background session too. A node has at most one: if
/// it has one already, that is returned and `body` never runs.
fn spawn_session<F, Fut>(
    core: &Core,
    node: Node,
    kind: SessionKind,
    output: Option<mpsc::Receiver<Vec<u8>>>,
    background: bool,
    body: F,
) -> SessionId
where
    F: FnOnce(Core, Node, mpsc::UnboundedReceiver<Input>, Prompter) -> Fut + Send + 'static,
    Fut: Future<Output = std::result::Result<(), String>> + Send,
{
    let (input_tx, input_rx) = mpsc::unbounded_channel();
    let prompts = Pending::default();
    let mut map = core.sessions().map.lock().unwrap();
    // Checked under the same lock as the insert, so two starts at once
    // still make one.
    let running = map
        .iter()
        .find(|(_, e)| e.background && e.node == node.id && e.input.is_some());
    if background && let Some((&id, _)) = running {
        return id;
    }
    let id = SessionId(core.sessions().next.fetch_add(1, Ordering::Relaxed) + 1);
    map.insert(
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
            background,
            keeper: None,
        },
    );
    drop(map);
    let prompter = Prompter::new(core.clone(), id, prompts);
    let explainer = prompter.clone();
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
                error: result.err().map(|e| explainer.explain(e)),
            },
        });
    });
    id
}
