//! Database sessions. Each one is a session like a terminal: it connects in
//! the background, asks its questions through prompts, and reports its
//! state as events. Once connected, commands run queries on it.
//!
//! Connections start read-only (`default_transaction_read_only`). That
//! guards against slips, not against the user: `SET` turns it off, and
//! [`Command::SetReadOnly`](crate::api::Command::SetReadOnly) does just
//! that.

mod history;
mod postgres;

pub(crate) use history::History;
pub(crate) use postgres::Handle;

use std::sync::Arc;

use tokio::sync::mpsc;

use crate::api::{Event, Node, SessionId, SessionKind, SessionState};
use crate::engine::Core;
use crate::ssh::{self, Attached, Input, Prompter};

/// Rows kept per statement when a query doesn't say.
pub(crate) const DEFAULT_LIMIT: u32 = 1000;

/// The most rows a query may ask to keep per statement.
pub(crate) const MAX_LIMIT: u32 = 100_000;

/// Connects to a database node. The session runs until a client closes it
/// or the connection ends.
pub(crate) fn open(core: &Core, node: Node) -> SessionId {
    ssh::spawn(
        core,
        node,
        SessionKind::Database,
        None,
        |core, node, input, prompter| async move { run(&core, &node, input, &prompter).await },
    )
}

async fn run(
    core: &Core,
    node: &Node,
    mut input: mpsc::UnboundedReceiver<Input>,
    prompter: &Prompter,
) -> Result<(), String> {
    let session = prompter.session();
    let step = |step: String| {
        core.emit(Event::SessionChanged {
            session,
            node: node.id,
            state: SessionState::Connecting { step },
        })
    };
    let spec = postgres::Spec::from_node(node)?;
    let setup = postgres::connect(core, node, &spec, prompter, &step);
    let closed = async {
        while let Some(input) = input.recv().await {
            if let Input::Close = input {
                return;
            }
        }
    };
    let (handle, mut connection) = tokio::select! {
        result = setup => result?,
        () = closed => return Ok(()),
    };
    core.sessions()
        .attach(session, Attached::Database(Arc::new(handle)));
    core.emit(Event::SessionChanged {
        session,
        node: node.id,
        state: SessionState::Connected,
    });
    let result = loop {
        tokio::select! {
            ended = &mut connection => {
                break Err(ended.unwrap_or_else(|e| e.to_string()));
            }
            message = input.recv() => match message {
                Some(Input::Close) | None => break Ok(()),
                Some(_) => {}
            },
        }
    };
    // A query still running holds the client; stopping the connection
    // ends it rather than letting it run on unseen.
    connection.abort();
    result
}
