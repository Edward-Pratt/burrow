//! A kept terminal outlives its client, and ends when it is closed.

use std::time::Duration;

use burrow_core::ipc::{Client, Role};
use burrow_core::{Command, SessionState};

use super::common::*;
use super::helpers::*;

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

#[tokio::test(flavor = "multi_thread")]
async fn a_kept_terminal_names_the_role_that_kept_it_first() {
    let Some(server) = Server::start() else {
        return;
    };
    let (socket, node) = daemon(&server).await;
    let phone = Client::connect(&socket, Role::Mobile).await.unwrap();
    let cli = Client::connect(&socket, Role::Cli).await.unwrap();

    let phones = kept(&phone, &node).await;
    let clis = kept(&cli, &node).await;
    assert_eq!(
        info(&phone, phones).await.unwrap().kept_by,
        Some(Role::Mobile)
    );
    assert_eq!(info(&phone, clis).await.unwrap().kept_by, Some(Role::Cli));

    // Keeping it again doesn't make it another client's.
    cli.execute(Command::KeepTerminal { session: phones })
        .await
        .unwrap();
    assert_eq!(
        info(&cli, phones).await.unwrap().kept_by,
        Some(Role::Mobile)
    );
    // A terminal that isn't kept has no keeper.
    let open = terminal(&phone, &node).await;
    assert_eq!(info(&phone, open).await.unwrap().kept_by, None);
}
