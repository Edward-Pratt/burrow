//! The client over an in-memory stream, with the test playing the daemon.

use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines, WriteHalf};
use tokio::sync::broadcast::error::RecvError;

use super::*;
use crate::{Event, NodeId};

const WAIT: Duration = Duration::from_secs(5);

/// The daemon's end of the stream.
struct Daemon {
    lines: Lines<BufReader<tokio::io::ReadHalf<DuplexStream>>>,
    write: WriteHalf<DuplexStream>,
}

impl Daemon {
    async fn read(&mut self) -> Value {
        let line = tokio::time::timeout(WAIT, self.lines.next_line())
            .await
            .expect("timed out waiting for the client")
            .unwrap()
            .expect("the client closed the stream");
        serde_json::from_str(&line).unwrap()
    }

    async fn send(&mut self, message: Value) {
        let line = format!("{message}\n");
        self.write.write_all(line.as_bytes()).await.unwrap();
    }
}

/// A client connected over an in-memory stream, and the daemon's end,
/// with `initialize` already answered.
async fn connected(role: Role) -> (Client, Daemon) {
    let (ours, theirs) = tokio::io::duplex(1 << 16);
    let (read, write) = tokio::io::split(theirs);
    let mut daemon = Daemon {
        lines: BufReader::new(read).lines(),
        write,
    };
    let (read, write) = tokio::io::split(ours);
    let connecting = tokio::spawn(Client::over(read, write, role));

    let hello = daemon.read().await;
    assert_eq!(hello["method"], "initialize");
    let params: Initialize = serde_json::from_value(hello["params"].clone()).unwrap();
    assert_eq!(params.api, API_VERSION);
    assert_eq!(params.role, role);
    assert_eq!(params.build, BUILD);
    let info = json!({"api": API_VERSION, "build": "daemon-build", "addons_dir": "/addons"});
    daemon
        .send(json!({"jsonrpc": "2.0", "id": hello["id"], "result": info}))
        .await;
    let client = connecting.await.unwrap().unwrap();
    assert_eq!(client.info().build, "daemon-build");
    (client, daemon)
}

#[tokio::test]
async fn commands_go_out_and_answers_and_errors_come_back() {
    let (client, mut daemon) = connected(Role::Cli).await;

    let running = tokio::spawn({
        let client = client.clone();
        async move { client.execute(Command::ListNodes).await }
    });
    let request = daemon.read().await;
    assert_eq!(request["method"], "execute");
    assert_eq!(request["params"], json!({"type": "list_nodes"}));
    daemon
        .send(json!({"jsonrpc": "2.0", "id": request["id"], "result": {"type": "nodes", "nodes": []}}))
        .await;
    let response = running.await.unwrap().unwrap();
    assert_eq!(response, Response::Nodes { nodes: vec![] });

    let running = tokio::spawn({
        let client = client.clone();
        async move { client.execute(Command::Version).await }
    });
    let request = daemon.read().await;
    let error = json!({"code": -32000, "message": "node 1 not found"});
    daemon
        .send(json!({"jsonrpc": "2.0", "id": request["id"], "error": error}))
        .await;
    let error = running.await.unwrap().unwrap_err();
    assert!(
        matches!(&error, Error::Remote(m) if m == "node 1 not found"),
        "{error}"
    );
}

#[tokio::test]
async fn events_reach_subscribers_until_the_stream_ends() {
    let (client, mut daemon) = connected(Role::Gui).await;
    let mut events = client.subscribe();

    let id = NodeId::new();
    let event = json!({"type": "node_deleted", "id": id});
    daemon
        .send(json!({"jsonrpc": "2.0", "method": "event", "params": event}))
        .await;
    let heard = tokio::time::timeout(WAIT, events.recv()).await.unwrap();
    assert!(
        matches!(heard, Ok(Event::NodeDeleted { id: got }) if got == id),
        "{heard:?}"
    );

    daemon
        .send(json!({"jsonrpc": "2.0", "method": "resync"}))
        .await;
    let heard = tokio::time::timeout(WAIT, events.recv()).await.unwrap();
    assert!(matches!(heard, Err(RecvError::Lagged(0))), "{heard:?}");

    // A call waiting when the daemon goes fails rather than hang.
    let running = tokio::spawn({
        let client = client.clone();
        async move { client.execute(Command::ListNodes).await }
    });
    daemon.read().await;
    drop(daemon);
    let error = tokio::time::timeout(WAIT, running).await.unwrap().unwrap();
    assert!(
        error
            .unwrap_err()
            .to_string()
            .contains("lost the connection")
    );
    let heard = tokio::time::timeout(WAIT, events.recv()).await.unwrap();
    assert!(matches!(heard, Err(RecvError::Closed)), "{heard:?}");
    assert!(matches!(
        client.subscribe().recv().await,
        Err(RecvError::Closed)
    ));
    assert!(client.execute(Command::Version).await.is_err());
}

#[tokio::test]
async fn terminal_output_is_decoded_and_released_when_nobody_reads() {
    let (client, mut daemon) = connected(Role::Gui).await;
    let session = SessionId(7);

    let taking = tokio::spawn({
        let client = client.clone();
        async move { client.take_output(session).await }
    });
    let request = daemon.read().await;
    assert_eq!(request["method"], "take_output");
    assert_eq!(request["params"], json!({"session": 7}));
    daemon
        .send(json!({"jsonrpc": "2.0", "id": request["id"], "result": null}))
        .await;
    let mut output = taking.await.unwrap().unwrap();

    let data = json!({"session": 7, "data": "aGVsbG8="});
    daemon
        .send(json!({"jsonrpc": "2.0", "method": "output", "params": data}))
        .await;
    let chunk = tokio::time::timeout(WAIT, output.recv()).await.unwrap();
    assert_eq!(chunk.as_deref(), Some(&b"hello"[..]));

    // With the stream dropped, the next output tells the daemon to let
    // the session go.
    drop(output);
    daemon
        .send(json!({"jsonrpc": "2.0", "method": "output", "params": data}))
        .await;
    let release = daemon.read().await;
    assert_eq!(release["method"], "release_output");
    assert_eq!(release["params"], json!({"session": 7}));
    assert!(release.get("id").is_none());
}
