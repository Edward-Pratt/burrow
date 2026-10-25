//! A throwaway `sshd` for integration tests, started on a free port of
//! 127.0.0.1 with its own keys, and helpers for following a session's
//! events. Each test file uses only some of it.
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as Process, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use burrow_core::{
    Command, Core, CoreOptions, Event, MemoryStore, Node, Prompt, PromptAnswer, Response,
    SessionId, SessionState,
};
use tempfile::TempDir;
use tokio::sync::broadcast;

pub const SSHD: &str = "/usr/bin/sshd";
pub const WAIT: Duration = Duration::from_secs(15);

pub struct Server {
    pub dir: TempDir,
    pub port: u16,
    sshd: Child,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.sshd.kill();
        let _ = self.sshd.wait();
    }
}

pub fn keygen(path: &Path) {
    let status = Process::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", "", "-f"])
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success());
}

/// The ports tests listen on. They sit below the range the kernel gives
/// to outgoing connections (32768 and up, by default), since a connection
/// a test makes could otherwise take a port between it being picked and
/// the test's own bind. Each goes out once per test binary, as tests run
/// side by side.
const FIRST_PORT: u16 = 20000;
const LAST_PORT: u16 = 32767;

/// A port nothing listens on.
pub fn free_port() -> u16 {
    static NEXT: Mutex<u16> = Mutex::new(FIRST_PORT);
    let mut next = NEXT.lock().unwrap();
    loop {
        let port = *next;
        assert!(port <= LAST_PORT, "the tests have used every port");
        *next += 1;
        // Something else may listen there; a connection closed earlier in
        // TIME_WAIT doesn't count, since std sets SO_REUSEADDR as sshd,
        // postgres and the core do.
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
}

impl Server {
    /// `None` when sshd isn't installed.
    pub fn start() -> Option<Server> {
        if !Path::new(SSHD).exists() {
            eprintln!("skipping: {SSHD} is not installed");
            return None;
        }
        let dir = TempDir::new().unwrap();
        let root = dir.path().to_path_buf();
        let path = |name: &str| root.join(name);
        keygen(&path("host_key"));
        keygen(&path("client_key"));
        fs::copy(path("client_key.pub"), path("authorized_keys")).unwrap();

        let port = free_port();
        fs::write(
            path("sshd_config"),
            format!(
                "Port {port}\n\
                 ListenAddress 127.0.0.1\n\
                 HostKey {host_key}\n\
                 AuthorizedKeysFile {authorized}\n\
                 PasswordAuthentication no\n\
                 KbdInteractiveAuthentication no\n\
                 UsePAM no\n\
                 StrictModes no\n\
                 PidFile none\n\
                 PerSourcePenalties no\n\
                 Subsystem sftp internal-sftp\n\
                 LogLevel ERROR\n",
                host_key = path("host_key").display(),
                authorized = path("authorized_keys").display(),
            ),
        )
        .unwrap();
        // sshd re-executes itself, so it must be started by absolute path.
        let sshd = Process::new(SSHD)
            .args(["-D", "-e", "-f"])
            .arg(path("sshd_config"))
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let server = Server { dir, port, sshd };
        server.wait_until_listening();

        fs::write(
            path("ssh_config"),
            format!(
                "Host test\n\
                 \tHostName 127.0.0.1\n\
                 \tPort {port}\n\
                 \tUser {user}\n\
                 \tIdentityFile {key}\n\
                 \tIdentitiesOnly yes\n\
                 \tIdentityAgent none\n\
                 \tUserKnownHostsFile {known}\n\
                 \tGlobalKnownHostsFile /dev/null\n\
                 \tStrictHostKeyChecking ask\n",
                user = whoami(),
                key = path("client_key").display(),
                known = path("known_hosts").display(),
            ),
        )
        .unwrap();
        Some(server)
    }

    fn wait_until_listening(&self) {
        for _ in 0..100 {
            if std::net::TcpStream::connect(("127.0.0.1", self.port)).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("sshd did not start on port {}", self.port);
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    pub fn known_hosts(&self) -> String {
        fs::read_to_string(self.path("known_hosts")).unwrap_or_default()
    }

    pub fn core(&self) -> Core {
        Core::open(CoreOptions {
            config_dir: self.path("burrow"),
            state_dir: self.path("state"),
            secrets: Arc::new(MemoryStore::new()),
            ssh_config: Some(self.path("ssh_config")),
        })
        .unwrap()
    }
}

pub fn whoami() -> String {
    let output = Process::new("id").arg("-un").output().unwrap();
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

pub async fn host(core: &Core, alias: &str) -> Node {
    let mut props = toml::Table::new();
    props.insert("alias".into(), alias.into());
    let command = Command::CreateNode {
        parent: None,
        kind: "ssh-host".into(),
        name: "test".into(),
        color: None,
        props,
    };
    match core.execute(command).await.unwrap() {
        Response::Node { node } => node,
        other => panic!("expected a node, got {other:?}"),
    }
}

pub enum Next {
    Prompt(Prompt),
    State(SessionState),
}

/// The next prompt, or the next state other than "connecting".
pub async fn next(events: &mut broadcast::Receiver<Event>, session: SessionId) -> Next {
    tokio::time::timeout(WAIT, async {
        loop {
            match events.recv().await.unwrap() {
                Event::PromptRequested { session: s, prompt } if s == session => {
                    return Next::Prompt(prompt);
                }
                Event::SessionChanged {
                    session: s, state, ..
                } if s == session && !matches!(state, SessionState::Connecting { .. }) => {
                    return Next::State(state);
                }
                _ => {}
            }
        }
    })
    .await
    .expect("timed out waiting for the session")
}

pub async fn expect_prompt(events: &mut broadcast::Receiver<Event>, session: SessionId) -> Prompt {
    match next(events, session).await {
        Next::Prompt(prompt) => prompt,
        Next::State(state) => panic!("expected a prompt, got {state:?}"),
    }
}

pub async fn expect_state(
    events: &mut broadcast::Receiver<Event>,
    session: SessionId,
) -> SessionState {
    match next(events, session).await {
        Next::State(state) => state,
        Next::Prompt(prompt) => panic!("expected a state, got prompt {prompt:?}"),
    }
}

pub async fn answer(core: &Core, session: SessionId, prompt: &Prompt, answer: PromptAnswer) {
    let command = Command::AnswerPrompt {
        session,
        prompt: prompt.id,
        answer,
    };
    core.execute(command).await.unwrap();
}

pub fn closed_error(state: SessionState) -> Option<String> {
    match state {
        SessionState::Closed { error } => error,
        other => panic!("expected closed, got {other:?}"),
    }
}

/// Writes the server's host key to known_hosts, so connecting asks nothing.
pub fn trust(server: &Server) {
    let host_key = fs::read_to_string(server.path("host_key.pub")).unwrap();
    fs::write(
        server.path("known_hosts"),
        format!("[127.0.0.1]:{} {host_key}", server.port),
    )
    .unwrap();
}

/// Serves `core` on a socket in `dir`, for as long as the test runs.
pub fn serve(core: Core, dir: &Path) -> PathBuf {
    let path = dir.join("burrowd.sock");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    tokio::spawn(burrow_core::ipc::serve(core, listener, None));
    path
}

/// Grants `capabilities` on `node` from `client`, as the user would after
/// seeing it.
pub async fn grant(
    client: &burrow_core::ipc::Client,
    node: &Node,
    capabilities: &[burrow_core::Capability],
) {
    let nodes = match client.execute(Command::ListNodes).await.unwrap() {
        Response::Nodes { nodes } => nodes,
        other => panic!("expected nodes, got {other:?}"),
    };
    let command = Command::SetGrant {
        node: node.id,
        capabilities: capabilities.iter().copied().collect(),
        expect: Some(burrow_core::GrantShown::of(node, &nodes)),
    };
    client.execute(command).await.unwrap();
}

/// Opens a session from `client` and waits, by listing sessions as the MCP
/// bridge does, until it is connected.
pub async fn open_and_wait(client: &burrow_core::ipc::Client, command: Command) -> SessionId {
    let session = match client.execute(command).await.unwrap() {
        Response::Session { session } => session,
        other => panic!("expected a session, got {other:?}"),
    };
    tokio::time::timeout(WAIT, async {
        loop {
            if let Response::Sessions { sessions } =
                client.execute(Command::ListSessions).await.unwrap()
            {
                match sessions
                    .iter()
                    .find(|s| s.session == session)
                    .map(|s| &s.state)
                {
                    Some(SessionState::Connected) => return session,
                    Some(SessionState::Connecting { .. }) => {}
                    other => panic!("session ended: {other:?}"),
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("timed out waiting for the session")
}
