//! What the monitor tests share: a throwaway sshd whose host key is
//! trusted, and a core that puts this crate's binary on it.
#![allow(dead_code)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use burrow_core::{
    Command, Core, Event, GrantShown, MonitorHistory, Node, PromptAnswer, PromptKind, Response,
    SessionId, SessionState,
};
use tokio::sync::broadcast;

use crate::common::{Server, WAIT, answer, expect_prompt, expect_state, host, trust};

pub const AGENT: &str = env!("CARGO_BIN_EXE_burrow-agent");

/// A server with its host key trusted, and a core that takes agent builds
/// from `builds` and puts the agent in `remote`.
pub struct Rig {
    pub server: Server,
    pub core: Core,
    pub node: Node,
    pub events: broadcast::Receiver<Event>,
}

impl Rig {
    pub async fn start() -> Option<Rig> {
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

    pub fn remote(&self) -> PathBuf {
        PathBuf::from(remote_dir(&self.server))
            .join(format!("burrow-agent-{}", burrow_core::monitor::VERSION))
    }

    pub async fn open(&mut self) -> SessionId {
        let command = Command::OpenMonitor { node: self.node.id };
        match self.core.execute(command).await.unwrap() {
            Response::Session { session } => session,
            other => panic!("expected a session, got {other:?}"),
        }
    }

    pub async fn history(&self, session: SessionId) -> MonitorHistory {
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
    pub async fn open_and_install(&mut self) -> SessionId {
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

    pub async fn next_sample(&mut self, session: SessionId) -> burrow_core::monitor::Sample {
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

pub fn remote_dir(server: &Server) -> String {
    server.path("agent").to_str().unwrap().to_string()
}

/// Makes `binary` the agent build for this machine.
pub fn install_build(server: &Server, binary: &[u8]) {
    let path = server
        .path("builds")
        .join(format!("burrow-agent-{}", std::env::consts::ARCH));
    fs::write(&path, binary).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// Turns background monitoring on for the rig's host, as the user would
/// after seeing it, and returns the session the core started for it.
pub async fn monitor_in_background(rig: &mut Rig) -> SessionId {
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
