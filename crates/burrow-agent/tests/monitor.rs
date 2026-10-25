//! Monitors end to end: burrow-core puts this crate's binary on a
//! throwaway sshd over SFTP and runs it there. They skip, like the other
//! SSH tests, when sshd isn't installed.

#[path = "../../burrow-core/tests/common/mod.rs"]
mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use burrow_core::{
    Command, Core, Event, GrantShown, MonitorHistory, Node, NodePatch, PromptAnswer, PromptKind,
    Response, SessionId, SessionState, TrendRange,
};
use common::{Server, WAIT, answer, closed_error, expect_prompt, expect_state, host, trust};
use tokio::sync::broadcast;

const AGENT: &str = env!("CARGO_BIN_EXE_burrow-agent");

/// A server with its host key trusted, and a core that takes agent builds
/// from `builds` and puts the agent in `remote`.
struct Rig {
    server: Server,
    core: Core,
    node: Node,
    events: broadcast::Receiver<Event>,
}

impl Rig {
    async fn start() -> Option<Rig> {
        let server = Server::start()?;
        trust(&server);
        let core = server.core();
        fs::create_dir(server.path("builds")).unwrap();
        install_build(&server, &fs::read(AGENT).unwrap());
        core.set_agent_dirs(server.path("builds"), remote_dir(&server));
        let node = host(&core, "test").await;
        let events = core.subscribe();
        Some(Rig {
            server,
            core,
            node,
            events,
        })
    }

    fn remote(&self) -> PathBuf {
        PathBuf::from(remote_dir(&self.server))
            .join(format!("burrow-agent-{}", burrow_core::monitor::VERSION))
    }

    async fn open(&mut self) -> SessionId {
        let command = Command::OpenMonitor { node: self.node.id };
        match self.core.execute(command).await.unwrap() {
            Response::Session { session } => session,
            other => panic!("expected a session, got {other:?}"),
        }
    }

    async fn history(&self, session: SessionId) -> MonitorHistory {
        match self
            .core
            .execute(Command::MonitorHistory { session })
            .await
            .unwrap()
        {
            Response::Monitor { history } => history,
            other => panic!("expected a history, got {other:?}"),
        }
    }

    /// Opens a monitor and agrees to the install.
    async fn open_and_install(&mut self) -> SessionId {
        let session = self.open().await;
        let prompt = expect_prompt(&mut self.events, session).await;
        assert_eq!(prompt.kind, PromptKind::Confirm);
        assert_eq!(prompt.title, "Install burrow-agent?");
        assert_eq!(prompt.action.as_deref(), Some("Install"));
        answer(&self.core, session, &prompt, PromptAnswer::Confirm).await;
        assert_eq!(
            expect_state(&mut self.events, session).await,
            SessionState::Connected
        );
        session
    }

    async fn next_sample(&mut self, session: SessionId) -> burrow_core::monitor::Sample {
        tokio::time::timeout(WAIT, async {
            loop {
                match self.events.recv().await.unwrap() {
                    Event::MonitorSample {
                        session: s, sample, ..
                    } if s == session => return sample,
                    Event::SessionChanged {
                        session: s,
                        state: state @ SessionState::Closed { .. },
                        ..
                    } if s == session => panic!("the monitor closed: {state:?}"),
                    _ => {}
                }
            }
        })
        .await
        .expect("timed out waiting for a sample")
    }
}

fn remote_dir(server: &Server) -> String {
    server.path("agent").to_str().unwrap().to_string()
}

/// Makes `binary` the agent build for this machine.
fn install_build(server: &Server, binary: &[u8]) {
    let path = server
        .path("builds")
        .join(format!("burrow-agent-{}", std::env::consts::ARCH));
    fs::write(&path, binary).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn installs_after_asking_then_reports() {
    let Some(mut rig) = Rig::start().await else {
        return;
    };
    let session = rig.open_and_install().await;
    let mode = fs::metadata(rig.remote()).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o700);
    assert_eq!(fs::read(rig.remote()).unwrap(), fs::read(AGENT).unwrap());

    let sample = rig.next_sample(session).await;
    assert!(sample.memory.total > 0);
    assert!(sample.cpu.cores > 0);
    assert!(!rig.history(session).await.samples.is_empty());

    rig.core
        .execute(Command::CloseSession { session })
        .await
        .unwrap();
    assert_eq!(
        closed_error(expect_state(&mut rig.events, session).await),
        None
    );
    // Closing the monitor stops the agent on the server.
    let running = || {
        std::process::Command::new("pgrep")
            .arg("-f")
            .arg(rig.remote())
            .status()
            .unwrap()
            .success()
    };
    let deadline = std::time::Instant::now() + WAIT;
    while running() {
        assert!(
            std::time::Instant::now() < deadline,
            "the agent kept running"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn asks_once_and_replaces_a_changed_agent() {
    let Some(mut rig) = Rig::start().await else {
        return;
    };
    let first = rig.open_and_install().await;
    rig.core
        .execute(Command::CloseSession { session: first })
        .await
        .unwrap();
    expect_state(&mut rig.events, first).await;

    // A file that isn't the agent any more, and an older version. The
    // first agent may still be exiting, so the file is replaced rather
    // than written to.
    fs::remove_file(rig.remote()).unwrap();
    fs::write(rig.remote(), b"#!/bin/sh\necho changed\n").unwrap();
    let old = PathBuf::from(remote_dir(&rig.server)).join("burrow-agent-0.0.1");
    fs::write(&old, b"old").unwrap();

    let second = rig.open().await;
    assert_eq!(
        expect_state(&mut rig.events, second).await,
        SessionState::Connected
    );
    assert_eq!(fs::read(rig.remote()).unwrap(), fs::read(AGENT).unwrap());
    assert!(!old.exists(), "the old version was left behind");
    rig.next_sample(second).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn refusing_installs_nothing() {
    let Some(mut rig) = Rig::start().await else {
        return;
    };
    let session = rig.open().await;
    let prompt = expect_prompt(&mut rig.events, session).await;
    answer(&rig.core, session, &prompt, PromptAnswer::Cancel).await;
    let error = closed_error(expect_state(&mut rig.events, session).await).unwrap();
    assert!(error.contains("not installed"), "{error}");
    assert!(!rig.remote().exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_symlinked_agent_is_never_run_or_written_through() {
    let Some(mut rig) = Rig::start().await else {
        return;
    };
    fs::create_dir(remote_dir(&rig.server)).unwrap();
    let target = rig.server.path("elsewhere");
    fs::write(&target, b"keep").unwrap();
    std::os::unix::fs::symlink(&target, rig.remote()).unwrap();

    let session = rig.open().await;
    let error = closed_error(expect_state(&mut rig.events, session).await).unwrap();
    assert!(error.contains("symlink"), "{error}");
    assert_eq!(fs::read(&target).unwrap(), b"keep");
}

/// Runs a fake agent and returns why the monitor closed.
async fn fake_agent(script: &str) -> Option<String> {
    let mut rig = Rig::start().await?;
    install_build(&rig.server, format!("#!/bin/sh\n{script}\n").as_bytes());
    let session = rig.open_and_install_or_fail().await;
    Some(closed_error(expect_state(&mut rig.events, session).await).unwrap())
}

impl Rig {
    /// Like [`Rig::open_and_install`], for agents that may stop before
    /// they say hello.
    async fn open_and_install_or_fail(&mut self) -> SessionId {
        let session = self.open().await;
        let prompt = expect_prompt(&mut self.events, session).await;
        answer(&self.core, session, &prompt, PromptAnswer::Confirm).await;
        session
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_agent_that_talks_nonsense_is_stopped() {
    let Some(error) = fake_agent("echo nonsense; sleep 5").await else {
        return;
    };
    assert!(error.contains("can't read"), "{error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_agent_must_say_hello_first() {
    let script = r#"echo '{"type":"units","units":[]}'; sleep 5"#;
    let Some(error) = fake_agent(script).await else {
        return;
    };
    assert!(error.contains("unexpected"), "{error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_agent_of_another_protocol_is_refused() {
    let script = r#"echo '{"type":"hello","version":"9","proto":99,"systemd":false}'; sleep 5"#;
    let Some(error) = fake_agent(script).await else {
        return;
    };
    assert!(error.contains("version 99"), "{error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_agent_that_stops_says_why() {
    let Some(error) = fake_agent("echo 'out of cheese' >&2; exit 1").await else {
        return;
    };
    assert!(error.contains("out of cheese"), "{error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_mcp_bridge_monitors_only_with_a_grant_and_never_installs() {
    use burrow_core::Capability;
    use burrow_core::ipc::{Client, Role};

    let Some(mut rig) = Rig::start().await else {
        return;
    };
    let socket = common::serve(rig.core.clone(), rig.server.dir.path());
    let gui = Client::connect(&socket, Role::Gui).await.unwrap();
    let mcp = Client::connect(&socket, Role::Mcp).await.unwrap();
    let node = rig.node.id;
    let open = || Command::OpenMonitor { node };

    let trend = || Command::MonitorTrend {
        node,
        range: TrendRange::Day,
    };

    let error = mcp.execute(open()).await.unwrap_err();
    assert!(error.to_string().contains("grant"), "{error}");
    assert!(mcp.execute(trend()).await.is_err());
    // Running commands is a different grant.
    common::grant(&gui, &rig.node, &[Capability::Exec]).await;
    assert!(mcp.execute(open()).await.is_err());

    // Granted, but nothing is installed yet: the bridge gets an error, and
    // nobody is asked to install.
    common::grant(&gui, &rig.node, &[Capability::MonitorRead]).await;
    assert!(matches!(
        mcp.execute(trend()).await.unwrap(),
        Response::Trend { points } if points.is_empty()
    ));
    let session = match mcp.execute(open()).await.unwrap() {
        Response::Session { session } => session,
        other => panic!("expected a session, got {other:?}"),
    };
    let error = closed_error(expect_state(&mut rig.events, session).await).unwrap();
    assert!(error.contains("isn't on"), "{error}");
    assert!(!rig.remote().exists());

    // Once the user has installed it, the bridge can run it.
    let installed = rig.open_and_install().await;
    rig.core
        .execute(Command::CloseSession { session: installed })
        .await
        .unwrap();
    let session = common::open_and_wait(&mcp, open()).await;
    let history = tokio::time::timeout(WAIT, async {
        loop {
            match mcp
                .execute(Command::MonitorHistory { session })
                .await
                .unwrap()
            {
                Response::Monitor { history } if !history.samples.is_empty() => return history,
                _ => tokio::time::sleep(std::time::Duration::from_millis(200)).await,
            }
        }
    })
    .await
    .expect("no sample came");
    assert!(history.samples[0].memory.total > 0);

    // It can't read a monitor it didn't open.
    let theirs = rig.open().await;
    expect_state(&mut rig.events, theirs).await;
    let error = mcp
        .execute(Command::MonitorHistory { session: theirs })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("did not open"), "{error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn changing_the_host_asks_again() {
    let Some(mut rig) = Rig::start().await else {
        return;
    };
    let first = rig.open_and_install().await;
    rig.core
        .execute(Command::CloseSession { session: first })
        .await
        .unwrap();
    expect_state(&mut rig.events, first).await;
    // Make an upload necessary, so consent is what decides.
    fs::remove_file(rig.remote()).unwrap();

    // The same node, now a different host as far as consent goes. Its
    // alias still reaches the test server through the name.
    let mut props = rig.node.props.clone();
    props.remove("alias");
    let patch = burrow_core::NodePatch {
        name: Some("test".into()),
        props: Some(props),
        ..Default::default()
    };
    rig.core
        .execute(Command::UpdateNode {
            id: rig.node.id,
            patch,
        })
        .await
        .unwrap();

    let second = rig.open().await;
    let prompt = expect_prompt(&mut rig.events, second).await;
    assert_eq!(prompt.title, "Install burrow-agent?");
}

/// Turns background monitoring on for the rig's host, as the user would
/// after seeing it, and returns the session the core started for it.
async fn monitor_in_background(rig: &mut Rig) -> SessionId {
    let command = Command::SetMonitoring {
        node: rig.node.id,
        enabled: true,
        expect: Some(GrantShown::of(&rig.node, std::slice::from_ref(&rig.node))),
    };
    rig.core.execute(command).await.unwrap();
    match rig.core.execute(Command::ListSessions).await.unwrap() {
        Response::Sessions { sessions } => {
            sessions
                .into_iter()
                .find(|s| s.node == rig.node.id && s.background)
                .expect("no background monitor started")
                .session
        }
        other => panic!("expected sessions, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_background_monitor_is_shared_and_only_stops_when_turned_off() {
    let Some(mut rig) = Rig::start().await else {
        return;
    };
    let unseen = Command::SetMonitoring {
        node: rig.node.id,
        enabled: true,
        expect: None,
    };
    assert!(rig.core.execute(unseen).await.is_err());

    let session = monitor_in_background(&mut rig).await;
    assert!(
        fs::read_to_string(rig.server.path("state/monitoring.toml"))
            .unwrap()
            .contains(&rig.node.id.to_string())
    );
    // The agent's own consent still decides the first upload.
    let prompt = expect_prompt(&mut rig.events, session).await;
    assert_eq!(prompt.title, "Install burrow-agent?");
    answer(&rig.core, session, &prompt, PromptAnswer::Confirm).await;
    assert_eq!(
        expect_state(&mut rig.events, session).await,
        SessionState::Connected
    );

    assert_eq!(rig.open().await, session);
    let close = Command::CloseSession { session };
    assert!(rig.core.execute(close).await.is_err());
    rig.next_sample(session).await;

    // An agent that stops is reconnected, after a wait.
    std::process::Command::new("pkill")
        .arg("-f")
        .arg(rig.remote())
        .status()
        .unwrap();
    let step = tokio::time::timeout(WAIT, async {
        loop {
            if let Event::SessionChanged {
                session: s,
                state: SessionState::Connecting { step },
                ..
            } = rig.events.recv().await.unwrap()
                && s == session
            {
                return step;
            }
        }
    })
    .await
    .expect("the monitor did not try again");
    assert!(step.starts_with("Reconnecting in 30 s: "), "{step}");

    rig.core
        .execute(Command::SetMonitoring {
            node: rig.node.id,
            enabled: false,
            expect: None,
        })
        .await
        .unwrap();
    assert_eq!(
        closed_error(expect_state(&mut rig.events, session).await),
        None
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn changing_the_host_stops_its_background_monitor() {
    let Some(mut rig) = Rig::start().await else {
        return;
    };
    let session = monitor_in_background(&mut rig).await;
    let prompt = expect_prompt(&mut rig.events, session).await;
    answer(&rig.core, session, &prompt, PromptAnswer::Confirm).await;
    expect_state(&mut rig.events, session).await;

    let mut props = rig.node.props.clone();
    props.insert("user".into(), "someone-else".into());
    let patch = NodePatch {
        props: Some(props),
        ..Default::default()
    };
    let command = Command::UpdateNode {
        id: rig.node.id,
        patch,
    };
    rig.core.execute(command).await.unwrap();
    assert_eq!(
        closed_error(expect_state(&mut rig.events, session).await),
        None
    );
    match rig.core.execute(Command::ListMonitoring).await.unwrap() {
        Response::Monitoring { nodes } => assert!(nodes.is_empty()),
        other => panic!("expected monitoring, got {other:?}"),
    }
}
