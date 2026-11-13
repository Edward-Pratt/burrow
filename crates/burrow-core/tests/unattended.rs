//! Sessions the MCP bridge starts have nobody to answer their questions,
//! so they fail at the first one and say what it was, rather than wait
//! until they time out. Against a throwaway `sshd`, over the socket.

use std::process::Command as Process;
use std::time::{Duration, Instant};

use burrow_core::ipc::{Client, Events, Role};
use burrow_core::{Capability, Command, Event, Response, SessionId, SessionState};

mod common;
use common::*;

/// Well inside exec's default timeout of 60 seconds, which is how long a
/// question used to hold the bridge up.
const PROMPT_FREE: Duration = Duration::from_secs(10);

fn encrypt_key(server: &Server) {
    let status = Process::new("ssh-keygen")
        .args(["-q", "-p", "-P", "", "-N", "open sesame", "-f"])
        .arg(server.path("client_key"))
        .status()
        .unwrap();
    assert!(status.success());
}

/// A GUI and an MCP client of one daemon, with `capabilities` granted on
/// an SSH host node for the server.
async fn clients(
    server: &Server,
    capabilities: &[Capability],
) -> (Client, Client, burrow_core::Node) {
    let core = server.core();
    let node = host(&core, "test").await;
    let socket = serve(core, server.dir.path());
    let gui = Client::connect(&socket, Role::Gui).await.unwrap();
    let mcp = Client::connect(&socket, Role::Mcp).await.unwrap();
    grant(&gui, &node, capabilities).await;
    (gui, mcp, node)
}

/// The events already sent, and any that come within a moment.
async fn drain(events: &mut Events) -> Vec<Event> {
    let mut seen = Vec::new();
    while let Ok(Ok(event)) = tokio::time::timeout(Duration::from_millis(300), events.recv()).await
    {
        seen.push(event);
    }
    seen
}

/// Fails if the client heard a question since `events` was subscribed.
async fn assert_nothing_asked(events: &mut Events) {
    for event in drain(events).await {
        assert!(
            !matches!(event, Event::PromptRequested { .. }),
            "asked {event:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_command_fails_at_once_on_a_key_passphrase() {
    let Some(server) = Server::start() else {
        return;
    };
    trust(&server);
    encrypt_key(&server);
    let (gui, mcp, node) = clients(&server, &[Capability::Exec]).await;
    let mut gui_events = gui.subscribe();

    let started = Instant::now();
    let command = Command::Exec {
        node: node.id,
        command: "true".into(),
        timeout: None,
    };
    let error = mcp.execute(command).await.unwrap_err().to_string();
    assert!(
        started.elapsed() < PROMPT_FREE,
        "took {:?}",
        started.elapsed()
    );
    assert!(error.contains("\"Key passphrase\""), "{error}");
    assert!(error.contains("ssh-add"), "{error}");
    // The window wasn't asked on the bridge's behalf either.
    assert_nothing_asked(&mut gui_events).await;

    // The window still asks, for itself.
    let command = Command::OpenTerminal {
        node: node.id,
        cols: 80,
        rows: 24,
        run: None,
    };
    let Response::Session { session } = gui.execute(command).await.unwrap() else {
        panic!("expected a session");
    };
    let prompt = tokio::time::timeout(WAIT, async {
        loop {
            if let Ok(Event::PromptRequested { session: s, prompt }) = gui_events.recv().await
                && s == session
            {
                return prompt;
            }
        }
    })
    .await
    .expect("the window's own session asked nothing");
    assert_eq!(prompt.title, "Key passphrase");
}

/// The error a session the bridge opened closed with, from its events.
async fn closed_with(events: &mut Events, session: SessionId) -> Option<String> {
    tokio::time::timeout(WAIT, async {
        loop {
            match events.recv().await {
                Ok(Event::SessionChanged {
                    session: s,
                    state: SessionState::Closed { error },
                    ..
                }) if s == session => return error,
                Ok(_) => {}
                Err(e) => panic!("events ended: {e}"),
            }
        }
    })
    .await
    .expect("the session never closed")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_host_key_closes_the_session_and_trusts_nothing() {
    let Some(server) = Server::start() else {
        return;
    };
    let (gui, mcp, node) = clients(&server, &[Capability::FilesRead]).await;
    let mut gui_events = gui.subscribe();
    let mut mcp_events = mcp.subscribe();

    let started = Instant::now();
    let command = Command::OpenFiles { node: node.id };
    let Response::Session { session } = mcp.execute(command).await.unwrap() else {
        panic!("expected a session");
    };
    // The bridge hears why its own session closed.
    let error = closed_with(&mut mcp_events, session).await.unwrap();
    assert!(
        started.elapsed() < PROMPT_FREE,
        "took {:?}",
        started.elapsed()
    );
    assert!(error.contains("not trusted"), "{error}");
    assert!(error.contains("\"Trust this host?\""), "{error}");
    assert!(error.contains("once in Burrow"), "{error}");
    assert_eq!(server.known_hosts(), "");
    assert_nothing_asked(&mut gui_events).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_bridge_hears_only_its_own_sessions() {
    let Some(server) = Server::start() else {
        return;
    };
    trust(&server);
    let (gui, mcp, node) = clients(&server, &[Capability::FilesRead]).await;
    let mut mcp_events = mcp.subscribe();

    // The window's session, which the bridge must not hear about.
    let theirs = open_and_wait(&gui, Command::OpenFiles { node: node.id }).await;
    let ours = open_and_wait(&mcp, Command::OpenFiles { node: node.id }).await;
    for session in [theirs, ours] {
        gui.execute(Command::CloseSession { session })
            .await
            .unwrap();
    }
    let heard: Vec<SessionId> = drain(&mut mcp_events)
        .await
        .into_iter()
        .filter_map(|event| match event {
            Event::SessionChanged { session, .. } => Some(session),
            _ => None,
        })
        .collect();
    assert!(heard.contains(&ours), "{heard:?}");
    assert!(!heard.contains(&theirs), "{heard:?}");
}
