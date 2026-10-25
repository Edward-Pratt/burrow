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

    let status = burrow(dir, &["daemon", "status"]);
    assert!(status.status.success(), "{}", stdout(&status));
    assert!(burrow(dir, &["daemon", "stop"]).status.success());
    assert_eq!(burrow(dir, &["daemon", "status"]).status.code(), Some(3));
}
