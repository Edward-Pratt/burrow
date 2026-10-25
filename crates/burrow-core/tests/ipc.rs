//! The core served over a Unix socket, driven by [`Client`] as the GTK
//! window and the CLI drive it. The terminal tests need a throwaway `sshd`
//! and skip without one.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use burrow_core::ipc::{self, Client, Events, Role};
use burrow_core::{
    Command, Core, CoreOptions, Event, MemoryStore, Response, SessionId, SessionState,
};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

mod common;
use common::*;

/// Serves `core` on a socket in `dir`, for as long as the test runs.
fn serve(core: Core, dir: &Path) -> PathBuf {
    let path = dir.join("burrowd.sock");
    let listener = UnixListener::bind(&path).unwrap();
    tokio::spawn(ipc::serve(core, listener, None));
    path
}

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

async fn next_state(events: &mut Events, session: SessionId) -> SessionState {
    tokio::time::timeout(WAIT, async {
        loop {
            if let Event::SessionChanged {
                session: s, state, ..
            } = events.recv().await.unwrap()
                && s == session
                && !matches!(state, SessionState::Connecting { .. })
            {
                return state;
            }
        }
    })
    .await
    .expect("timed out waiting for the session")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_runs_commands_and_hears_events() {
    let dir = TempDir::new().unwrap();
    let socket = serve(core(dir.path()), dir.path());
    let client = Client::connect(&socket, Role::Gui).await.unwrap();
    assert_eq!(client.info().api, burrow_core::API_VERSION);
    assert_eq!(client.info().addons_dir, dir.path().join("burrow/addons"));
    let mut events = client.subscribe();

    let node = match client.execute(folder("Work")).await.unwrap() {
        Response::Node { node } => node,
        other => panic!("expected a node, got {other:?}"),
    };
    match tokio::time::timeout(WAIT, events.recv()).await.unwrap() {
        Ok(Event::NodeCreated { node: created }) => assert_eq!(created, node),
        other => panic!("expected node_created, got {other:?}"),
    }

    // Errors come back as the core put them.
    let error = client
        .execute(Command::GetNode {
            id: burrow_core::NodeId::new(),
        })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("not found"), "{error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_mcp_bridge_gets_nothing_without_grants() {
    let dir = TempDir::new().unwrap();
    let socket = serve(core(dir.path()), dir.path());
    let client = Client::connect(&socket, Role::Mcp).await.unwrap();
    let error = client.execute(Command::ListNodes).await.unwrap_err();
    assert!(error.to_string().contains("permission denied"), "{error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_must_say_hello_with_the_same_api() {
    let dir = TempDir::new().unwrap();
    let socket = serve(core(dir.path()), dir.path());
    let stream = UnixStream::connect(&socket).await.unwrap();
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    let mut ask = async |line: &str| {
        write
            .write_all(format!("{line}\n").as_bytes())
            .await
            .unwrap();
        lines.next_line().await.unwrap().unwrap()
    };

    let early =
        ask(r#"{"jsonrpc":"2.0","id":1,"method":"execute","params":{"type":"list_nodes"}}"#).await;
    assert!(early.contains("send initialize first"), "{early}");
    let wrong = ask(
        r#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"api":99,"role":"cli","build":"x"}}"#,
    )
    .await;
    assert!(wrong.contains("needs API 99"), "{wrong}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_terminal_runs_over_the_socket() {
    let Some(server) = Server::start() else {
        return;
    };
    trust(&server);
    let core = server.core();
    let node = host(&core, "test").await;
    let socket = serve(core, server.dir.path());
    let client = Client::connect(&socket, Role::Gui).await.unwrap();
    let mut events = client.subscribe();

    let command = Command::OpenTerminal {
        node: node.id,
        cols: 80,
        rows: 24,
    };
    let session = match client.execute(command).await.unwrap() {
        Response::Session { session } => session,
        other => panic!("expected a session, got {other:?}"),
    };
    let mut output = client.take_output(session).await.unwrap();
    assert_eq!(
        next_state(&mut events, session).await,
        SessionState::Connected
    );

    // Arithmetic, so the echoed command line doesn't match on its own.
    let write = |text: &str| Command::WriteTerminal {
        session,
        data: text.as_bytes().to_vec(),
    };
    client
        .execute(write("echo burrow-$((40 + 2))\n"))
        .await
        .unwrap();
    let mut seen = Vec::new();
    tokio::time::timeout(WAIT, async {
        while !String::from_utf8_lossy(&seen).contains("burrow-42") {
            seen.extend(output.recv().await.expect("output ended early"));
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no output in {:?}", String::from_utf8_lossy(&seen)));

    // As in-process, letting go of the output ends the session.
    drop(output);
    let _ = client.execute(write("echo bye\n")).await;
    assert!(matches!(
        next_state(&mut events, session).await,
        SessionState::Closed { .. }
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn another_client_can_answer_a_prompt() {
    let Some(server) = Server::start() else {
        return;
    };
    let core = server.core();
    let node = host(&core, "test").await;
    let socket = serve(core, server.dir.path());
    let gui = Client::connect(&socket, Role::Gui).await.unwrap();
    let cli = Client::connect(&socket, Role::Cli).await.unwrap();
    let mut gui_events = gui.subscribe();
    let mut cli_events = cli.subscribe();

    let command = Command::OpenTerminal {
        node: node.id,
        cols: 80,
        rows: 24,
    };
    let Response::Session { session } = cli.execute(command).await.unwrap() else {
        panic!("expected a session");
    };
    // The window hears the host key question and answers it.
    let prompt = tokio::time::timeout(WAIT, async {
        loop {
            if let Event::PromptRequested { session: s, prompt } = gui_events.recv().await.unwrap()
                && s == session
            {
                return prompt;
            }
        }
    })
    .await
    .unwrap();
    gui.execute(Command::AnswerPrompt {
        session,
        prompt: prompt.id,
        answer: burrow_core::PromptAnswer::Confirm,
    })
    .await
    .unwrap();

    // The CLI is told the question is gone, then that it connected.
    tokio::time::timeout(WAIT, async {
        loop {
            if let Event::PromptClosed {
                session: s,
                prompt: p,
            } = cli_events.recv().await.unwrap()
                && (s, p) == (session, prompt.id)
            {
                return;
            }
        }
    })
    .await
    .expect("no prompt_closed");
    assert_eq!(
        next_state(&mut cli_events, session).await,
        SessionState::Connected
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_clients_sessions_close_when_it_goes() {
    let Some(server) = Server::start() else {
        return;
    };
    trust(&server);
    let core = server.core();
    let node = host(&core, "test").await;
    let socket = serve(core, server.dir.path());
    let watcher = Client::connect(&socket, Role::Gui).await.unwrap();
    let mut events = watcher.subscribe();

    let client = Client::connect(&socket, Role::Cli).await.unwrap();
    let command = Command::OpenTerminal {
        node: node.id,
        cols: 80,
        rows: 24,
    };
    let Response::Session { session } = client.execute(command).await.unwrap() else {
        panic!("expected a session");
    };
    assert_eq!(
        next_state(&mut events, session).await,
        SessionState::Connected
    );
    drop(client);
    assert!(matches!(
        next_state(&mut events, session).await,
        SessionState::Closed { .. }
    ));
}
