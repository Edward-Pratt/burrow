//! An SSH agent that stops to ask before it signs, with nobody to answer:
//! signing in gives up and says why, instead of waiting until the session
//! times out. The agent is a real `ssh-agent` holding a key added with
//! `ssh-add -c`, so each signature needs confirming, and its askpass never
//! answers, as a passphrase dialog nobody is at doesn't. Against a
//! throwaway `sshd`; skipped when `/usr/bin/sshd` isn't installed.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::{Child, Command as Process, Stdio};
use std::time::{Duration, Instant};

use burrow_core::ipc::{Client, Role};
use burrow_core::{Capability, Command};

mod common;
use common::*;

/// Kills the agent when the test ends, however it ends.
struct Agent(Child);

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_agent_that_never_answers_stops_signing_in() {
    let Some(server) = Server::start() else {
        return;
    };
    trust(&server);
    // Waits longer than signing in may, then gives up by itself, so no
    // process outlives the test.
    let askpass = server.path("askpass");
    fs::write(&askpass, "#!/bin/sh\nexec sleep 60\n").unwrap();
    fs::set_permissions(&askpass, fs::Permissions::from_mode(0o700)).unwrap();
    let socket = server.path("agent.sock");
    let agent = Process::new("ssh-agent")
        .arg("-D")
        .arg("-a")
        .arg(&socket)
        .env("SSH_ASKPASS", &askpass)
        .env("SSH_ASKPASS_REQUIRE", "force")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _agent = Agent(agent);
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let status = Process::new("ssh-add")
        .args(["-q", "-c"])
        .arg(server.path("client_key"))
        .env("SSH_AUTH_SOCK", &socket)
        .status()
        .unwrap();
    assert!(status.success());
    let config = fs::read_to_string(server.path("ssh_config")).unwrap();
    let config = config.replace(
        "IdentityAgent none",
        &format!("IdentityAgent {}", socket.display()),
    );
    fs::write(server.path("ssh_config"), config).unwrap();

    // Through the MCP bridge, the way an assistant on a phone would find it.
    let core = server.core();
    let node = host(&core, "test").await;
    let socket = serve(core, server.dir.path());
    let gui = Client::connect(&socket, Role::Gui).await.unwrap();
    let mcp = Client::connect(&socket, Role::Mcp).await.unwrap();
    grant(&gui, &node, &[Capability::Exec]).await;

    let started = Instant::now();
    let command = Command::Exec {
        node: node.id,
        command: "true".into(),
        timeout: None,
    };
    let error = mcp.execute(command).await.unwrap_err().to_string();
    let took = started.elapsed();
    // The unattended limit, well before exec's own 60 seconds.
    assert!(took >= Duration::from_secs(14), "gave up after {took:?}");
    assert!(took < Duration::from_secs(30), "took {took:?}");
    assert!(error.contains("SSH agent didn't sign"), "{error}");
    assert!(error.contains("SHA256:"), "{error}");
    // It doesn't go on to the key file, whose passphrase isn't the problem.
    assert!(!error.contains("passphrase."), "{error}");
}
