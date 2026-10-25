//! One SSH terminal session: connect, check the host key, sign in, then
//! pass bytes between the remote shell and the client.

use std::fmt::Display;

use russh::ChannelMsg;
use tokio::sync::mpsc;

use super::{Input, Prompter};
use crate::api::{Event, Node, SessionState};
use crate::engine::Core;

type Result<T> = std::result::Result<T, String>;

fn text(error: impl Display) -> String {
    error.to_string()
}

/// Runs the session to the end. `Err` carries a message for the user.
pub(super) async fn run(
    core: &Core,
    node: &Node,
    (mut cols, mut rows): (u16, u16),
    mut input: mpsc::UnboundedReceiver<Input>,
    output: mpsc::Sender<Vec<u8>>,
    prompter: &Prompter,
) -> Result<()> {
    let step = |step: String| {
        core.emit(Event::SessionChanged {
            session: prompter.session,
            node: node.id,
            state: SessionState::Connecting { step },
        })
    };

    // Connecting can wait on the network or on the user, so watch for a
    // close meanwhile. Keystrokes typed before the shell starts are dropped;
    // resizes are kept.
    let setup = core.connections().get(core, node, prompter, &step);
    let closed = async {
        loop {
            match input.recv().await {
                Some(Input::Resize { cols: c, rows: r }) => (cols, rows) = (c, r),
                Some(Input::Write(_)) => {}
                Some(Input::Close) | None => return,
            }
        }
    };
    let connection = tokio::select! {
        connection = setup => connection?,
        () = closed => return Ok(()),
    };

    let mut channel = connection
        .handle()
        .channel_open_session()
        .await
        .map_err(text)?;
    channel
        .request_pty(false, "xterm-256color", cols.into(), rows.into(), 0, 0, &[])
        .await
        .map_err(text)?;
    channel.request_shell(false).await.map_err(text)?;
    core.emit(Event::SessionChanged {
        session: prompter.session,
        node: node.id,
        state: SessionState::Connected,
    });

    loop {
        tokio::select! {
            message = channel.wait() => match message {
                Some(ChannelMsg::Data { data }) | Some(ChannelMsg::ExtendedData { data, .. }) => {
                    // The client stopped reading: nobody is watching.
                    if output.send(data.to_vec()).await.is_err() {
                        break;
                    }
                }
                Some(ChannelMsg::Close) => break,
                // The channel went without closing: the connection ended.
                None => return Err(connection.closed().await),
                Some(_) => {}
            },
            message = input.recv() => match message {
                Some(Input::Write(data)) => channel.data(&data[..]).await.map_err(text)?,
                Some(Input::Resize { cols, rows }) => channel
                    .window_change(cols.into(), rows.into(), 0, 0)
                    .await
                    .map_err(text)?,
                Some(Input::Close) | None => break,
            },
        }
    }
    // Other sessions may still use the connection; it closes when the
    // last one lets go.
    let _ = channel.close().await;
    Ok(())
}
