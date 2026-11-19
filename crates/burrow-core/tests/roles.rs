//! The role a client gives in `initialize`, and how firmly it sticks:
//! `burrow proxy` checks only the first line it is sent, so the daemon
//! must never let a connection change its role afterwards.

use std::path::Path;
use std::sync::Arc;

use burrow_core::{Command, Core, CoreOptions, MemoryStore};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

mod common;
use common::*;

fn core(dir: &Path) -> Core {
    Core::open(CoreOptions {
        config_dir: dir.join("burrow"),
        state_dir: dir.join("state"),
        secrets: Arc::new(MemoryStore::new()),
        ssh_config: None,
    })
    .unwrap()
}

fn folder(name: &str) -> Command {
    Command::CreateNode {
        parent: None,
        kind: "folder".into(),
        name: name.into(),
        color: None,
        props: toml::Table::new(),
    }
}

/// A connection spoken to line by line, as a client that isn't
/// [`burrow_core::ipc::Client`] would.
struct Raw {
    lines: Lines<tokio::io::BufReader<OwnedReadHalf>>,
    write: OwnedWriteHalf,
}

impl Raw {
    async fn open(socket: &Path) -> Raw {
        let (read, write) = UnixStream::connect(socket).await.unwrap().into_split();
        Raw {
            lines: BufReader::new(read).lines(),
            write,
        }
    }

    /// Sends every message before reading any answer.
    async fn send(&mut self, messages: &[Value]) {
        let text: String = messages.iter().map(|m| format!("{m}\n")).collect();
        self.write.write_all(text.as_bytes()).await.unwrap();
    }

    /// The next answer, skipping notifications. `None` once the daemon
    /// has closed the connection.
    async fn answer(&mut self) -> Option<Value> {
        loop {
            let line = tokio::time::timeout(WAIT, self.lines.next_line())
                .await
                .expect("timed out waiting for burrowd")
                .unwrap()?;
            let message: Value = serde_json::from_str(&line).unwrap();
            if message.get("method").is_none() {
                return Some(message);
            }
        }
    }
}

fn initialize(id: u64, api: u32, role: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "initialize",
        "params": {"api": api, "role": role, "build": "test"},
    })
}

fn execute(id: u64, command: &Command) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": "execute", "params": command})
}

/// Requests run on tasks of their own, so a second `initialize` sent
/// straight behind the first could once have won the race to set the
/// role.
#[tokio::test(flavor = "multi_thread")]
async fn a_second_initialize_never_changes_the_role() {
    let dir = TempDir::new().unwrap();
    let socket = serve(core(dir.path()), dir.path());
    for _ in 0..20 {
        let mut raw = Raw::open(&socket).await;
        raw.send(&[
            initialize(1, burrow_core::API_VERSION, "mcp"),
            initialize(2, burrow_core::API_VERSION, "gui"),
            execute(3, &folder("Work")),
        ])
        .await;
        let first = raw.answer().await.unwrap();
        assert_eq!(first["id"], 1);
        assert!(first.get("result").is_some(), "{first}");
        let mut answers = [raw.answer().await.unwrap(), raw.answer().await.unwrap()];
        answers.sort_by_key(|a| a["id"].as_u64());
        let [second, third] = answers;
        assert_eq!(second["error"]["message"], "already initialized");
        let refused = third["error"]["message"].as_str().unwrap_or_default();
        assert!(refused.contains("the MCP bridge may not"), "{third}");
    }
}

/// A connection whose `initialize` failed is closed, so a client refused
/// one way can't try again with something else.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_initialize_ends_the_connection() {
    let dir = TempDir::new().unwrap();
    let socket = serve(core(dir.path()), dir.path());
    let mut raw = Raw::open(&socket).await;
    raw.send(&[
        initialize(1, 99, "gui"),
        initialize(2, burrow_core::API_VERSION, "gui"),
        execute(3, &folder("Work")),
    ])
    .await;
    let refused = raw.answer().await.unwrap();
    assert_eq!(refused["id"], 1);
    let message = refused["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("needs API 99"), "{refused}");
    assert_eq!(raw.answer().await, None);

    // A role the daemon doesn't know is refused the same way.
    let mut raw = Raw::open(&socket).await;
    raw.send(&[
        initialize(1, burrow_core::API_VERSION, "root"),
        initialize(2, burrow_core::API_VERSION, "gui"),
    ])
    .await;
    let refused = raw.answer().await.unwrap();
    assert_eq!(refused["id"], 1);
    assert!(refused.get("error").is_some(), "{refused}");
    assert_eq!(raw.answer().await, None);
}
