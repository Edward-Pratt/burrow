//! The proxy over in-memory streams, with the test playing both the phone
//! and the daemon.

use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, DuplexStream};

use super::*;

const WAIT: Duration = Duration::from_secs(5);

fn hello(role: &str) -> String {
    let api = burrow_api::API_VERSION;
    format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"api":{api},"role":"{role}","build":"phone"}}}}"#
    )
}

/// Runs the proxy on `sent` as the client's whole input, with a daemon
/// that must never be reached, and returns its error and what the client
/// was told.
async fn refused(role: Role, sent: &str) -> (String, Value) {
    let (mut client, output) = tokio::io::duplex(1 << 16);
    let daemon = async { panic!("a refused client reached the daemon") };
    let result: Result<(), String> =
        relay::<_, _, DuplexStream>(role, sent.as_bytes(), output, daemon).await;
    let error = result.unwrap_err();
    let mut told = String::new();
    client.read_to_string(&mut told).await.unwrap();
    let told: Value = serde_json::from_str(told.trim()).unwrap();
    assert_eq!(told["error"]["message"], error.as_str());
    (error, told)
}

#[tokio::test]
async fn anything_but_initialize_with_its_role_is_refused_first() {
    let list = r#"{"jsonrpc":"2.0","id":7,"method":"execute","params":{"type":"list_nodes"}}"#;
    let (error, told) = refused(Role::Mobile, &format!("{list}\n{}\n", hello("mobile"))).await;
    assert_eq!(error, "send initialize first");
    assert_eq!(told["id"], 7);

    let (error, told) = refused(Role::Mobile, &format!("{}\n", hello("gui"))).await;
    assert_eq!(error, "this key may only connect as mobile, not gui");
    assert_eq!(told["id"], 1);
    let (error, _) = refused(Role::Mcp, &format!("{}\n", hello("mobile"))).await;
    assert!(error.contains("only connect as mcp"), "{error}");

    let (error, told) = refused(Role::Mobile, "not json\n").await;
    assert!(error.starts_with("the first line must be a JSON-RPC request"));
    assert_eq!(told["id"], Value::Null);

    // A notification would get no answer, so the client couldn't tell.
    let notification = hello("mobile").replace(r#""id":1,"#, "");
    let (error, _) = refused(Role::Mobile, &notification).await;
    assert!(error.contains("with an id"), "{error}");

    let (error, _) = refused(Role::Mobile, &hello("root")).await;
    assert!(error.starts_with("bad initialize"), "{error}");
}

#[tokio::test]
async fn a_first_line_too_long_is_dropped_before_the_daemon() {
    let long = format!("{}\n", " ".repeat(MAX_FIRST + 1));
    let daemon = async { panic!("a refused client reached the daemon") };
    let result = relay::<_, _, DuplexStream>(Role::Mobile, long.as_bytes(), Vec::new(), daemon);
    assert!(result.await.unwrap_err().contains("over"));
}

/// Read as a map, the last of two `role` keys wins. The daemon is sent
/// the line as the proxy read it, so it can't read the first one instead.
#[tokio::test]
async fn the_daemon_gets_initialize_as_checked_and_then_every_byte() {
    let (phone, proxy_in) = tokio::io::duplex(1 << 16);
    let (proxy_out, phone_reads) = tokio::io::duplex(1 << 16);
    let (daemon, proxy_daemon) = tokio::io::duplex(1 << 16);
    let relaying = tokio::spawn(relay(Role::Mobile, proxy_in, proxy_out, async {
        Ok(proxy_daemon)
    }));
    let (phone_read, mut phone) = (BufReader::new(phone_reads), phone);
    let mut daemon = BufReader::new(daemon);

    let twice = hello("mobile").replace(r#""role":"mobile""#, r#""role":"gui","role":"mobile""#);
    // The next line in the same write, so the proxy has read past the
    // first line before it connects.
    let next = r#"{"jsonrpc":"2.0","id":2,"method":"execute","params":{"type":"list_nodes"}}"#;
    phone
        .write_all(format!("{twice}\n{next}\n").as_bytes())
        .await
        .unwrap();

    let mut line = String::new();
    daemon.read_line(&mut line).await.unwrap();
    assert!(!line.contains("gui"), "{line}");
    let sent: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(sent["method"], "initialize");
    assert_eq!(sent["id"], 1);
    assert_eq!(sent["params"]["role"], "mobile");

    // Nothing more reaches the daemon until it has answered; an event
    // before the answer goes through.
    let early = tokio::time::timeout(Duration::from_millis(100), daemon.read_line(&mut line));
    assert!(
        early.await.is_err(),
        "sent on before initialize was answered"
    );
    let event = "{\"jsonrpc\":\"2.0\",\"method\":\"resync\"}\n";
    let answer = "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n";
    daemon
        .write_all(format!("{event}{answer}").as_bytes())
        .await
        .unwrap();
    let mut lines = phone_read.lines();
    for expected in [event, answer] {
        let got = tokio::time::timeout(WAIT, lines.next_line()).await.unwrap();
        assert_eq!(format!("{}\n", got.unwrap().unwrap()), expected);
    }
    line.clear();
    daemon.read_line(&mut line).await.unwrap();
    assert_eq!(line.trim_end(), next);

    let reply = "{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"type\":\"nodes\",\"nodes\":[]}}\n";
    daemon.write_all(reply.as_bytes()).await.unwrap();
    let got = tokio::time::timeout(WAIT, lines.next_line()).await.unwrap();
    assert_eq!(format!("{}\n", got.unwrap().unwrap()), reply);

    // The phone hanging up reaches the daemon as the end of its input,
    // and the daemon closing its side ends the proxy.
    drop(phone);
    line.clear();
    let read = tokio::time::timeout(WAIT, daemon.read_line(&mut line));
    assert_eq!(read.await.unwrap().unwrap(), 0);
    drop(daemon);
    let done = tokio::time::timeout(WAIT, relaying).await.unwrap();
    done.unwrap().unwrap();
}

/// A daemon from before it closed refused connections would leave one
/// open for another `initialize`, so the proxy gives up itself.
#[tokio::test]
async fn a_refused_initialize_ends_the_proxy_before_anything_else_is_sent() {
    let (proxy_out, mut phone_reads) = tokio::io::duplex(1 << 16);
    let (daemon, proxy_daemon) = tokio::io::duplex(1 << 16);
    let next = format!("{}\n", hello("gui"));
    let sent = format!("{}\n{next}", hello("mobile"));
    let relaying = tokio::spawn(async move {
        let input = sent.as_bytes();
        relay(Role::Mobile, input, proxy_out, async { Ok(proxy_daemon) }).await
    });
    let mut daemon = BufReader::new(daemon);
    let mut line = String::new();
    daemon.read_line(&mut line).await.unwrap();
    let error = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"needs API 1"}}"#;
    daemon
        .write_all(format!("{error}\n").as_bytes())
        .await
        .unwrap();

    let done = tokio::time::timeout(WAIT, relaying).await.unwrap().unwrap();
    assert_eq!(done.unwrap_err(), "burrowd refused initialize: needs API 1");
    let mut told = String::new();
    phone_reads.read_to_string(&mut told).await.unwrap();
    assert_eq!(told, format!("{error}\n"));
    // The proxy has dropped its end, and never sent the second line.
    line.clear();
    assert_eq!(daemon.read_line(&mut line).await.unwrap(), 0);
}
