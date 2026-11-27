//! Reading `ssh -G` output into a [`HostConfig`].

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::tokens::{self, Local};
use super::{HostConfig, StrictHostKeyChecking};

/// Parses `ssh -G` output: one `keyword value` pair per line, keywords in
/// lowercase, list settings repeated or space-separated.
/// `alias` is the name `ssh -G` was given, which `IdentityFile`'s `%n`
/// stands for.
pub(super) fn parse(
    output: &str,
    alias: &str,
    local: &Local,
    auth_sock: Option<PathBuf>,
) -> HostConfig {
    let home = local.home.as_path();
    let mut config = HostConfig {
        hostname: String::new(),
        port: 22,
        user: String::new(),
        identity_files: Vec::new(),
        identity_agent: auth_sock.clone(),
        identities_only: false,
        user_known_hosts: vec![home.join(".ssh/known_hosts")],
        global_known_hosts: Vec::new(),
        strict_host_key_checking: StrictHostKeyChecking::Ask,
        hash_known_hosts: false,
        host_key_alias: None,
        proxy_jump: None,
        proxy_command: None,
        connect_timeout: None,
        server_alive_interval: None,
    };
    let expand = |value: &str| expand_path(value, home);
    // `none` means no file at all, not a file named "none".
    let paths = |value: &str| match value {
        "none" => Vec::new(),
        _ => value.split_whitespace().map(expand).collect(),
    };
    // Expanded once the host is known: their tokens name it.
    let mut identity_files = Vec::new();
    for line in output.lines() {
        let Some((key, value)) = line.split_once(' ') else {
            continue;
        };
        let value = value.trim();
        match key {
            "hostname" => config.hostname = value.to_string(),
            "port" => config.port = value.parse().unwrap_or(22),
            "user" => config.user = value.to_string(),
            "identityfile" => identity_files.push(tilde(value, home)),
            "identityagent" => {
                config.identity_agent = match value {
                    "none" => None,
                    "SSH_AUTH_SOCK" => auth_sock.clone(),
                    path => Some(expand(path)),
                }
            }
            "identitiesonly" => config.identities_only = is_yes(value),
            "userknownhostsfile" => config.user_known_hosts = paths(value),
            "globalknownhostsfile" => config.global_known_hosts = paths(value),
            "stricthostkeychecking" => {
                config.strict_host_key_checking = match value {
                    "accept-new" | "false" | "no" | "off" => StrictHostKeyChecking::AcceptNew,
                    "true" | "yes" => StrictHostKeyChecking::Yes,
                    _ => StrictHostKeyChecking::Ask,
                }
            }
            "hashknownhosts" => config.hash_known_hosts = is_yes(value),
            "hostkeyalias" => config.host_key_alias = Some(value.to_string()),
            "proxyjump" if value != "none" => config.proxy_jump = Some(value.to_string()),
            "proxycommand" if value != "none" => config.proxy_command = Some(value.to_string()),
            "connecttimeout" => config.connect_timeout = seconds(value),
            "serveraliveinterval" => config.server_alive_interval = seconds(value),
            _ => {}
        }
    }
    config.identity_files = identity_files
        .iter()
        .map(|value| PathBuf::from(tokens::expand(value, alias, &config, local)))
        .collect();
    config
}

fn is_yes(value: &str) -> bool {
    matches!(value, "yes" | "true")
}

/// `none` and `0` mean "not set".
fn seconds(value: &str) -> Option<Duration> {
    value
        .parse::<u64>()
        .ok()
        .filter(|&s| s > 0)
        .map(Duration::from_secs)
}

/// `ssh -G` leaves `~` and `%d` in some paths for the client to expand.
fn expand_path(value: &str, home: &Path) -> PathBuf {
    PathBuf::from(tilde(&value.replace("%d", &home.to_string_lossy()), home))
}

/// A leading `~` as the home folder. `ssh` expands it before any `%`
/// token, so a `%%` that leaves `%d` stays as written.
fn tilde(value: &str, home: &Path) -> String {
    let home = home.to_string_lossy();
    match value.strip_prefix("~/") {
        Some(rest) => format!("{}/{rest}", home.trim_end_matches('/')),
        None if value == "~" => home.into_owned(),
        None => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OUTPUT: &str = "\
user edward
hostname example.com
port 2222
identityfile ~/.ssh/id_rsa
identityfile %d/.ssh/work
identityagent /home/edward/.1password/agent.sock
identitiesonly yes
userknownhostsfile /home/edward/.ssh/known_hosts /home/edward/.ssh/known_hosts2
globalknownhostsfile /etc/ssh/ssh_known_hosts /etc/ssh/ssh_known_hosts2
stricthostkeychecking accept-new
hashknownhosts yes
connecttimeout none
serveraliveinterval 30
proxyjump bastion
";

    fn parsed(output: &str) -> HostConfig {
        parse(output, "web", &local(), Some("/run/agent".into()))
    }

    fn local() -> Local {
        Local {
            home: "/home/edward".into(),
            user: "edward".into(),
            uid: 1000,
            host: "desk.lan".into(),
        }
    }

    #[test]
    fn reads_ssh_g_output() {
        let config = parsed(OUTPUT);
        assert_eq!(config.hostname, "example.com");
        assert_eq!(config.port, 2222);
        assert_eq!(config.user, "edward");
        assert_eq!(
            config.identity_files,
            [
                PathBuf::from("/home/edward/.ssh/id_rsa"),
                PathBuf::from("/home/edward/.ssh/work")
            ]
        );
        assert_eq!(
            config.identity_agent,
            Some("/home/edward/.1password/agent.sock".into())
        );
        assert!(config.identities_only);
        assert_eq!(config.user_known_hosts.len(), 2);
        assert_eq!(config.global_known_hosts.len(), 2);
        assert_eq!(
            config.strict_host_key_checking,
            StrictHostKeyChecking::AcceptNew
        );
        assert!(config.hash_known_hosts);
        assert_eq!(config.connect_timeout, None);
        assert_eq!(config.server_alive_interval, Some(Duration::from_secs(30)));
        assert_eq!(config.proxy_jump.as_deref(), Some("bastion"));
    }

    #[test]
    fn identity_files_have_their_tokens_expanded() {
        let config = parsed("hostname example.com\nuser bob\nidentityfile ~/.ssh/key_%n_%h_%r\n");
        assert_eq!(
            config.identity_files,
            [PathBuf::from("/home/edward/.ssh/key_web_example.com_bob")]
        );
    }

    #[test]
    fn known_hosts_none_means_no_file() {
        let config = parsed("userknownhostsfile none\nglobalknownhostsfile none\n");
        assert!(config.user_known_hosts.is_empty());
        assert!(config.global_known_hosts.is_empty());
    }

    #[test]
    fn agent_settings() {
        assert_eq!(
            parsed("hostname h\n").identity_agent,
            Some("/run/agent".into())
        );
        assert_eq!(parsed("identityagent none\n").identity_agent, None);
        assert_eq!(
            parsed("identityagent SSH_AUTH_SOCK\n").identity_agent,
            Some("/run/agent".into())
        );
    }

    #[test]
    fn strict_checking_values() {
        let strict =
            |v: &str| parsed(&format!("stricthostkeychecking {v}\n")).strict_host_key_checking;
        assert_eq!(strict("ask"), StrictHostKeyChecking::Ask);
        assert_eq!(strict("true"), StrictHostKeyChecking::Yes);
        assert_eq!(strict("false"), StrictHostKeyChecking::AcceptNew);
    }
}
