//! The real `burrowd` binary, pointed at throwaway folders.

use std::path::Path;
use std::process::{Child, Command as Process};
use std::time::{Duration, Instant};

use burrow_core::Command;
use burrow_core::ipc::{Client, Role};
use tempfile::TempDir;

const WAIT: Duration = Duration::from_secs(15);

fn start(dir: &Path) -> Child {
    Process::new(env!("CARGO_BIN_EXE_burrowd"))
        .env("BURROW_RUNTIME_DIR", dir.join("run"))
        .env("BURROW_CONFIG_DIR", dir.join("config"))
        .env("BURROW_STATE_DIR", dir.join("state"))
        .env("BURROW_SSH_CONFIG", "/dev/null")
        .stderr(std::fs::File::create(dir.join("log")).unwrap())
        .spawn()
        .unwrap()
}

/// Waits for the daemon's socket. `None` when it stopped first because
/// there is no Secret Service here, as on a CI runner without a session bus.
fn wait_for_socket(dir: &Path, daemon: &mut Child) -> Option<()> {
    let deadline = Instant::now() + WAIT;
    while !dir.join("run/burrowd.sock").exists() {
        if let Some(status) = daemon.try_wait().unwrap() {
            let log = std::fs::read_to_string(dir.join("log")).unwrap();
            if log.contains("secret store") {
                eprintln!("skipping: no Secret Service ({})", log.trim());
                return None;
            }
            panic!("burrowd stopped with {status}: {log}");
        }
        assert!(Instant::now() < deadline, "burrowd did not start");
        std::thread::sleep(Duration::from_millis(50));
    }
    Some(())
}

fn wait(daemon: &mut Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(status) = daemon.try_wait().unwrap() {
            return status;
        }
        assert!(Instant::now() < deadline, "burrowd did not stop");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn one_daemon_serves_and_stops_cleanly() {
    let dir = TempDir::new().unwrap();
    let mut daemon = start(dir.path());
    if wait_for_socket(dir.path(), &mut daemon).is_none() {
        return;
    }
    let client = Client::connect(&dir.path().join("run/burrowd.sock"), Role::Cli)
        .await
        .unwrap();
    client.execute(Command::ListNodes).await.unwrap();

    // A second one sees the first and leaves it alone.
    let mut second = start(dir.path());
    assert!(wait(&mut second).success());
    client.execute(Command::ListNodes).await.unwrap();

    let killed = Process::new("kill")
        .arg(daemon.id().to_string())
        .status()
        .unwrap();
    assert!(killed.success());
    assert!(wait(&mut daemon).success());
    assert!(!dir.path().join("run/burrowd.sock").exists());
}
