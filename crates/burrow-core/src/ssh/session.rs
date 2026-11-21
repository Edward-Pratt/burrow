//! One SSH terminal session: connect, check the host key, sign in, then
//! pass bytes between the remote shell, or a command, and the client.

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

/// The node as a terminal signs in to it: with `props.shell_alias`, when
/// it has one, in place of its alias. A host can then be monitored with a
/// key limited to that (one installed with `restrict`, as a user without
/// sudo), and give shells with another, as one node. The shell alias is
/// read and checked exactly as the alias is, and its connection is its own.
pub(super) fn for_shell(mut node: Node) -> Node {
    let shell = node.props.get("shell_alias").and_then(|v| v.as_str());
    if let Some(shell) = shell.map(str::to_owned) {
        node.props.insert("alias".into(), shell.into());
    }
    node
}

const REFUSED_PTY: &str = "the server refused a terminal. The key Burrow signs in with may be \
     limited with no-pty or restrict in authorized_keys there, as a monitoring key often is";

/// Waits for the server's answer to a request sent with a reply wanted.
async fn answered_yes(channel: &mut russh::Channel<russh::client::Msg>) -> Result<bool> {
    loop {
        match channel.wait().await {
            Some(ChannelMsg::Success) => return Ok(true),
            Some(ChannelMsg::Failure) => return Ok(false),
            Some(_) => {}
            None => return Err("the connection closed while asking for a terminal".into()),
        }
    }
}

/// Runs the session to the end. `Err` carries a message for the user.
pub(super) async fn run(
    core: &Core,
    node: &Node,
    (mut cols, mut rows): (u16, u16),
    command: Option<&str>,
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
    // Asked with a reply: a server that refuses a terminal, as a key
    // limited with no-pty or restrict does, would otherwise give a shell
    // with no prompt or echo, where Enter never ends a line.
    channel
        .request_pty(true, "xterm-256color", cols.into(), rows.into(), 0, 0, &[])
        .await
        .map_err(text)?;
    if !answered_yes(&mut channel).await? {
        return Err(REFUSED_PTY.to_string());
    }
    match command {
        Some(command) => channel.exec(false, command).await.map_err(text)?,
        None => channel.request_shell(false).await.map_err(text)?,
    }
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
