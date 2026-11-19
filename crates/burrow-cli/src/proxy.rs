//! `burrow proxy --role <role>`: what an SSH key's forced command runs, so
//! the phone can reach the daemon. It finds or starts the daemon, then
//! passes JSON-RPC lines between its stdin and stdout and the socket.
//!
//! The key's `authorized_keys` line fixes the role, so the proxy checks the
//! first line is `initialize` with that role before anything else, the
//! daemon included, is touched. Only the first line: the daemon answers
//! `initialize` before reading on, and closes a connection whose
//! `initialize` failed, so a later line can't change the role.

use std::future::Future;
use std::process::ExitCode;

use burrow_api::rpc::{Initialize, Role, read_line};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

/// The longest first line read. `initialize` is a few dozen bytes; the
/// lines after it go to the daemon, which has a limit of its own.
const MAX_FIRST: usize = 64 << 10;

/// JSON-RPC's code for a request that may not be made.
const INVALID_REQUEST: i64 = -32600;

/// Runs the proxy on stdin and stdout.
pub async fn run(role: Role) -> Result<ExitCode, String> {
    let daemon = async move {
        burrow_core::ipc::connect_stream(role)
            .await
            .map_err(|e| format!("could not reach burrowd: {e}"))
    };
    relay(role, tokio::io::stdin(), tokio::io::stdout(), daemon).await?;
    Ok(ExitCode::SUCCESS)
}

/// Checks the first line from `input`, then connects with `daemon` and
/// passes bytes both ways until the daemon's side ends. `daemon` is a
/// future, and futures do nothing until awaited, so a refused client
/// never starts a daemon.
pub async fn relay<I, O, D>(
    role: Role,
    input: I,
    mut output: O,
    daemon: impl Future<Output = Result<D, String>>,
) -> Result<(), String>
where
    I: AsyncRead + Unpin,
    O: AsyncWrite + Unpin,
    D: AsyncRead + AsyncWrite + Unpin,
{
    // Kept for the rest: it may hold bytes from past the first line.
    let mut input = BufReader::new(input);
    let mut line = Vec::new();
    match read_line(&mut input, &mut line, MAX_FIRST).await {
        Ok(Some(())) => {}
        // Gone before saying anything.
        Ok(None) => return Ok(()),
        Err(e) => return Err(format!("the client {e}")),
    }
    let (id, hello) = match check(role, &line) {
        Ok(checked) => checked,
        Err((id, message)) => return Err(refuse(&mut output, id, message).await),
    };
    let daemon = match daemon.await {
        Ok(daemon) => daemon,
        Err(message) => return Err(refuse(&mut output, id, message).await),
    };
    let (mut from_daemon, mut to_daemon) = tokio::io::split(daemon);
    // What was checked, written out again, rather than the client's own
    // bytes: a line with a key twice could otherwise read one way here
    // and another way in the daemon.
    let hello = json!({"jsonrpc": "2.0", "id": id, "method": "initialize", "params": hello});
    let sent = to_daemon.write_all(format!("{hello}\n").as_bytes()).await;
    sent.map_err(|e| format!("could not write to burrowd: {e}"))?;

    let upstream = async {
        // The client closing its side ends the daemon's connection, which
        // then ends the other direction; until then this waits.
        if tokio::io::copy(&mut input, &mut to_daemon).await.is_ok() {
            let _ = to_daemon.shutdown().await;
        }
        std::future::pending::<()>().await
    };
    tokio::select! {
        done = tokio::io::copy(&mut from_daemon, &mut output) => {
            done.map_err(|e| format!("lost the connection: {e}"))?;
        }
        () = upstream => {}
    }
    Ok(())
}

#[derive(Deserialize)]
struct First {
    #[serde(default)]
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
}

/// The first line's id and `initialize`, or the id and why it is refused.
fn check(role: Role, line: &[u8]) -> Result<(Value, Initialize), (Value, String)> {
    let first: First = serde_json::from_slice(line).map_err(|e| {
        let message = format!("the first line must be a JSON-RPC request: {e}");
        (Value::Null, message)
    })?;
    if !matches!(first.id, Value::Number(_) | Value::String(_)) {
        let message = "initialize must be a request, with an id".to_string();
        return Err((Value::Null, message));
    }
    let id = first.id;
    if first.method != "initialize" {
        return Err((id, "send initialize first".to_string()));
    }
    let hello: Initialize = match serde_json::from_value(first.params) {
        Ok(hello) => hello,
        Err(e) => return Err((id, format!("bad initialize: {e}"))),
    };
    if hello.role != role {
        let message = format!("this key may only connect as {role}, not {}", hello.role);
        return Err((id, message));
    }
    Ok((id, hello))
}

/// Answers the client's request with `message`, and returns it.
async fn refuse<O: AsyncWrite + Unpin>(output: &mut O, id: Value, message: String) -> String {
    let error = json!({"code": INVALID_REQUEST, "message": message});
    let reply = json!({"jsonrpc": "2.0", "id": id, "error": error});
    let _ = output.write_all(format!("{reply}\n").as_bytes()).await;
    let _ = output.flush().await;
    message
}

#[cfg(test)]
mod tests;
