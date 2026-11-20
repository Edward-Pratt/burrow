//! `burrow proxy` as an SSH forced command runs it, with burrow-api's
//! client on its stdin and stdout as the phone's would be: the phone's
//! path to the daemon, without the SSH hop.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use burrow_api::{Client, Command, Event, Response, Role};
use tempfile::TempDir;
use tokio::process::Child;

const WAIT: Duration = Duration::from_secs(15);

fn proxy(dir: &Path, role: &str) -> Child {
    tokio::process::Command::new(env!("CARGO_BIN_EXE_burrow"))
        .args(["proxy", "--role", role])
        .env("BURROW_RUNTIME_DIR", dir.join("run"))
        .env("BURROW_CONFIG_DIR", dir.join("config"))
        .env("BURROW_STATE_DIR", dir.join("state"))
        .env("BURROW_SSH_CONFIG", "/dev/null")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap()
}

/// The client over a proxy's stdout and stdin.
async fn over(child: &mut Child, role: Role) -> burrow_api::Result<Client> {
    let read = child.stdout.take().unwrap();
    let write = child.stdin.take().unwrap();
    tokio::time::timeout(WAIT, Client::over(read, write, role))
        .await
        .expect("timed out waiting for the proxy")
}

/// Stops the daemon a test started, even when the test fails.
struct Daemon<'a>(&'a Path);

impl Drop for Daemon<'_> {
    fn drop(&mut self) {
        std::process::Command::new(env!("CARGO_BIN_EXE_burrow"))
            .args(["daemon", "stop"])
            .env("BURROW_RUNTIME_DIR", self.0.join("run"))
            .output()
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_phone_reaches_the_daemon_through_the_proxy() {
    let dir = TempDir::new().unwrap();
    let dir = dir.path();
    let _daemon = Daemon(dir);

    let mut child = proxy(dir, "mobile");
    let client = match over(&mut child, Role::Mobile).await {
        Ok(client) => client,
        Err(e) if e.to_string().contains("secret store") => {
            eprintln!("skipping: no Secret Service ({e})");
            return;
        }
        Err(e) => panic!("the proxy refused a mobile client: {e}"),
    };
    assert_eq!(client.info().api, burrow_api::API_VERSION);
    let mut events = client.subscribe();
    let created = client
        .execute(Command::CreateNode {
            parent: None,
            kind: "folder".into(),
            name: "Phone".into(),
            color: None,
            props: toml::Table::new(),
        })
        .await
        .unwrap();
    let Response::Node { node } = created else {
        panic!("expected a node, got {created:?}");
    };
    match tokio::time::timeout(WAIT, events.recv()).await.unwrap() {
        Ok(Event::NodeCreated { node: heard }) => assert_eq!(heard.id, node.id),
        other => panic!("expected node_created, got {other:?}"),
    }

    // The phone going away ends the proxy.
    drop((client, events));
    let status = tokio::time::timeout(WAIT, child.wait()).await.unwrap();
    assert!(status.unwrap().success());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_key_for_one_role_cannot_connect_as_another() {
    let dir = TempDir::new().unwrap();
    let dir = dir.path();
    let _daemon = Daemon(dir);

    let mut child = proxy(dir, "mobile");
    let error = over(&mut child, Role::Gui).await.unwrap_err();
    assert!(
        error.to_string().contains("may only connect as mobile"),
        "{error}"
    );
    let output = tokio::time::timeout(WAIT, child.wait_with_output()).await;
    let output = output.unwrap().unwrap();
    assert!(!output.status.success());
    // Refused before the daemon was found or started.
    assert!(!dir.join("run/burrowd.sock").exists());
}
