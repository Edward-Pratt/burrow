//! Joining a background monitor that was already running: a client that
//! arrives after the monitor spoke, such as a window opened once the daemon
//! had started, has to be told where it stands. They skip, like the other
//! SSH tests, when sshd isn't installed.

#[path = "../../burrow-core/tests/common/mod.rs"]
mod common;
mod rig;

use std::fs;

use burrow_core::ipc::{Client, Events, Role};
use burrow_core::{Command, Event, PromptAnswer, Response, SessionId, SessionState};
use common::{WAIT, answer, expect_prompt, expect_state};
use rig::{Rig, monitor_in_background};

/// A client that connects only now, as a window opened later does.
async fn late_client(rig: &Rig) -> (Client, Events) {
    let socket = common::serve(rig.core.clone(), rig.server.dir.path());
    let client = Client::connect(&socket, Role::Gui).await.unwrap();
    let events = client.subscribe();
    (client, events)
}

async fn open(client: &Client, rig: &Rig) -> SessionId {
    let command = Command::OpenMonitor { node: rig.node.id };
    match client.execute(command).await.unwrap() {
        Response::Session { session } => session,
        other => panic!("expected a session, got {other:?}"),
    }
}

/// The next thing `session` says to `events`.
async fn said(events: &mut Events, session: SessionId) -> Event {
    tokio::time::timeout(WAIT, async {
        loop {
            match events.recv().await.unwrap() {
                event @ (Event::SessionChanged { session: s, .. }
                | Event::PromptRequested { session: s, .. })
                    if s == session =>
                {
                    return event;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("the session said nothing to a client that joined it")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_that_joins_a_waiting_monitor_is_asked_too() {
    let Some(mut rig) = Rig::start().await else {
        return;
    };
    // Asked with no client there to hear it, as by a daemon whose window
    // opens later.
    let session = monitor_in_background(&mut rig).await;
    let asked = expect_prompt(&mut rig.events, session).await;
    assert_eq!(asked.title, "Install burrow-agent?");

    let (window, mut events) = late_client(&rig).await;
    assert_eq!(open(&window, &rig).await, session);
    let state = SessionState::Connecting {
        step: "Checking burrow-agent".into(),
    };
    assert!(matches!(
        said(&mut events, session).await,
        Event::SessionChanged { state: s, .. } if s == state
    ));
    assert!(matches!(
        said(&mut events, session).await,
        Event::PromptRequested { prompt, .. } if prompt == asked
    ));

    // Its answer is the one the monitor was waiting for.
    let command = Command::AnswerPrompt {
        session,
        prompt: asked.id,
        answer: PromptAnswer::Confirm,
    };
    window.execute(command).await.unwrap();
    loop {
        match said(&mut events, session).await {
            Event::SessionChanged {
                state: SessionState::Connecting { .. },
                ..
            } => {}
            Event::SessionChanged { state, .. } => {
                break assert_eq!(state, SessionState::Connected);
            }
            other => panic!("asked again: {other:?}"),
        }
    }
    assert!(rig.remote().exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_that_joins_a_monitor_between_tries_sees_why_it_waits() {
    let Some(mut rig) = Rig::start().await else {
        return;
    };
    // Burrow has no agent to send, so every try fails the same way.
    fs::remove_dir_all(rig.server.path("builds")).unwrap();
    let session = monitor_in_background(&mut rig).await;
    let waiting = tokio::time::timeout(WAIT, async {
        loop {
            if let Event::SessionChanged {
                session: s,
                state: state @ SessionState::Connecting { .. },
                ..
            } = rig.events.recv().await.unwrap()
                && s == session
                && matches!(&state, SessionState::Connecting { step } if step.starts_with("Reconnecting"))
            {
                return state;
            }
        }
    })
    .await
    .expect("the monitor did not wait to try again");

    let (window, mut events) = late_client(&rig).await;
    assert_eq!(open(&window, &rig).await, session);
    assert!(matches!(
        said(&mut events, session).await,
        Event::SessionChanged { state, .. } if state == waiting
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_that_joins_a_running_monitor_is_asked_nothing() {
    let Some(mut rig) = Rig::start().await else {
        return;
    };
    let session = monitor_in_background(&mut rig).await;
    let asked = expect_prompt(&mut rig.events, session).await;
    answer(&rig.core, session, &asked, PromptAnswer::Confirm).await;
    assert_eq!(
        expect_state(&mut rig.events, session).await,
        SessionState::Connected
    );

    let (window, mut events) = late_client(&rig).await;
    assert_eq!(open(&window, &rig).await, session);
    assert!(matches!(
        said(&mut events, session).await,
        Event::SessionChanged {
            state: SessionState::Connected,
            ..
        }
    ));
    // The answered question is over: what follows is the monitor's own.
    tokio::time::timeout(WAIT, async {
        loop {
            match events.recv().await.unwrap() {
                Event::PromptRequested { prompt, .. } => panic!("asked again: {prompt:?}"),
                Event::MonitorSample { session: s, .. } if s == session => return,
                _ => {}
            }
        }
    })
    .await
    .expect("no sample came");
}
