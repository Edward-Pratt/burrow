//! One command run on an SSH host, without a terminal: what `ssh host cmd`
//! does. Its output is collected and returned whole, so it suits scripts
//! and the MCP bridge rather than people.

use std::time::Duration;

use russh::ChannelMsg;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use super::{Input, Prompter};
use crate::api::{Event, ExecResult, Node, SessionKind, SessionState};
use crate::engine::Core;
use crate::error::{Error, Result};

/// How much of each output stream is kept. The rest is read and dropped,
/// so a chatty command still finishes.
pub(crate) const MAX_OUTPUT: usize = 1 << 20;

/// How long a command may take when the caller doesn't say.
pub(crate) const DEFAULT_TIMEOUT: u32 = 60;

/// The longest a caller may ask for.
pub(crate) const MAX_TIMEOUT: u32 = 3600;

/// Runs `command` on an SSH host node, for up to `timeout` seconds
/// including connecting. A command still running then is sent SIGTERM, and
/// what it wrote so far is returned. It runs as an `exec` session, so
/// connecting can ask questions like any other session.
///
/// It only starts the session: awaiting it here would make the session's
/// task part of `Core::execute`'s future, a cycle the compiler can't prove
/// `Send`. Await the receiver instead.
pub(crate) fn start(
    core: &Core,
    node: Node,
    command: String,
    timeout: Option<u32>,
) -> Result<oneshot::Receiver<std::result::Result<ExecResult, String>>> {
    let timeout = timeout.unwrap_or(DEFAULT_TIMEOUT);
    if timeout == 0 || timeout > MAX_TIMEOUT {
        return Err(Error::invalid(
            "timeout",
            format!("must be 1 to {MAX_TIMEOUT} seconds"),
        ));
    }
    let deadline = Instant::now() + Duration::from_secs(timeout.into());
    let (tx, rx) = oneshot::channel();
    super::spawn(
        core,
        node,
        SessionKind::Exec,
        None,
        move |core, node, input, prompter| async move {
            // The caller gets the error too, not only the session's events.
            match exec(&core, &node, &command, input, &prompter, deadline).await {
                Ok(Some(result)) => {
                    let _ = tx.send(Ok(result));
                    Ok(())
                }
                // A client closed it before it finished.
                Ok(None) => Ok(()),
                Err(error) => {
                    let _ = tx.send(Err(error.clone()));
                    Err(error)
                }
            }
        },
    );
    Ok(rx)
}

async fn exec(
    core: &Core,
    node: &Node,
    command: &str,
    mut input: mpsc::UnboundedReceiver<Input>,
    prompter: &Prompter,
    deadline: Instant,
) -> std::result::Result<Option<ExecResult>, String> {
    let step = |step: String| {
        core.emit(Event::SessionChanged {
            session: prompter.session(),
            node: node.id,
            state: SessionState::Connecting { step },
        })
    };
    let closed = async {
        while let Some(message) = input.recv().await {
            if matches!(message, Input::Close) {
                return;
            }
        }
    };
    let setup = core.connections().get(core, node, prompter, &step);
    let connection = tokio::select! {
        connection = setup => connection?,
        () = closed => return Ok(None),
        () = tokio::time::sleep_until(deadline) => {
            return Err("timed out before the command started".to_string());
        }
    };
    let mut channel = connection
        .handle()
        .channel_open_session()
        .await
        .map_err(|e| e.to_string())?;
    channel
        .exec(true, command)
        .await
        .map_err(|e| e.to_string())?;
    core.emit(Event::SessionChanged {
        session: prompter.session(),
        node: node.id,
        state: SessionState::Connected,
    });

    let mut result = ExecResult::default();
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    let mut truncated = false;
    let mut keep = |buffer: &mut Vec<u8>, data: &[u8]| {
        let room = MAX_OUTPUT.saturating_sub(buffer.len());
        truncated |= data.len() > room;
        buffer.extend_from_slice(&data[..data.len().min(room)]);
    };
    loop {
        tokio::select! {
            message = channel.wait() => match message {
                Some(ChannelMsg::Data { data }) => keep(&mut stdout, &data),
                Some(ChannelMsg::ExtendedData { data, .. }) => keep(&mut stderr, &data),
                Some(ChannelMsg::ExitStatus { exit_status }) => result.status = Some(exit_status),
                Some(ChannelMsg::ExitSignal { signal_name, .. }) => {
                    result.signal = Some(format!("{signal_name:?}"));
                }
                Some(ChannelMsg::Close) => break,
                // The channel went without closing: the connection ended.
                None => return Err(connection.closed().await),
                Some(_) => {}
            },
            // Without a terminal, closing the channel may leave the command
            // running, so ask the server to stop it first. Servers that
            // don't take signals ignore this.
            () = tokio::time::sleep_until(deadline) => {
                let _ = channel.signal(russh::Sig::TERM).await;
                result.timed_out = true;
                break;
            }
            message = input.recv() => if matches!(message, Some(Input::Close) | None) {
                let _ = channel.close().await;
                return Ok(None);
            },
        }
    }
    let _ = channel.close().await;
    result.truncated = truncated;
    result.stdout = String::from_utf8_lossy(&stdout).into_owned();
    result.stderr = String::from_utf8_lossy(&stderr).into_owned();
    Ok(Some(result))
}
