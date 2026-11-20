//! Kept terminals over the socket, against a throwaway `sshd`.

use std::path::PathBuf;
use std::time::Duration;

use burrow_core::ipc::Client;
use burrow_core::{Command, Node, SessionId, SessionInfo};
use tokio::sync::mpsc;

use super::common::*;

/// A daemon with an SSH host node for the server, served on a socket.
pub async fn daemon(server: &Server) -> (PathBuf, Node) {
    trust(server);
    let core = server.core();
    let node = host(&core, "test").await;
    (serve(core, server.dir.path()), node)
}

/// Opens a terminal from `client` and keeps it.
pub async fn kept(client: &Client, node: &Node) -> SessionId {
    let session = terminal(client, node).await;
    client
        .execute(Command::KeepTerminal { session })
        .await
        .unwrap();
    session
}

pub async fn terminal(client: &Client, node: &Node) -> SessionId {
    let command = Command::OpenTerminal {
        node: node.id,
        cols: 80,
        rows: 24,
        run: None,
    };
    open_and_wait(client, command).await
}

pub async fn write(client: &Client, session: SessionId, text: &str) {
    let command = Command::WriteTerminal {
        session,
        data: text.as_bytes().to_vec(),
    };
    client.execute(command).await.unwrap();
}

pub async fn info(client: &Client, session: SessionId) -> Option<SessionInfo> {
    match client.execute(Command::ListSessions).await.unwrap() {
        burrow_core::Response::Sessions { sessions } => {
            sessions.into_iter().find(|s| s.session == session)
        }
        other => panic!("expected sessions, got {other:?}"),
    }
}

/// Reads until the output, after `seen`, contains `needle`.
pub async fn read_until(output: &mut mpsc::Receiver<Vec<u8>>, mut seen: Vec<u8>, needle: &str) {
    tokio::time::timeout(WAIT, async {
        while !String::from_utf8_lossy(&seen).contains(needle) {
            let chunk = output.recv().await.expect("output ended");
            seen.extend(chunk);
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no {needle:?} in {:?}", String::from_utf8_lossy(&seen)));
}

/// Waits until listing says what `ready` wants of the session.
pub async fn until(client: &Client, session: SessionId, ready: impl Fn(&SessionInfo) -> bool) {
    tokio::time::timeout(WAIT, async {
        while !info(client, session).await.as_ref().is_some_and(&ready) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("timed out waiting on the session");
}
