//! A server that refuses a terminal, as a key installed with `restrict` or
//! `no-pty` makes it, ends the session with a reason instead of giving a
//! shell with no prompt or echo. Against a throwaway `sshd`.

use std::fs;

use burrow_core::{Command, Response};

mod common;
use common::*;

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_terminal_says_why() {
    let Some(server) = Server::start() else {
        return;
    };
    trust(&server);
    let key = fs::read_to_string(server.path("client_key.pub")).unwrap();
    fs::write(server.path("authorized_keys"), format!("restrict {key}")).unwrap();
    let core = server.core();
    let node = host(&core, "test").await;
    let mut events = core.subscribe();
    let command = Command::OpenTerminal {
        node: node.id,
        cols: 80,
        rows: 24,
        run: None,
    };
    let session = match core.execute(command).await.unwrap() {
        Response::Session { session } => session,
        other => panic!("expected a session, got {other:?}"),
    };
    let _output = core.take_output(session).unwrap();
    // Connected is only said once the terminal is granted, so a shell that
    // went ahead without one fails here.
    let state = expect_state(&mut events, session).await;
    let error = closed_error(state).expect("closed without a reason");
    assert!(error.contains("refused a terminal"), "{error}");
}
