//! Finding the user's SSH agent, which the daemon uses and the doctor checks.

use std::ffi::OsString;
use std::path::PathBuf;

use super::Check;

const START_AGENT: &str = "Start one and add your key: `eval $(ssh-agent)` (on Arch, `systemctl \
                           --user enable --now ssh-agent.service`), then `ssh-add`. Without it \
                           Burrow asks for the key's passphrase on each connection";

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

pub(super) fn ssh_agent() -> Check {
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
}
