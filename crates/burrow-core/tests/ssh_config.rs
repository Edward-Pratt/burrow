//! Hosts as the OpenSSH config describes them: `Include`d files, `Match`
//! blocks and proxies. Burrow asks `ssh -G` rather than reading the config
//! itself, so these pin that a host described the roundabout way is reached
//! as `ssh <alias>` would reach it, against a real `sshd` on 127.0.0.1.
//!
//! As in `ssh.rs`, nothing here reads `~/.ssh` or talks to the user's
//! agent. Skipped (with a note) when `/usr/bin/sshd` isn't installed.

use std::fs;

use burrow_core::{Command, Core, Event, Node, Response, SessionId, SessionState};

mod common;
use common::*;

/// Replaces the config `Server::start` wrote. `{port}` is the server's
/// port, `{closed}` one nothing listens on, `{dir}` the test's folder, and
/// `{options}` what a host needs to sign in to the server with nothing of
/// the user's: the test key, and no agent or `known_hosts` of theirs.
/// Returns `{closed}`.
fn configure(server: &Server, config: &str) -> u16 {
    let closed = free_port();
    fs::write(server.path("ssh_config"), fill(server, config, closed)).unwrap();
    closed
}

fn fill(server: &Server, text: &str, closed: u16) -> String {
    let options = format!(
        "\tUser {user}\n\
         \tIdentityFile {key}\n\
         \tIdentitiesOnly yes\n\
         \tIdentityAgent none\n\
         \tUserKnownHostsFile {known}\n\
         \tGlobalKnownHostsFile /dev/null\n",
        user = whoami(),
        key = server.path("client_key").display(),
        known = server.path("known_hosts").display(),
    );
    text.replace("{options}", &options)
        .replace("{port}", &server.port.to_string())
        .replace("{closed}", &closed.to_string())
        .replace("{dir}", server.dir.path().to_str().unwrap())
}

/// An SSH host node for `alias`, named after it.
async fn node(core: &Core, alias: &str) -> Node {
    let command = Command::CreateNode {
        parent: None,
        kind: "ssh-host".into(),
        name: alias.into(),
        color: None,
        props: toml::Table::new(),
    };
    match core.execute(command).await.unwrap() {
        Response::Node { node } => node,
        other => panic!("expected a node, got {other:?}"),
    }
}

/// Runs a command on `alias` and returns the port of the server it ran
/// on, or why it didn't get there.
async fn reached(core: &Core, alias: &str) -> Result<u16, String> {
    let command = Command::Exec {
        node: node(core, alias).await.id,
        command: "echo $SSH_CONNECTION".into(),
        timeout: Some(15),
    };
    match core.execute(command).await {
        Ok(Response::Exec { result }) => {
            let port = result.stdout.split_whitespace().last().map(str::parse);
            Ok(port.expect("no SSH_CONNECTION").unwrap())
        }
        Ok(other) => panic!("expected an exec result, got {other:?}"),
        Err(error) => Err(error.to_string()),
    }
}

/// Follows `session` until it closes. Returns the steps it went through
/// and the error it closed with.
async fn refused(
    events: &mut tokio::sync::broadcast::Receiver<Event>,
    session: SessionId,
) -> (Vec<String>, String) {
    let mut steps = Vec::new();
    tokio::time::timeout(WAIT, async {
        loop {
            match events.recv().await.unwrap() {
                Event::SessionChanged {
                    session: s, state, ..
                } if s == session => match state {
                    SessionState::Connecting { step } => steps.push(step),
                    state => return (steps, closed_error(state).expect("it closed cleanly")),
                },
                Event::PromptRequested { prompt, .. } => panic!("asked {prompt:?}"),
                _ => {}
            }
        }
    })
    .await
    .expect("timed out waiting for the session to close")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_in_an_included_file_is_reached() {
    let Some(server) = Server::start() else {
        return;
    };
    trust(&server);
    // The port comes from a file matched by a glob, the rest from the
    // file that includes it. Two decoys would send it elsewhere: a file
    // the glob doesn't match, and an Include inside another host's block,
    // which ssh reads but doesn't apply.
    let closed = configure(
        &server,
        "Include {dir}/conf.d/*.conf\n\
         Host elsewhere\n\
         \tInclude {dir}/elsewhere.conf\n\
         Host *\n\
         \tHostName 127.0.0.1\n\
         \tPort {closed}\n\
         {options}",
    );
    fs::create_dir(server.path("conf.d")).unwrap();
    let lab = fill(&server, "Host lab\n\tPort {port}\n", closed);
    fs::write(server.path("conf.d/10-lab.conf"), lab).unwrap();
    let old = fill(&server, "Host lab\n\tPort {closed}\n", closed);
    fs::write(server.path("conf.d/00-old.conf.bak"), old).unwrap();
    let elsewhere = "Host lab\n\tHostName lab.invalid\n";
    fs::write(server.path("elsewhere.conf"), elsewhere).unwrap();

    let core = server.core();
    assert_eq!(reached(&core, "lab").await, Ok(server.port));
}

#[tokio::test(flavor = "multi_thread")]
async fn match_blocks_apply_as_ssh_applies_them() {
    let Some(server) = Server::start() else {
        return;
    };
    trust(&server);
    // Only a Match block gives either host the server's port. The first
    // names another host, so it must not apply. The second needs its
    // command to succeed. The third matches on the address `HostName` gave,
    // which only holds below the Host block.
    let closed = configure(
        &server,
        "Match originalhost elsewhere\n\
         \tPort {closed}\n\
         Match originalhost gated exec \"test -e {dir}/open\"\n\
         \tPort {port}\n\
         Host gated byaddress\n\
         \tHostName 127.0.0.1\n\
         Match host 127.0.0.1 originalhost byaddress\n\
         \tPort {port}\n\
         Host *\n\
         \tPort {closed}\n\
         {options}",
    );
    let core = server.core();
    assert_eq!(reached(&core, "byaddress").await, Ok(server.port));

    let error = reached(&core, "gated").await.unwrap_err();
    assert!(
        error.contains(&format!("could not connect to 127.0.0.1:{closed}")),
        "{error}"
    );
    // The config is read again for each connection, and its command run.
    fs::write(server.path("open"), "").unwrap();
    assert_eq!(reached(&core, "gated").await, Ok(server.port));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_behind_a_proxy_jump_is_refused_before_connecting() {
    let Some(server) = Server::start() else {
        return;
    };
    trust(&server);
    // `inner` could be reached directly here, which is what ignoring its
    // ProxyJump would do: on a real network that reaches the wrong machine
    // or hangs.
    configure(
        &server,
        "Host direct\n\
         \tProxyJump none\n\
         Host bastion direct inner\n\
         \tHostName 127.0.0.1\n\
         \tPort {port}\n\
         Host * !bastion\n\
         \tProxyJump bastion\n\
         Host *\n\
         {options}",
    );
    let core = server.core();
    let inner = node(&core, "inner").await;
    let mut events = core.subscribe();
    let said = "inner connects through ProxyJump bastion, which Burrow doesn't support yet";

    // Every kind of session connects the same way; two of them here.
    let terminal = Command::OpenTerminal {
        node: inner.id,
        cols: 80,
        rows: 24,
        run: None,
    };
    for command in [terminal, Command::OpenFiles { node: inner.id }] {
        let session = match core.execute(command).await.unwrap() {
            Response::Session { session } => session,
            other => panic!("expected a session, got {other:?}"),
        };
        let (steps, error) = refused(&mut events, session).await;
        assert_eq!(error, said);
        assert!(
            !steps.iter().any(|s| s.starts_with("Connecting")),
            "{steps:?}"
        );
    }
    assert_eq!(reached(&core, "inner").await.unwrap_err(), said);

    // The jump host itself, and a host that turns the jump off, are plain
    // hosts.
    assert_eq!(reached(&core, "bastion").await, Ok(server.port));
    assert_eq!(reached(&core, "direct").await, Ok(server.port));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_with_a_proxy_command_is_refused_and_the_command_never_runs() {
    let Some(server) = Server::start() else {
        return;
    };
    trust(&server);
    configure(
        &server,
        "Host piped\n\
         \tHostName 127.0.0.1\n\
         \tPort {port}\n\
         \tProxyCommand touch {dir}/ran\n\
         {options}",
    );
    let core = server.core();
    assert_eq!(
        reached(&core, "piped").await.unwrap_err(),
        "piped uses a ProxyCommand, which Burrow doesn't support yet"
    );
    assert!(!server.path("ran").exists());
}
