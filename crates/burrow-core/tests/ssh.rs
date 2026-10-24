//! SSH sessions against a real `sshd` started for each test on a free port
//! of 127.0.0.1, with throwaway keys. Nothing here reads `~/.ssh` or talks
//! to the user's agent: the client config sets `IdentityAgent none` and its
//! own `UserKnownHostsFile`.
//!
//! Skipped (with a note) when `/usr/bin/sshd` isn't installed.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as Process, Stdio};
use std::sync::Arc;
use std::time::Duration;

use burrow_core::{
    Command, Core, CoreOptions, Event, MemoryStore, Node, Prompt, PromptAnswer, PromptKind,
    Response, SecretValue, SessionId, SessionState,
};
use tempfile::TempDir;
use tokio::sync::{broadcast, mpsc};

const SSHD: &str = "/usr/bin/sshd";
const WAIT: Duration = Duration::from_secs(15);

struct Server {
    dir: TempDir,
    port: u16,
    sshd: Child,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.sshd.kill();
        let _ = self.sshd.wait();
    }
}

fn keygen(path: &Path) {
    let status = Process::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", "", "-f"])
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success());
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

impl Server {
    /// `None` when sshd isn't installed.
    fn start() -> Option<Server> {
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

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn known_hosts(&self) -> String {
        fs::read_to_string(self.path("known_hosts")).unwrap_or_default()
    }

    fn core(&self) -> Core {
        Core::open(CoreOptions {
            config_dir: self.path("burrow"),
            state_dir: self.path("state"),
            secrets: Arc::new(MemoryStore::new()),
            ssh_config: Some(self.path("ssh_config")),
        })
        .unwrap()
    }
}

fn whoami() -> String {
    let output = Process::new("id").arg("-un").output().unwrap();
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

async fn host(core: &Core, alias: &str) -> Node {
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

async fn open(core: &Core, node: &Node) -> (SessionId, mpsc::Receiver<Vec<u8>>) {
    let command = Command::OpenTerminal {
        node: node.id,
        cols: 80,
        rows: 24,
    };
    let session = match core.execute(command).await.unwrap() {
        Response::Session { session } => session,
        other => panic!("expected a session, got {other:?}"),
    };
    (session, core.take_output(session).unwrap())
}

enum Next {
    Prompt(Prompt),
    State(SessionState),
}

/// The next prompt, or the next state other than "connecting".
async fn next(events: &mut broadcast::Receiver<Event>, session: SessionId) -> Next {
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

async fn expect_prompt(events: &mut broadcast::Receiver<Event>, session: SessionId) -> Prompt {
    match next(events, session).await {
        Next::Prompt(prompt) => prompt,
        Next::State(state) => panic!("expected a prompt, got {state:?}"),
    }
}

async fn expect_state(events: &mut broadcast::Receiver<Event>, session: SessionId) -> SessionState {
    match next(events, session).await {
        Next::State(state) => state,
        Next::Prompt(prompt) => panic!("expected a state, got prompt {prompt:?}"),
    }
}

async fn answer(core: &Core, session: SessionId, prompt: &Prompt, answer: PromptAnswer) {
    let command = Command::AnswerPrompt {
        session,
        prompt: prompt.id,
        answer,
    };
    core.execute(command).await.unwrap();
}

async fn write(core: &Core, session: SessionId, text: &str) {
    let command = Command::WriteTerminal {
        session,
        data: text.as_bytes().to_vec(),
    };
    core.execute(command).await.unwrap();
}

/// Reads output until it contains `needle`.
async fn read_until(output: &mut mpsc::Receiver<Vec<u8>>, needle: &str) -> String {
    let mut seen = Vec::new();
    tokio::time::timeout(WAIT, async {
        while let Some(chunk) = output.recv().await {
            seen.extend(chunk);
            if String::from_utf8_lossy(&seen).contains(needle) {
                return;
            }
        }
        panic!("output ended before {needle:?}");
    })
    .await
    .unwrap_or_else(|_| panic!("no {needle:?} in {:?}", String::from_utf8_lossy(&seen)));
    String::from_utf8_lossy(&seen).into_owned()
}

fn closed_error(state: SessionState) -> Option<String> {
    match state {
        SessionState::Closed { error } => error,
        other => panic!("expected closed, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn trusting_a_new_host_runs_a_shell() {
    let Some(server) = Server::start() else {
        return;
    };
    let core = server.core();
    let node = host(&core, "test").await;
    let mut events = core.subscribe();
    let (session, mut output) = open(&core, &node).await;

    let prompt = expect_prompt(&mut events, session).await;
    assert_eq!(prompt.kind, PromptKind::Confirm);
    assert!(prompt.message.contains("SHA256:"), "{}", prompt.message);
    answer(&core, session, &prompt, PromptAnswer::Confirm).await;
    assert_eq!(
        expect_state(&mut events, session).await,
        SessionState::Connected
    );
    assert!(
        server
            .known_hosts()
            .starts_with(&format!("[127.0.0.1]:{} ssh-ed25519 ", server.port)),
        "{}",
        server.known_hosts()
    );

    // Arithmetic, so the echoed command line doesn't match on its own.
    write(&core, session, "echo burrow-$((40 + 2))\n").await;
    read_until(&mut output, "burrow-42").await;

    write(&core, session, "exit\n").await;
    assert_eq!(closed_error(expect_state(&mut events, session).await), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_known_host_connects_without_asking() {
    let Some(server) = Server::start() else {
        return;
    };
    let host_key = fs::read_to_string(server.path("host_key.pub")).unwrap();
    fs::write(
        server.path("known_hosts"),
        format!("[127.0.0.1]:{} {host_key}", server.port),
    )
    .unwrap();
    let core = server.core();
    let node = host(&core, "test").await;
    let mut events = core.subscribe();
    let (session, _output) = open(&core, &node).await;

    assert_eq!(
        expect_state(&mut events, session).await,
        SessionState::Connected
    );
    core.execute(Command::CloseSession { session })
        .await
        .unwrap();
    assert_eq!(closed_error(expect_state(&mut events, session).await), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn refusing_the_host_key_stops_and_records_nothing() {
    let Some(server) = Server::start() else {
        return;
    };
    let core = server.core();
    let node = host(&core, "test").await;
    let mut events = core.subscribe();
    let (session, _output) = open(&core, &node).await;

    let prompt = expect_prompt(&mut events, session).await;
    answer(&core, session, &prompt, PromptAnswer::Cancel).await;
    let error = closed_error(expect_state(&mut events, session).await).unwrap();
    assert!(error.contains("not trusted"), "{error}");
    assert_eq!(server.known_hosts(), "");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_changed_host_key_is_refused_without_asking() {
    let Some(server) = Server::start() else {
        return;
    };
    keygen(&server.path("other_key"));
    let other = fs::read_to_string(server.path("other_key.pub")).unwrap();
    let recorded = format!("[127.0.0.1]:{} {other}", server.port);
    fs::write(server.path("known_hosts"), &recorded).unwrap();
    let core = server.core();
    let node = host(&core, "test").await;
    let mut events = core.subscribe();
    let (session, _output) = open(&core, &node).await;

    let error = closed_error(expect_state(&mut events, session).await).unwrap();
    assert!(error.contains("HAS CHANGED"), "{error}");
    assert_eq!(server.known_hosts(), recorded);
}

#[tokio::test(flavor = "multi_thread")]
async fn dropping_the_output_ends_the_session() {
    let Some(server) = Server::start() else {
        return;
    };
    let host_key = fs::read_to_string(server.path("host_key.pub")).unwrap();
    fs::write(
        server.path("known_hosts"),
        format!("[127.0.0.1]:{} {host_key}", server.port),
    )
    .unwrap();
    let core = server.core();
    let node = host(&core, "test").await;
    let mut events = core.subscribe();
    let (session, output) = open(&core, &node).await;
    assert_eq!(
        expect_state(&mut events, session).await,
        SessionState::Connected
    );

    drop(output);
    // The shell's prompt or echo is the next output, and nobody reads it.
    let _ = core
        .execute(Command::WriteTerminal {
            session,
            data: b"echo bye\n".to_vec(),
        })
        .await;
    assert_eq!(closed_error(expect_state(&mut events, session).await), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_alias_that_looks_like_an_option_is_refused() {
    let Some(server) = Server::start() else {
        return;
    };
    let core = server.core();
    let node = host(&core, "-F/tmp/evil").await;
    let mut events = core.subscribe();
    let (session, _output) = open(&core, &node).await;

    let error = closed_error(expect_state(&mut events, session).await).unwrap();
    assert!(error.contains("must not start with '-'"), "{error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn only_ssh_hosts_open_terminals() {
    let Some(server) = Server::start() else {
        return;
    };
    let core = server.core();
    let folder = match core
        .execute(Command::CreateNode {
            parent: None,
            kind: "folder".into(),
            name: "f".into(),
            color: None,
            props: toml::Table::new(),
        })
        .await
        .unwrap()
    {
        Response::Node { node } => node,
        other => panic!("{other:?}"),
    };
    let result = core
        .execute(Command::OpenTerminal {
            node: folder.id,
            cols: 80,
            rows: 24,
        })
        .await;
    assert!(result.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_encrypted_key_asks_for_its_passphrase() {
    let Some(server) = Server::start() else {
        return;
    };
    let key = server.path("client_key");
    let status = Process::new("ssh-keygen")
        .args(["-q", "-p", "-P", "", "-N", "open sesame", "-f"])
        .arg(&key)
        .status()
        .unwrap();
    assert!(status.success());
    let host_key = fs::read_to_string(server.path("host_key.pub")).unwrap();
    fs::write(
        server.path("known_hosts"),
        format!("[127.0.0.1]:{} {host_key}", server.port),
    )
    .unwrap();
    let core = server.core();
    let node = host(&core, "test").await;
    let mut events = core.subscribe();
    let (session, _output) = open(&core, &node).await;

    let wrong = expect_prompt(&mut events, session).await;
    assert_eq!(wrong.kind, PromptKind::Secret { can_save: false });
    let text = |value: &str| PromptAnswer::Text {
        value: SecretValue::new(value),
        save: false,
    };
    answer(&core, session, &wrong, text("nope")).await;
    let retry = expect_prompt(&mut events, session).await;
    assert!(
        retry.message.starts_with("That didn't work"),
        "{}",
        retry.message
    );
    answer(&core, session, &retry, text("open sesame")).await;
    assert_eq!(
        expect_state(&mut events, session).await,
        SessionState::Connected
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unfamiliar_key_type_warns_even_under_accept_new() {
    let Some(server) = Server::start() else {
        return;
    };
    // The server only has an ed25519 key; known_hosts only an ECDSA one.
    let status = Process::new("ssh-keygen")
        .args(["-q", "-t", "ecdsa", "-N", "", "-C", "", "-f"])
        .arg(server.path("ecdsa_key"))
        .status()
        .unwrap();
    assert!(status.success());
    let ecdsa = fs::read_to_string(server.path("ecdsa_key.pub")).unwrap();
    fs::write(
        server.path("known_hosts"),
        format!("[127.0.0.1]:{} {ecdsa}", server.port),
    )
    .unwrap();
    let config = fs::read_to_string(server.path("ssh_config")).unwrap();
    fs::write(
        server.path("ssh_config"),
        config.replace(
            "StrictHostKeyChecking ask",
            "StrictHostKeyChecking accept-new",
        ),
    )
    .unwrap();
    let core = server.core();
    let node = host(&core, "test").await;
    let mut events = core.subscribe();
    let (session, _output) = open(&core, &node).await;

    let prompt = expect_prompt(&mut events, session).await;
    assert!(
        prompt.message.contains("intercepting"),
        "{}",
        prompt.message
    );
    answer(&core, session, &prompt, PromptAnswer::Cancel).await;
    assert!(closed_error(expect_state(&mut events, session).await).is_some());
}

/// A throwaway `ssh-agent` on its own socket.
struct Agent {
    socket: PathBuf,
    process: Child,
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

impl Agent {
    fn start(dir: &Path) -> Agent {
        let socket = dir.join("agent.sock");
        let process = Process::new("ssh-agent")
            .arg("-D")
            .arg("-a")
            .arg(&socket)
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        // Owned by `Agent` straight away, so it is killed and reaped even
        // if the wait below fails.
        let agent = Agent { socket, process };
        for _ in 0..100 {
            if agent.socket.exists() {
                return agent;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("ssh-agent did not start");
    }

    fn add(&self, key: &Path) {
        let status = Process::new("ssh-add")
            .arg("-q")
            .arg(key)
            .env("SSH_AUTH_SOCK", &self.socket)
            .status()
            .unwrap();
        assert!(status.success());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn signs_in_with_a_key_only_the_agent_holds() {
    let Some(server) = Server::start() else {
        return;
    };
    let agent = Agent::start(server.dir.path());
    // An unrelated key first, as agents such as 1Password hold many.
    keygen(&server.path("other_key"));
    agent.add(&server.path("other_key"));
    agent.add(&server.path("client_key"));
    // Only the agent can sign now. The config names the public key, the
    // way 1Password's setup picks an agent key.
    fs::remove_file(server.path("client_key")).unwrap();
    let config = fs::read_to_string(server.path("ssh_config")).unwrap();
    let config = config
        .replace(
            &format!("IdentityFile {}", server.path("client_key").display()),
            &format!("IdentityFile {}", server.path("client_key.pub").display()),
        )
        .replace(
            "IdentityAgent none",
            &format!("IdentityAgent {}", agent.socket.display()),
        );
    fs::write(server.path("ssh_config"), config).unwrap();
    let host_key = fs::read_to_string(server.path("host_key.pub")).unwrap();
    fs::write(
        server.path("known_hosts"),
        format!("[127.0.0.1]:{} {host_key}", server.port),
    )
    .unwrap();

    let core = server.core();
    let node = host(&core, "test").await;
    let mut events = core.subscribe();
    let (session, _output) = open(&core, &node).await;
    assert_eq!(
        expect_state(&mut events, session).await,
        SessionState::Connected
    );
}
