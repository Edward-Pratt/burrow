//! Attaching to a kept terminal: taking over, the replay, and what the
//! terminal said while nobody was attached.

use std::process::Command as Process;

use burrow_core::ipc::{Client, Role};
use burrow_core::{Command, Event, SessionId, SessionState};

use super::common::*;
use super::helpers::*;

#[tokio::test(flavor = "multi_thread")]
async fn attaching_again_takes_over_from_the_reader_before() {
    let Some(server) = Server::start() else {
        return;
    };
    let (socket, node) = daemon(&server).await;
    let old = Client::connect(&socket, Role::Mobile).await.unwrap();
    let new = Client::connect(&socket, Role::Mobile).await.unwrap();
    let session = kept(&old, &node).await;
    let (_, mut first) = old.attach_output(session).await.unwrap();
    let (_, mut second) = new.attach_output(session).await.unwrap();
    let ended = tokio::time::timeout(WAIT, async { while first.recv().await.is_some() {} }).await;
    assert!(ended.is_ok(), "the first reader's stream went on");

    write(&new, session, "echo taken-$((2 + 3))\n").await;
    read_until(&mut second, Vec::new(), "taken-5").await;
}

/// The numbers of the whole `L<n>` lines in `text`, in order.
fn numbered(text: &str) -> Vec<u32> {
    let lines = text.split("\r\n").skip(1);
    lines
        .filter_map(|l| l.strip_prefix('L')?.parse().ok())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn attaching_again_on_one_connection_repeats_nothing() {
    let Some(server) = Server::start() else {
        return;
    };
    let (socket, node) = daemon(&server).await;
    let phone = Client::connect(&socket, Role::Mobile).await.unwrap();
    let session = kept(&phone, &node).await;
    let (_, mut first) = phone.attach_output(session).await.unwrap();
    write(
        &phone,
        session,
        "for i in $(seq 1 4000); do echo L$i; done; echo done-$((1+1))\n",
    )
    .await;
    // Mid-stream, with output from the first attach still on its way.
    read_until(&mut first, Vec::new(), "L200\r\n").await;
    let (replay, mut second) = phone.attach_output(session).await.unwrap();
    let mut seen = replay;
    tokio::time::timeout(WAIT, async {
        while !String::from_utf8_lossy(&seen).contains("done-2") {
            seen.extend(second.recv().await.expect("output ended"));
        }
    })
    .await
    .expect("no end of the loop");
    let numbers = numbered(&String::from_utf8_lossy(&seen));
    assert_eq!(numbers.last(), Some(&4000));
    for pair in numbers.windows(2) {
        assert_eq!(pair[1], pair[0] + 1, "repeated or lost output");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn keeping_a_connected_terminal_says_so_again() {
    let Some(server) = Server::start() else {
        return;
    };
    let (socket, node) = daemon(&server).await;
    let phone = Client::connect(&socket, Role::Mobile).await.unwrap();
    let window = Client::connect(&socket, Role::Gui).await.unwrap();
    let session = terminal(&phone, &node).await;
    let mut events = window.subscribe();
    phone
        .execute(Command::KeepTerminal { session })
        .await
        .unwrap();
    let said = tokio::time::timeout(WAIT, async {
        loop {
            if let Ok(Event::SessionChanged {
                session: s,
                state: SessionState::Connected,
                ..
            }) = events.recv().await
                && s == session
            {
                return;
            }
        }
    })
    .await;
    assert!(said.is_ok(), "keeping it said nothing");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_question_asked_while_nobody_was_attached_is_asked_again() {
    let Some(server) = Server::start() else {
        return;
    };
    let status = Process::new("ssh-keygen")
        .args(["-q", "-p", "-P", "", "-N", "open sesame", "-f"])
        .arg(server.path("client_key"))
        .status()
        .unwrap();
    assert!(status.success());
    let (socket, node) = daemon(&server).await;
    let phone = Client::connect(&socket, Role::Mobile).await.unwrap();
    let mut phone_events = phone.subscribe();
    let open = Command::OpenTerminal {
        node: node.id,
        cols: 80,
        rows: 24,
        run: None,
    };
    let session = match phone.execute(open).await.unwrap() {
        burrow_core::Response::Session { session } => session,
        other => panic!("expected a session, got {other:?}"),
    };
    phone
        .execute(Command::KeepTerminal { session })
        .await
        .unwrap();
    let asked = asked(&mut phone_events, session).await;
    drop(phone);

    let again = Client::connect(&socket, Role::Mobile).await.unwrap();
    let mut events = again.subscribe();
    let _attached = again.attach_output(session).await.unwrap();
    assert_eq!(asked, self::asked(&mut events, session).await);
}

/// Waits for `session`'s next question, and returns its id.
async fn asked(events: &mut burrow_core::ipc::Events, session: SessionId) -> u32 {
    tokio::time::timeout(WAIT, async {
        loop {
            if let Ok(Event::PromptRequested { session: s, prompt }) = events.recv().await
                && s == session
            {
                return prompt.id;
            }
        }
    })
    .await
    .expect("nothing asked")
}

#[tokio::test(flavor = "multi_thread")]
async fn the_mcp_bridge_can_neither_keep_nor_attach() {
    let Some(server) = Server::start() else {
        return;
    };
    let (socket, node) = daemon(&server).await;
    let phone = Client::connect(&socket, Role::Mobile).await.unwrap();
    let mcp = Client::connect(&socket, Role::Mcp).await.unwrap();
    let session = terminal(&phone, &node).await;
    let keep = mcp.execute(Command::KeepTerminal { session }).await;
    assert!(keep.unwrap_err().to_string().contains("MCP"));
    phone
        .execute(Command::KeepTerminal { session })
        .await
        .unwrap();
    let attach = mcp.attach_output(session).await.unwrap_err().to_string();
    assert!(attach.contains("MCP"), "{attach}");
    let take = mcp.take_output(session).await.unwrap_err().to_string();
    assert!(take.contains("MCP"), "{take}");
}
