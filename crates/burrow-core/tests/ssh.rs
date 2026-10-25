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
    Command, Core, CoreOptions, Event, MemoryStore, Node, NodePatch, Prompt, PromptAnswer,
    PromptKind, Response, SecretValue, SessionId, SessionKind, SessionState, TransferId,
    TransferState, TunnelShown,
};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
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

#[tokio::test(flavor = "multi_thread")]
async fn sessions_on_one_host_share_its_connection() {
    let Some(server) = Server::start() else {
        return;
    };
    // A passphrase is asked for on every sign-in, so a second prompt would
    // mean a second connection.
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

    let (first, _first_output) = open(&core, &node).await;
    let prompt = expect_prompt(&mut events, first).await;
    let passphrase = PromptAnswer::Text {
        value: SecretValue::new("open sesame"),
        save: false,
    };
    answer(&core, first, &prompt, passphrase).await;
    assert_eq!(
        expect_state(&mut events, first).await,
        SessionState::Connected
    );

    let (second, mut output) = open(&core, &node).await;
    assert_eq!(
        expect_state(&mut events, second).await,
        SessionState::Connected
    );

    // Closing one session leaves the connection to the other.
    core.execute(Command::CloseSession { session: first })
        .await
        .unwrap();
    assert_eq!(closed_error(expect_state(&mut events, first).await), None);
    write(&core, second, "echo still-$((20 + 22))\n").await;
    read_until(&mut output, "still-42").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_waiting_on_another_takes_over_when_it_closes() {
    let Some(server) = Server::start() else {
        return;
    };
    let core = server.core();
    let node = host(&core, "test").await;
    let mut events = core.subscribe();

    let (first, _first_output) = open(&core, &node).await;
    expect_prompt(&mut events, first).await;
    // Waits for the first session's connection instead of asking too.
    let (second, _second_output) = open(&core, &node).await;
    core.execute(Command::CloseSession { session: first })
        .await
        .unwrap();
    assert_eq!(closed_error(expect_state(&mut events, first).await), None);

    let prompt = expect_prompt(&mut events, second).await;
    answer(&core, second, &prompt, PromptAnswer::Confirm).await;
    assert_eq!(
        expect_state(&mut events, second).await,
        SessionState::Connected
    );
}

/// Writes the server's host key to known_hosts, so connecting asks nothing.
fn trust(server: &Server) {
    let host_key = fs::read_to_string(server.path("host_key.pub")).unwrap();
    fs::write(
        server.path("known_hosts"),
        format!("[127.0.0.1]:{} {host_key}", server.port),
    )
    .unwrap();
}

/// A TCP server on 127.0.0.1 that sends back whatever it gets.
async fn echo_server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let (mut reader, mut writer) = socket.split();
                let _ = tokio::io::copy(&mut reader, &mut writer).await;
            });
        }
    });
    port
}

async fn tunnel(core: &Core, via: &Node, props: &str) -> Node {
    let mut props: toml::Table = toml::from_str(props).unwrap();
    props.insert("via".into(), via.id.to_string().into());
    let command = Command::CreateNode {
        parent: None,
        kind: "ssh-tunnel".into(),
        name: "tunnel".into(),
        color: None,
        props,
    };
    match core.execute(command).await.unwrap() {
        Response::Node { node } => node,
        other => panic!("expected a node, got {other:?}"),
    }
}

async fn start_tunnel(core: &Core, node: &Node) -> SessionId {
    match core
        .execute(Command::StartTunnel { node: node.id })
        .await
        .unwrap()
    {
        Response::Session { session } => session,
        other => panic!("expected a session, got {other:?}"),
    }
}

/// Sends `text` and expects it back.
async fn echoes(socket: &mut TcpStream, text: &str) {
    socket.write_all(text.as_bytes()).await.unwrap();
    let mut back = vec![0; text.len()];
    tokio::time::timeout(WAIT, socket.read_exact(&mut back))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(back, text.as_bytes());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_local_tunnel_forwards_to_its_target() {
    let Some(server) = Server::start() else {
        return;
    };
    trust(&server);
    let core = server.core();
    let host = host(&core, "test").await;
    let (target, port) = (echo_server().await, free_port());
    let node = tunnel(
        &core,
        &host,
        &format!(
            "type = \"local\"\nport = {port}\ntarget_host = \"127.0.0.1\"\ntarget_port = {target}"
        ),
    )
    .await;
    let mut events = core.subscribe();
    let session = start_tunnel(&core, &node).await;
    assert_eq!(
        expect_state(&mut events, session).await,
        SessionState::Connected
    );
    // Starting it again gives the running one.
    assert_eq!(start_tunnel(&core, &node).await, session);

    let mut socket = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    echoes(&mut socket, "through the tunnel").await;

    let sessions = match core.execute(Command::ListSessions).await.unwrap() {
        Response::Sessions { sessions } => sessions,
        other => panic!("{other:?}"),
    };
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].kind, SessionKind::Tunnel);
    assert_eq!(sessions[0].state, SessionState::Connected);

    core.execute(Command::CloseSession { session })
        .await
        .unwrap();
    assert_eq!(closed_error(expect_state(&mut events, session).await), None);
    // The forwarded connection ended with it, and nothing listens.
    let mut rest = Vec::new();
    let _ = tokio::time::timeout(WAIT, socket.read_to_end(&mut rest))
        .await
        .unwrap();
    assert!(TcpStream::connect(("127.0.0.1", port)).await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dynamic_tunnel_is_a_socks_proxy() {
    let Some(server) = Server::start() else {
        return;
    };
    trust(&server);
    let core = server.core();
    let host = host(&core, "test").await;
    let (target, port) = (echo_server().await, free_port());
    let node = tunnel(&core, &host, &format!("type = \"dynamic\"\nport = {port}")).await;
    let mut events = core.subscribe();
    let session = start_tunnel(&core, &node).await;
    assert_eq!(
        expect_state(&mut events, session).await,
        SessionState::Connected
    );

    let mut socket = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    socket.write_all(&[5, 1, 0]).await.unwrap();
    let mut greeting = [0; 2];
    socket.read_exact(&mut greeting).await.unwrap();
    assert_eq!(greeting, [5, 0]);
    // CONNECT to "localhost" by name, which the SSH host resolves.
    let mut request = vec![5, 1, 0, 3, 9];
    request.extend(b"localhost");
    request.extend(target.to_be_bytes());
    socket.write_all(&request).await.unwrap();
    let mut reply = [0; 10];
    socket.read_exact(&mut reply).await.unwrap();
    assert_eq!(reply[..2], [5, 0]);
    echoes(&mut socket, "through the proxy").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tunnel_only_listens_on_loopback() {
    let Some(server) = Server::start() else {
        return;
    };
    let core = server.core();
    let host = host(&core, "test").await;
    let node = tunnel(
        &core,
        &host,
        &format!(
            "type = \"dynamic\"\nport = {}\nbind = \"0.0.0.0\"",
            free_port()
        ),
    )
    .await;
    let mut events = core.subscribe();
    let session = start_tunnel(&core, &node).await;
    let error = closed_error(expect_state(&mut events, session).await).unwrap();
    assert!(error.contains("loopback"), "{error}");
}

async fn autostart(core: &Core) -> Vec<burrow_core::NodeId> {
    match core.execute(Command::ListAutostart).await.unwrap() {
        Response::Autostart { nodes } => nodes,
        other => panic!("{other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn autostart_is_kept_locally_and_pinned_to_the_settings() {
    let Some(server) = Server::start() else {
        return;
    };
    trust(&server);
    let core = server.core();
    let host = host(&core, "test").await;
    let port = free_port();
    let props = format!("type = \"dynamic\"\nport = {port}");
    let node = tunnel(&core, &host, &props).await;
    let enable = |expect| Command::SetAutostart {
        node: node.id,
        enabled: true,
        expect,
    };
    let shown = TunnelShown {
        props: node.props.clone(),
        host_name: host.name.clone(),
        host_props: host.props.clone(),
    };
    // Enabling needs what the user was shown, and refuses anything else:
    // the tunnel's settings, or the host's alias, changed since.
    assert!(core.execute(enable(None)).await.is_err());
    let mut stale = shown.clone();
    stale.props.insert("port".into(), 1.into());
    assert!(core.execute(enable(Some(stale))).await.is_err());
    let mut stale = shown.clone();
    stale.host_props.insert("alias".into(), "elsewhere".into());
    assert!(core.execute(enable(Some(stale))).await.is_err());
    assert!(autostart(&core).await.is_empty());
    core.execute(enable(Some(shown))).await.unwrap();
    assert_eq!(autostart(&core).await, vec![node.id]);
    let tunnel_file = fs::read_to_string(server.path("burrow/tree/tunnel.toml")).unwrap();
    assert!(!tunnel_file.contains("autostart"), "{tunnel_file}");

    // Pointing the host at another server withdraws it.
    let mut events = core.subscribe();
    let mut moved = host.props.clone();
    moved.insert("alias".into(), "elsewhere".into());
    let patch = NodePatch {
        props: Some(moved),
        ..Default::default()
    };
    core.execute(Command::UpdateNode { id: host.id, patch })
        .await
        .unwrap();
    assert!(autostart(&core).await.is_empty());
    let changed = tokio::time::timeout(WAIT, async {
        loop {
            if let Event::AutostartChanged { nodes } = events.recv().await.unwrap() {
                return nodes;
            }
        }
    })
    .await
    .unwrap();
    assert!(changed.is_empty());
    let patch = NodePatch {
        props: Some(host.props.clone()),
        ..Default::default()
    };
    core.execute(Command::UpdateNode { id: host.id, patch })
        .await
        .unwrap();
    assert_eq!(autostart(&core).await, vec![node.id]);
    drop(core);

    // A new core starts it.
    let core = server.core();
    let mut events = core.subscribe();
    core.autostart();
    let session = loop {
        match tokio::time::timeout(WAIT, events.recv())
            .await
            .unwrap()
            .unwrap()
        {
            Event::SessionChanged {
                session,
                state: SessionState::Connected,
                ..
            } => break session,
            Event::SessionChanged {
                state: SessionState::Closed { error },
                ..
            } => panic!("autostart failed: {error:?}"),
            _ => {}
        }
    };
    let sessions = match core.execute(Command::ListSessions).await.unwrap() {
        Response::Sessions { sessions } => sessions,
        other => panic!("{other:?}"),
    };
    assert_eq!(sessions[0].session, session);
    assert_eq!(sessions[0].node, node.id);
}

async fn open_files(
    core: &Core,
    node: &Node,
    events: &mut broadcast::Receiver<Event>,
) -> SessionId {
    let session = match core
        .execute(Command::OpenFiles { node: node.id })
        .await
        .unwrap()
    {
        Response::Session { session } => session,
        other => panic!("expected a session, got {other:?}"),
    };
    assert_eq!(expect_state(events, session).await, SessionState::Connected);
    session
}

async fn transferred(
    core: &Core,
    events: &mut broadcast::Receiver<Event>,
    command: Command,
) -> TransferState {
    let transfer: TransferId = match core.execute(command).await.unwrap() {
        Response::Transfer { transfer } => transfer,
        other => panic!("expected a transfer, got {other:?}"),
    };
    tokio::time::timeout(WAIT, async {
        loop {
            match events.recv().await.unwrap() {
                Event::TransferChanged {
                    transfer: t, state, ..
                } if t == transfer && !matches!(state, TransferState::Running { .. }) => {
                    return state;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("timed out waiting for the transfer")
}

fn remote(path: &Path) -> String {
    path.to_str().unwrap().to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_file_panel_lists_changes_and_copies_files() {
    let Some(server) = Server::start() else {
        return;
    };
    trust(&server);
    let core = server.core();
    let node = host(&core, "test").await;
    let mut events = core.subscribe();
    let session = open_files(&core, &node, &mut events).await;

    // The server is this machine, so remote paths are local ones.
    let root = server.path("remote");
    fs::create_dir(&root).unwrap();
    let ok = |result: burrow_core::Result<Response>| assert_eq!(result.unwrap(), Response::Ok);
    ok(core
        .execute(Command::MakeDir {
            session,
            path: remote(&root.join("docs")),
        })
        .await);
    let local = server.path("hello.txt");
    fs::write(&local, "hello over sftp").unwrap();
    let upload = Command::Upload {
        session,
        local: local.clone(),
        remote: remote(&root.join("docs/hello.txt")),
        overwrite: false,
    };
    assert_eq!(
        transferred(&core, &mut events, upload.clone()).await,
        TransferState::Finished
    );
    // A second upload to the same name is refused unless asked.
    match transferred(&core, &mut events, upload).await {
        TransferState::Failed { error } => assert!(error.contains("already exists"), "{error}"),
        other => panic!("{other:?}"),
    }

    ok(core
        .execute(Command::RenamePath {
            session,
            from: remote(&root.join("docs/hello.txt")),
            to: remote(&root.join("docs/greeting.txt")),
        })
        .await);
    let (path, entries) = match core
        .execute(Command::ListDir {
            session,
            path: remote(&root.join("docs")),
        })
        .await
        .unwrap()
    {
        Response::Dir { path, entries } => (path, entries),
        other => panic!("{other:?}"),
    };
    assert_eq!(path, remote(&fs::canonicalize(root.join("docs")).unwrap()));
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "greeting.txt");
    assert_eq!(entries[0].size, 15);

    let copy = server.path("copy.txt");
    let download = Command::Download {
        session,
        remote: remote(&root.join("docs/greeting.txt")),
        local: copy.clone(),
        overwrite: false,
    };
    assert_eq!(
        transferred(&core, &mut events, download).await,
        TransferState::Finished
    );
    assert_eq!(fs::read_to_string(&copy).unwrap(), "hello over sftp");

    ok(core
        .execute(Command::RemovePath {
            session,
            path: remote(&root.join("docs")),
        })
        .await);
    assert!(!root.join("docs").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn folders_copy_whole_but_symlinks_inside_are_skipped() {
    let Some(server) = Server::start() else {
        return;
    };
    trust(&server);
    let core = server.core();
    let node = host(&core, "test").await;
    let mut events = core.subscribe();
    let session = open_files(&core, &node, &mut events).await;

    let tree = server.path("tree");
    fs::create_dir_all(tree.join("a/b")).unwrap();
    fs::write(tree.join("a/b/deep.txt"), "deep").unwrap();
    fs::write(tree.join("top.txt"), "top").unwrap();
    fs::write(server.path("secret.txt"), "outside").unwrap();
    std::os::unix::fs::symlink(server.path("secret.txt"), tree.join("link.txt")).unwrap();

    let uploaded = server.path("uploaded");
    let upload = Command::Upload {
        session,
        local: tree.clone(),
        remote: remote(&uploaded),
        overwrite: false,
    };
    assert_eq!(
        transferred(&core, &mut events, upload).await,
        TransferState::Finished
    );
    assert_eq!(
        fs::read_to_string(uploaded.join("a/b/deep.txt")).unwrap(),
        "deep"
    );
    assert_eq!(fs::read_to_string(uploaded.join("top.txt")).unwrap(), "top");
    assert!(!uploaded.join("link.txt").exists());

    std::os::unix::fs::symlink(server.path("secret.txt"), uploaded.join("link.txt")).unwrap();
    let downloaded = server.path("downloaded");
    let download = Command::Download {
        session,
        remote: remote(&uploaded),
        local: downloaded.clone(),
        overwrite: false,
    };
    assert_eq!(
        transferred(&core, &mut events, download).await,
        TransferState::Finished
    );
    assert_eq!(
        fs::read_to_string(downloaded.join("a/b/deep.txt")).unwrap(),
        "deep"
    );
    assert!(fs::symlink_metadata(downloaded.join("link.txt")).is_err());
    // No leftover partial files.
    let names: Vec<_> = fs::read_dir(&downloaded)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert!(names.iter().all(|n| !n.starts_with('.')), "{names:?}");

    // Merging into a folder never writes through a symlink in it.
    fs::remove_file(uploaded.join("top.txt")).unwrap();
    std::os::unix::fs::symlink(server.path("secret.txt"), uploaded.join("top.txt")).unwrap();
    let merge = Command::Upload {
        session,
        local: tree,
        remote: remote(&uploaded),
        overwrite: true,
    };
    match transferred(&core, &mut events, merge).await {
        TransferState::Failed { error } => assert!(error.contains("symlink"), "{error}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        fs::read_to_string(server.path("secret.txt")).unwrap(),
        "outside"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn file_commands_need_a_connected_file_session() {
    let Some(server) = Server::start() else {
        return;
    };
    trust(&server);
    let core = server.core();
    let node = host(&core, "test").await;
    let mut events = core.subscribe();
    let (terminal, _output) = open(&core, &node).await;
    assert_eq!(
        expect_state(&mut events, terminal).await,
        SessionState::Connected
    );
    let list = Command::ListDir {
        session: terminal,
        path: String::new(),
    };
    let error = core.execute(list).await.unwrap_err().to_string();
    assert!(error.contains("not a file session"), "{error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_upload_leaves_no_partial_file() {
    let Some(server) = Server::start() else {
        return;
    };
    trust(&server);
    let core = server.core();
    let node = host(&core, "test").await;
    let mut events = core.subscribe();
    let session = open_files(&core, &node, &mut events).await;

    let big = server.path("big.bin");
    fs::File::create(&big)
        .unwrap()
        .set_len(8 * 1024 * 1024 * 1024)
        .unwrap();
    let target = server.path("big-copy.bin");
    let upload = Command::Upload {
        session,
        local: big,
        remote: remote(&target),
        overwrite: false,
    };
    let transfer = match core.execute(upload).await.unwrap() {
        Response::Transfer { transfer } => transfer,
        other => panic!("{other:?}"),
    };
    // Cancel once some bytes have moved.
    tokio::time::timeout(WAIT, async {
        loop {
            if let Event::TransferChanged {
                state: TransferState::Running { done, .. },
                ..
            } = events.recv().await.unwrap()
                && done > 0
            {
                return;
            }
        }
    })
    .await
    .unwrap();
    core.execute(Command::CancelTransfer { transfer })
        .await
        .unwrap();
    let state = tokio::time::timeout(WAIT, async {
        loop {
            if let Event::TransferChanged {
                transfer: t, state, ..
            } = events.recv().await.unwrap()
                && t == transfer
                && !matches!(state, TransferState::Running { .. })
            {
                return state;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(state, TransferState::Cancelled);
    let left: Vec<_> = fs::read_dir(server.dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.contains("big-copy"))
        .collect();
    assert!(left.is_empty(), "left behind: {left:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn copies_keep_private_files_private() {
    use std::os::unix::fs::PermissionsExt;
    let Some(server) = Server::start() else {
        return;
    };
    trust(&server);
    let core = server.core();
    let node = host(&core, "test").await;
    let mut events = core.subscribe();
    let session = open_files(&core, &node, &mut events).await;
    let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
    let set_mode = |path: &Path, mode| {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    };

    // Replacing a private file keeps it private.
    let secret = server.path("secret.txt");
    fs::write(&secret, "old").unwrap();
    set_mode(&secret, 0o600);
    let source = server.path("new.txt");
    fs::write(&source, "new").unwrap();
    set_mode(&source, 0o644);
    let upload = Command::Upload {
        session,
        local: source.clone(),
        remote: remote(&secret),
        overwrite: true,
    };
    assert_eq!(
        transferred(&core, &mut events, upload).await,
        TransferState::Finished
    );
    assert_eq!(fs::read_to_string(&secret).unwrap(), "new");
    assert_eq!(mode(&secret), 0o600);

    // A new copy takes the source's mode, never writable by group or other.
    set_mode(&source, 0o666);
    let fresh = server.path("fresh.txt");
    let upload = Command::Upload {
        session,
        local: source,
        remote: remote(&fresh),
        overwrite: false,
    };
    assert_eq!(
        transferred(&core, &mut events, upload).await,
        TransferState::Finished
    );
    assert_eq!(mode(&fresh), 0o644);

    // A download of a private file is private too.
    let copy = server.path("secret-copy.txt");
    let download = Command::Download {
        session,
        remote: remote(&secret),
        local: copy.clone(),
        overwrite: false,
    };
    assert_eq!(
        transferred(&core, &mut events, download).await,
        TransferState::Finished
    );
    assert_eq!(mode(&copy), 0o600);
}
