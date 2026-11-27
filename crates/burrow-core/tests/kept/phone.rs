//! The phone uses only the sessions it opened and the terminals it kept,
//! and keeps using its own terminals after it reconnects.

use burrow_core::ipc::{Client, Role};
use burrow_core::{Command, PromptAnswer, SessionId};

use super::common::*;
use super::helpers::*;

/// Every way the phone could reach into a terminal, each of which must be
/// refused when the terminal isn't the phone's.
async fn refused_everything(phone: &Client, session: SessionId) {
    let commands = [
        Command::WriteTerminal {
            session,
            data: b"echo hi\n".to_vec(),
        },
        Command::ResizeTerminal {
            session,
            cols: 100,
            rows: 30,
        },
        Command::AnswerPrompt {
            session,
            prompt: 1,
            answer: PromptAnswer::Cancel,
        },
        Command::KeepTerminal { session },
        Command::CloseSession { session },
    ];
    for command in commands {
        let what = format!("{command:?}");
        let error = phone.execute(command).await.unwrap_err().to_string();
        assert!(error.contains("the phone may only use"), "{what}: {error}");
    }
    let error = phone.attach_output(session).await.unwrap_err().to_string();
    assert!(error.contains("the phone may only use"), "attach: {error}");
    let error = phone.take_output(session).await.unwrap_err().to_string();
    assert!(error.contains("the phone may only use"), "take: {error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_phone_may_not_use_another_clients_sessions() {
    let Some(server) = Server::start() else {
        return;
    };
    let (socket, node) = daemon(&server).await;
    let window = Client::connect(&socket, Role::Gui).await.unwrap();
    let cli = Client::connect(&socket, Role::Cli).await.unwrap();
    let phone = Client::connect(&socket, Role::Mobile).await.unwrap();

    let windows = kept(&window, &node).await;
    let clis = kept(&cli, &node).await;
    // Not kept yet: keeping it would make the phone its keeper.
    let open = terminal(&window, &node).await;
    for session in [windows, clis, open] {
        refused_everything(&phone, session).await;
    }
    assert_eq!(info(&window, open).await.unwrap().kept_by, None);
    assert!(info(&window, windows).await.is_some());

    // Not only terminals: the window's file panel isn't the phone's either.
    let files = open_and_wait(&window, Command::OpenFiles { node: node.id }).await;
    for command in [
        Command::ListDir {
            session: files,
            path: ".".into(),
        },
        Command::CloseSession { session: files },
    ] {
        let error = phone.execute(command).await.unwrap_err().to_string();
        assert!(error.contains("the phone may only use"), "{error}");
    }
    let mine = open_and_wait(&phone, Command::OpenFiles { node: node.id }).await;
    phone
        .execute(Command::CloseSession { session: mine })
        .await
        .unwrap();

    // An id that isn't running is refused too.
    let error = phone
        .execute(Command::CloseSession {
            session: SessionId(9999),
        })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("9999"), "{error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_phone_uses_its_kept_terminal_again_after_reconnecting() {
    let Some(server) = Server::start() else {
        return;
    };
    let (socket, node) = daemon(&server).await;
    let phone = Client::connect(&socket, Role::Mobile).await.unwrap();
    let session = kept(&phone, &node).await;
    drop(phone);

    let phone = Client::connect(&socket, Role::Mobile).await.unwrap();
    let (replay, mut output) = phone.attach_output(session).await.unwrap();
    write(&phone, session, "echo again-$((40 + 2))\n").await;
    read_until(&mut output, replay, "again-42").await;
    phone
        .execute(Command::ResizeTerminal {
            session,
            cols: 100,
            rows: 30,
        })
        .await
        .unwrap();
    phone
        .execute(Command::CloseSession { session })
        .await
        .unwrap();
}
