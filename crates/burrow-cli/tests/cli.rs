//! The real `burrow` binary, which starts its own daemon in throwaway
//! folders.

use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

fn burrow(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_burrow"))
        .args(args)
        .env("BURROW_RUNTIME_DIR", dir.join("run"))
        .env("BURROW_CONFIG_DIR", dir.join("config"))
        .env("BURROW_STATE_DIR", dir.join("state"))
        .env("BURROW_SSH_CONFIG", "/dev/null")
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Stops the daemon a test started, even when the test fails.
struct Daemon<'a>(&'a Path);

impl Drop for Daemon<'_> {
    fn drop(&mut self) {
        burrow(self.0, &["daemon", "stop"]);
    }
}

#[test]
fn the_cli_starts_a_daemon_and_drives_it() {
    let dir = TempDir::new().unwrap();
    let dir = dir.path();
    let _daemon = Daemon(dir);

    let create = r#"{"type": "create_node", "kind": "folder", "name": "Work"}"#;
    let created = burrow(dir, &["call", create]);
    if !created.status.success() {
        let log = std::fs::read_to_string(dir.join("state/burrowd.log")).unwrap_or_default();
        if log.contains("secret store") {
            eprintln!("skipping: no Secret Service ({})", log.trim());
            return;
        }
        panic!(
            "call failed: {}\n{log}",
            String::from_utf8_lossy(&created.stderr)
        );
    }
    assert!(stdout(&created).contains(r#""name": "Work""#));

    let listed = burrow(dir, &["ls"]);
    assert!(listed.status.success());
    assert!(
        stdout(&listed).starts_with("Work\tfolder\t"),
        "{}",
        stdout(&listed)
    );

    let missing = burrow(dir, &["exec", "nowhere", "true"]);
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("no node is called \"nowhere\""));

    let host =
        r#"{"type": "create_node", "kind": "ssh-host", "name": "web", "props": {"alias": "web"}}"#;
    assert!(burrow(dir, &["call", host]).status.success());
    let granted = burrow(dir, &["mcp", "grant", "web", "exec", "files-read", "--yes"]);
    assert!(granted.status.success(), "{:?}", granted);
    assert!(String::from_utf8_lossy(&granted.stderr).contains("alias = \"web\""));
    let listed = stdout(&burrow(dir, &["mcp", "ls"]));
    assert!(listed.starts_with("web\texec,files-read\t"), "{listed}");
    // Not on a node where it means nothing.
    assert!(
        !burrow(dir, &["mcp", "grant", "Work", "exec", "--yes"])
            .status
            .success()
    );
    assert!(
        burrow(dir, &["mcp", "revoke", "web", "exec"])
            .status
            .success()
    );
    assert!(stdout(&burrow(dir, &["mcp", "ls"])).starts_with("web\tfiles-read\t"));
    assert!(burrow(dir, &["mcp", "revoke", "web"]).status.success());
    assert_eq!(stdout(&burrow(dir, &["mcp", "ls"])), "");

    let status = burrow(dir, &["daemon", "status"]);
    assert!(status.status.success(), "{}", stdout(&status));
    assert!(burrow(dir, &["daemon", "stop"]).status.success());
    assert_eq!(burrow(dir, &["daemon", "status"]).status.code(), Some(3));
}

/// Sends `requests` to `burrow mcp serve` and returns its replies.
fn mcp(dir: &Path, requests: &[serde_json::Value]) -> Vec<serde_json::Value> {
    use std::io::Write;
    let mut child = Command::new(env!("CARGO_BIN_EXE_burrow"))
        .args(["mcp", "serve"])
        .env("BURROW_RUNTIME_DIR", dir.join("run"))
        .env("BURROW_CONFIG_DIR", dir.join("config"))
        .env("BURROW_STATE_DIR", dir.join("state"))
        .env("BURROW_SSH_CONFIG", "/dev/null")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    for request in requests {
        writeln!(stdin, "{request}").unwrap();
    }
    drop(stdin);
    let output = child.wait_with_output().unwrap();
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn the_mcp_server_speaks_both_eras_and_respects_grants() {
    use serde_json::json;
    let dir = TempDir::new().unwrap();
    let dir = dir.path();
    let _daemon = Daemon(dir);
    let host =
        r#"{"type": "create_node", "kind": "ssh-host", "name": "web", "props": {"alias": "web"}}"#;
    let created = burrow(dir, &["call", host]);
    if !created.status.success() {
        let log = std::fs::read_to_string(dir.join("state/burrowd.log")).unwrap_or_default();
        assert!(log.contains("secret store"), "{log}");
        eprintln!("skipping: no Secret Service");
        return;
    }

    let modern = json!({"io.modelcontextprotocol/protocolVersion": "2026-07-28"});
    let replies = mcp(
        dir,
        &[
            json!({"jsonrpc": "2.0", "id": 1, "method": "server/discover", "params": {"_meta": modern}}),
            json!({"jsonrpc": "2.0", "id": 2, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}}),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            json!({"jsonrpc": "2.0", "id": 3, "method": "tools/list", "params": {}}),
            json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "list_nodes", "arguments": {}}}),
            json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {"name": "run_command", "arguments": {"node": "web", "command": "id"}}}),
            json!({"jsonrpc": "2.0", "id": 6, "method": "ping", "params": {"_meta": {"io.modelcontextprotocol/protocolVersion": "1900-01-01"}}}),
        ],
    );
    // The notification gets no reply.
    assert_eq!(replies.len(), 6, "{replies:#?}");
    assert!(replies[0]["result"]["supportedVersions"][0] == "2026-07-28");
    assert_eq!(replies[0]["result"]["resultType"], "complete");
    assert_eq!(replies[1]["result"]["protocolVersion"], "2025-06-18");
    let tools = replies[2]["result"]["tools"].as_array().unwrap();
    assert!(tools.iter().any(|t| t["name"] == "query"));
    let listed = replies[3]["result"]["content"][0]["text"].as_str().unwrap();
    assert!(listed.contains(r#""allowed": []"#), "{listed}");
    // Burrow refuses it, which the assistant sees as a failed tool.
    assert_eq!(replies[4]["result"]["isError"], true);
    let refused = replies[4]["result"]["content"][0]["text"].as_str().unwrap();
    assert!(refused.contains("exec"), "{refused}");
    assert_eq!(replies[5]["error"]["code"], -32022);

    // Granted, list_nodes says so.
    assert!(
        burrow(dir, &["mcp", "grant", "web", "exec", "--yes"])
            .status
            .success()
    );
    let replies = mcp(
        dir,
        &[
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "list_nodes", "arguments": {}}}),
        ],
    );
    let listed = replies[0]["result"]["content"][0]["text"].as_str().unwrap();
    assert!(listed.contains(r#""exec""#), "{listed}");
}
