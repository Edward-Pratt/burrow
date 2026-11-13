//! Finding the user's SSH agent, which the daemon uses and the doctor checks.

use std::ffi::OsString;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use super::Check;

const START_AGENT: &str = "Start one and add your key: `eval $(ssh-agent)` (on Arch, `systemctl \
                           --user enable --now ssh-agent.service`), then `ssh-add`. Without it \
                           Burrow asks for the key's passphrase on each connection";

/// The SSH agent's socket: `$SSH_AUTH_SOCK` when an agent answers there,
/// or else the socket of OpenSSH's own `ssh-agent.socket` user unit when it
/// exists. A daemon started by systemd rather than a desktop session has no
/// variable, and one started from an old shell can have a variable naming
/// an agent that has since gone. With neither, the variable as it is, so
/// the doctor can say what's wrong with it.
pub(crate) fn ssh_agent_socket(var: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let set = var("SSH_AUTH_SOCK").map(PathBuf::from);
    if set.as_deref().is_some_and(answers) {
        return set;
    }
    let unit = var("XDG_RUNTIME_DIR").map(|dir| PathBuf::from(dir).join("ssh-agent.socket"));
    match unit {
        Some(unit) if unit.exists() => Some(unit),
        _ => set,
    }
}

/// Whether something listens on the socket. The unit's socket starts its
/// agent on the first connection, so it answers too.
fn answers(socket: &Path) -> bool {
    UnixStream::connect(socket).is_ok()
}

pub(super) fn ssh_agent() -> Check {
    let set = std::env::var_os("SSH_AUTH_SOCK").map(PathBuf::from);
    match ssh_agent_socket(|name| std::env::var_os(name)) {
        Some(socket) if answers(&socket) => {
            let found = match set {
                Some(set) if set != socket => format!(
                    "{} (SSH_AUTH_SOCK is {}, where nothing answers)",
                    socket.display(),
                    set.display()
                ),
                _ => socket.display().to_string(),
            };
            Check::passed("SSH agent", found)
        }
        Some(socket) => Check::wanted(
            "SSH agent",
            format!("nothing answers on {}", socket.display()),
            START_AGENT,
        ),
        None => Check::wanted("SSH agent", "none", START_AGENT),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixListener;

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

        // A variable naming an agent that has gone gives way to the unit.
        let gone = runtime.path().join("gone");
        let gone = gone.to_str().unwrap();
        assert_eq!(
            socket(&[("SSH_AUTH_SOCK", gone), ("XDG_RUNTIME_DIR", path)]),
            Some(runtime.path().join("ssh-agent.socket"))
        );
        // One that answers is kept.
        let live = runtime.path().join("live");
        let _listener = UnixListener::bind(&live).unwrap();
        assert_eq!(
            socket(&[
                ("SSH_AUTH_SOCK", live.to_str().unwrap()),
                ("XDG_RUNTIME_DIR", path)
            ]),
            Some(live.clone())
        );
    }
}
