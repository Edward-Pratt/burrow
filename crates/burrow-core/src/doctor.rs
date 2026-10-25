//! Checks of the machine Burrow runs on, for `burrow doctor` and for the
//! window when it can't reach the core. Someone trying Burrow who is
//! missing a piece should learn which one and how to get it, without
//! reading the daemon's log.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use crate::engine::Paths;
use crate::ipc::{self, Role};
use crate::secrets::KeyringStore;
use crate::ssh;

/// How long `ssh` may take to say its version.
const SSH_TIMEOUT: Duration = Duration::from_secs(10);

const INSTALL_SSH: &str =
    "Install OpenSSH: pacman -S openssh, apt install openssh-client or dnf install openssh-clients";

const START_AGENT: &str = "Start one and add your key: `eval $(ssh-agent)` (on Arch, `systemctl \
                           --user enable --now ssh-agent.service`), then `ssh-add`. Without it \
                           Burrow asks for the key's passphrase on each connection";

/// One thing Burrow needs, or is better with, and whether it is there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    pub name: String,
    /// What was found, in a line.
    pub found: String,
    /// What to do about it. `None` when all is well.
    pub fix: Option<String>,
    /// Burrow runs without this, with less.
    pub optional: bool,
}

impl Check {
    pub fn ok(&self) -> bool {
        self.fix.is_none()
    }

    /// Whether Burrow can't run until this is fixed.
    pub fn blocks(&self) -> bool {
        !self.ok() && !self.optional
    }

    fn passed(name: impl Into<String>, found: impl Into<String>) -> Check {
        Check {
            name: name.into(),
            found: found.into(),
            fix: None,
            optional: false,
        }
    }

    fn needed(name: impl Into<String>, found: impl Into<String>, fix: impl Into<String>) -> Check {
        Check {
            name: name.into(),
            found: found.into(),
            fix: Some(fix.into()),
            optional: false,
        }
    }

    fn wanted(name: impl Into<String>, found: impl Into<String>, fix: impl Into<String>) -> Check {
        Check {
            fix: Some(fix.into()),
            optional: true,
            ..Check::passed(name, found)
        }
    }
}

/// Every check, ending with the daemon itself, which is only tried once
/// the rest passes. Starting it is what any client does; nothing else is
/// changed, except that the folders Burrow makes on its first run are made.
pub async fn run() -> Vec<Check> {
    let mut checks = environment().await;
    checks.push(if checks.iter().any(Check::blocks) {
        Check::needed("burrowd", "not tried", "Fix the problems above first")
    } else {
        daemon().await
    });
    checks
}

/// Everything but the daemon, for a client that has just failed to reach
/// it and wants to say why.
pub async fn environment() -> Vec<Check> {
    let paths = Paths::from_env();
    let mut checks = vec![
        folders(&paths),
        runtime_dir(),
        openssh(paths.as_ref().ok().and_then(|p| p.ssh_config.as_deref())).await,
        secret_service().await,
        ssh_agent(),
    ];
    for arch in ssh::agent::ARCHES {
        checks.push(agent_build(arch).await);
    }
    checks
}

fn folders(paths: &crate::Result<Paths>) -> Check {
    let paths = match paths {
        Ok(paths) => paths,
        Err(e) => {
            return Check::needed(
                "Folders",
                e.to_string(),
                "Set HOME, or BURROW_CONFIG_DIR and BURROW_STATE_DIR",
            );
        }
    };
    for dir in [&paths.config_dir, &paths.state_dir] {
        if let Err(e) = std::fs::create_dir_all(dir) {
            return Check::needed(
                "Folders",
                format!("{}: {e}", dir.display()),
                "Make it writable, or point BURROW_CONFIG_DIR or BURROW_STATE_DIR at a folder \
                 that is",
            );
        }
    }
    Check::passed(
        "Folders",
        format!(
            "config in {}, state in {}",
            paths.config_dir.display(),
            paths.state_dir.display()
        ),
    )
}

fn runtime_dir() -> Check {
    match ipc::runtime_dir().and_then(|dir| ipc::ensure_private_dir(&dir).map(|()| dir)) {
        Ok(dir) => Check::passed("Socket folder", dir.display().to_string()),
        Err(e) => Check::needed(
            "Socket folder",
            e.to_string(),
            "Log in through a desktop session or systemd, which set XDG_RUNTIME_DIR, or set \
             BURROW_RUNTIME_DIR to a folder only you can open",
        ),
    }
}

/// `ssh` must be on `PATH`, and the config it reads must parse, since every
/// host's settings come from `ssh -G`.
async fn openssh(config: Option<&Path>) -> Check {
    let version = tokio::process::Command::new("ssh")
        .arg("-V")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output();
    let version = match tokio::time::timeout(SSH_TIMEOUT, version).await {
        Ok(Ok(output)) if output.status.success() => String::from_utf8_lossy(&output.stderr)
            .lines()
            .next()
            .unwrap_or("OpenSSH")
            .trim()
            .to_string(),
        Ok(Ok(output)) => {
            return Check::needed(
                "OpenSSH",
                format!("`ssh -V` failed: {}", output.status),
                INSTALL_SSH,
            );
        }
        Ok(Err(e)) => {
            return Check::needed("OpenSSH", format!("ssh is not on PATH: {e}"), INSTALL_SSH);
        }
        Err(_) => return Check::needed("OpenSSH", "`ssh -V` took too long", INSTALL_SSH),
    };
    match ssh::config::resolve("localhost", config).await {
        Ok(_) => Check::passed("OpenSSH", version),
        Err(e) => Check::needed(
            "OpenSSH",
            format!("{version}; {e}"),
            "Fix your OpenSSH config; `ssh -G localhost` shows the error",
        ),
    }
}

/// Opening the keyring is a D-Bus call, so it runs off the async threads.
async fn secret_service() -> Check {
    let opened = tokio::task::spawn_blocking(KeyringStore::new).await;
    let error = match opened {
        Ok(Ok(_)) => return Check::passed("Secret Service", "reachable on the session bus"),
        Ok(Err(e)) => e.to_string(),
        Err(e) => format!("the check failed: {e}"),
    };
    Check::needed(
        "Secret Service",
        error,
        "Install and start one: gnome-keyring, KWallet or KeePassXC with its Secret Service \
         integration on. Burrow keeps passwords in it",
    )
}

/// The SSH agent's socket: `$SSH_AUTH_SOCK`, or the socket of OpenSSH's
/// own `ssh-agent.socket` user unit when nothing set the variable, which a
/// daemon started by systemd rather than a desktop session finds itself.
pub(crate) fn ssh_agent_socket(var: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    if let Some(socket) = var("SSH_AUTH_SOCK") {
        return Some(PathBuf::from(socket));
    }
    let socket = PathBuf::from(var("XDG_RUNTIME_DIR")?).join("ssh-agent.socket");
    socket.exists().then_some(socket)
}

fn ssh_agent() -> Check {
    match ssh_agent_socket(|name| std::env::var_os(name)) {
        Some(socket) if socket.exists() => Check::passed("SSH agent", socket.display().to_string()),
        Some(socket) => Check::wanted(
            "SSH agent",
            format!("SSH_AUTH_SOCK is {}, which doesn't exist", socket.display()),
            START_AGENT,
        ),
        None => Check::wanted("SSH agent", "none", START_AGENT),
    }
}

async fn agent_build(arch: &str) -> Check {
    let name = format!("burrow-agent ({arch})");
    match ssh::agent::find_build(arch, None).await {
        Ok(path) => Check::passed(name, path.display().to_string()),
        Err(_) => {
            let looked: Vec<String> = ssh::agent::candidates(arch, None)
                .iter()
                .map(|path| path.display().to_string())
                .collect();
            Check::wanted(
                name,
                format!("none in {}", looked.join(", ")),
                format!(
                    "Monitors on {arch} servers need it. Run `rustup target add \
                     {arch}-unknown-linux-musl`, then `cargo build -p burrow-agent --profile agent \
                     --target {arch}-unknown-linux-musl`, and run Burrow from that workspace, or \
                     put the build in one of the folders above"
                ),
            )
        }
    }
}

async fn daemon() -> Check {
    match ipc::connect(Role::Cli).await {
        Ok(client) => Check::passed("burrowd", format!("{} is running", client.info().build)),
        Err(e) => Check::needed(
            "burrowd",
            e.to_string(),
            "Run `burrow daemon run` in a terminal to see why",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn socket(vars: &[(&str, &str)]) -> Option<PathBuf> {
        ssh_agent_socket(|name| {
            vars.iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| OsString::from(v))
        })
    }

    #[test]
    fn the_agent_socket_is_the_variable_or_the_user_unit() {
        assert_eq!(
            socket(&[
                ("SSH_AUTH_SOCK", "/tmp/agent"),
                ("XDG_RUNTIME_DIR", "/run/user/1")
            ]),
            Some(PathBuf::from("/tmp/agent"))
        );
        // The unit's socket only counts when it exists.
        let runtime = tempfile::TempDir::new().unwrap();
        let path = runtime.path().to_str().unwrap();
        assert_eq!(socket(&[("XDG_RUNTIME_DIR", path)]), None);
        std::fs::write(runtime.path().join("ssh-agent.socket"), "").unwrap();
        assert_eq!(
            socket(&[("XDG_RUNTIME_DIR", path)]),
            Some(runtime.path().join("ssh-agent.socket"))
        );
        assert_eq!(socket(&[]), None);
    }

    #[test]
    fn only_a_failed_required_check_blocks() {
        assert!(!Check::passed("a", "fine").blocks());
        assert!(Check::needed("a", "gone", "get it").blocks());
        let wanted = Check::wanted("a", "gone", "get it");
        assert!(!wanted.ok());
        assert!(!wanted.blocks());
    }
}
