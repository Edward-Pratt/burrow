//! Kept terminals outlive the client that opened them, keep their output
//! meanwhile, and let a client attach to them again. Against a throwaway
//! `sshd`, over the socket, as the phone uses them.

use std::time::Duration;

use burrow_core::ipc::{Client, Role};
use burrow_core::{Command, SessionId, SessionInfo, SessionState};
use tokio::sync::mpsc;

mod common;
use common::*;

async fn terminal(client: &Client, node: &burrow_core::Node) -> SessionId {
    let command = Command::OpenTerminal {
        node: node.id,
        cols: 80,
        rows: 24,
        run: None,
    };
    open_and_wait(client, command).await
}

async fn write(client: &Client, session: SessionId, text: &str) {
    let command = Command::WriteTerminal {
        session,
        data: text.as_bytes().to_vec(),
    };
    client.execute(command).await.unwrap();
}

async fn info(client: &Client, session: SessionId) -> Option<SessionInfo> {
    match client.execute(Command::ListSessions).await.unwrap() {
        burrow_core::Response::Sessions { sessions } => {
            sessions.into_iter().find(|s| s.session == session)
        }
        other => panic!("expected sessions, got {other:?}"),
    }
}

/// Reads until the output, after `seen`, contains `needle`.
async fn read_until(output: &mut mpsc::Receiver<Vec<u8>>, mut seen: Vec<u8>, needle: &str) {
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
async fn until(client: &Client, session: SessionId, ready: impl Fn(&SessionInfo) -> bool) {
    tokio::time::timeout(WAIT, async {
        while !info(client, session).await.as_ref().is_some_and(&ready) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("timed out waiting on the session");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_kept_terminal_outlives_its_client_and_replays_its_output() {
    let Some(server) = Server::start() else {
        return;
    };
    trust(&server);
    let core = server.core();
    let node = host(&core, "test").await;
    let socket = serve(core, server.dir.path());

    let phone = Client::connect(&socket, Role::Mobile).await.unwrap();
    let session = terminal(&phone, &node).await;
    phone
        .execute(Command::KeepTerminal { session })
        .await
        .unwrap();
    let (_, mut output) = phone.attach_output(session).await.unwrap();
    // Printed after the phone has gone, so only the ring can hold it.
    write(&phone, session, "sleep 1; echo kept-$((40 + 2))\n").await;
    read_until(&mut output, Vec::new(), "sleep 1").await;
    drop(output);
    drop(phone);

    let window = Client::connect(&socket, Role::Gui).await.unwrap();
    until(&window, session, |s| s.kept && !s.attached).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let (replay, mut output) = window.attach_output(session).await.unwrap();
    read_until(&mut output, replay, "kept-42").await;
    assert!(info(&window, session).await.unwrap().attached);

    // Still a live shell.
    write(&window, session, "echo again-$((1 + 1))\n").await;
    read_until(&mut output, Vec::new(), "again-2").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn attaching_again_takes_over_from_the_reader_before() {
    let Some(server) = Server::start() else {
        return;
    };
    trust(&server);
    let core = server.core();
    let node = host(&core, "test").await;
    let socket = serve(core, server.dir.path());

    let old = Client::connect(&socket, Role::Mobile).await.unwrap();
    let new = Client::connect(&socket, Role::Mobile).await.unwrap();
    let session = terminal(&old, &node).await;
    old.execute(Command::KeepTerminal { session })
        .await
        .unwrap();
    let (_, mut first) = old.attach_output(session).await.unwrap();
    let (_, mut second) = new.attach_output(session).await.unwrap();
    let ended = tokio::time::timeout(WAIT, async { while first.recv().await.is_some() {} }).await;
    assert!(ended.is_ok(), "the first reader's stream went on");

    write(&new, session, "echo taken-$((2 + 3))\n").await;
    read_until(&mut second, Vec::new(), "taken-5").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_terminal_not_kept_still_closes_with_its_client() {
    let Some(server) = Server::start() else {
        return;
    };
    trust(&server);
    let core = server.core();
    let node = host(&core, "test").await;
    let socket = serve(core, server.dir.path());

    let phone = Client::connect(&socket, Role::Mobile).await.unwrap();
    let window = Client::connect(&socket, Role::Gui).await.unwrap();
    let session = terminal(&phone, &node).await;
    let _output = phone.take_output(session).await.unwrap();
    let error = phone
        .execute(Command::KeepTerminal { session })
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("already taken"), "{error}");
    drop(phone);
    tokio::time::timeout(WAIT, async {
        while info(&window, session).await.is_some() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the terminal outlived its client");
}

#[tokio::test(flavor = "multi_thread")]
async fn closing_a_kept_terminal_ends_it() {
    let Some(server) = Server::start() else {
        return;
    };
    trust(&server);
    let core = server.core();
    let node = host(&core, "test").await;
    let socket = serve(core, server.dir.path());

    let phone = Client::connect(&socket, Role::Mobile).await.unwrap();
    let session = terminal(&phone, &node).await;
    phone
        .execute(Command::KeepTerminal { session })
        .await
        .unwrap();
    drop(phone);

    let window = Client::connect(&socket, Role::Gui).await.unwrap();
    until(&window, session, |s| s.kept).await;
    let mut events = window.subscribe();
    window
        .execute(Command::CloseSession { session })
        .await
        .unwrap();
    tokio::time::timeout(WAIT, async {
        loop {
            if let Ok(burrow_core::Event::SessionChanged {
                session: s,
                state: SessionState::Closed { .. },
                ..
            }) = events.recv().await
                && s == session
            {
                return;
            }
        }
    })
    .await
    .expect("no close");
    assert!(info(&window, session).await.is_none());
}
