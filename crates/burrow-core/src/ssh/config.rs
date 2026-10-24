//! Host settings from the user's OpenSSH config.
//!
//! `ssh -G <alias>` prints the settings OpenSSH itself would use, after
//! `Include`, `Host` patterns, `Match` blocks and defaults. Asking it, rather
//! than parsing `~/.ssh/config` ourselves, means Burrow connects exactly as
//! `ssh <alias>` would.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

/// How long `ssh -G` may take. It only reads files, unless the config has
/// `Match exec`, which `ssh` would run too.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(10);

/// What to do with a host key that isn't in any `known_hosts` file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StrictHostKeyChecking {
    /// Ask the user (OpenSSH's default).
    Ask,
    /// Add it without asking, but still refuse a changed key.
    AcceptNew,
    /// Refuse it.
    Yes,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HostConfig {
    pub hostname: String,
    pub port: u16,
    pub user: String,
    pub identity_files: Vec<PathBuf>,
    /// The agent socket, or `None` when agent use is off.
    pub identity_agent: Option<PathBuf>,
    pub identities_only: bool,
    /// Checked, and the first one is where new keys are added.
    pub user_known_hosts: Vec<PathBuf>,
    /// Checked only.
    pub global_known_hosts: Vec<PathBuf>,
    pub strict_host_key_checking: StrictHostKeyChecking,
    pub hash_known_hosts: bool,
    /// Look up host keys under this name instead of `hostname`.
    pub host_key_alias: Option<String>,
    pub proxy_jump: Option<String>,
    pub proxy_command: Option<String>,
    pub connect_timeout: Option<Duration>,
    pub server_alive_interval: Option<Duration>,
}

impl HostConfig {
    /// The name host keys are recorded under.
    pub fn host_key_name(&self) -> &str {
        self.host_key_alias.as_deref().unwrap_or(&self.hostname)
    }
}

/// Rejects anything `ssh` could read as an option rather than a host name.
///
/// Node files can come from a shared folder or a `git pull`. An alias such
/// as `-F/tmp/evil` would make `ssh -G` load that file, and its `Match exec`
/// lines would run.
pub fn check_alias(alias: &str) -> Result<(), String> {
    if alias.is_empty() {
        return Err("the host alias is empty".into());
    }
    if alias.starts_with('-') {
        return Err(format!("host alias {alias:?} must not start with '-'"));
    }
    if alias.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(format!(
            "host alias {alias:?} must not contain spaces or control characters"
        ));
    }
    Ok(())
}

/// Resolves `alias` with `ssh -G`. `config_file` replaces `~/.ssh/config`,
/// as `ssh -F` does.
pub async fn resolve(alias: &str, config_file: Option<&Path>) -> Result<HostConfig, String> {
    check_alias(alias)?;
    let mut command = Command::new("ssh");
    command.arg("-G");
    if let Some(file) = config_file {
        command.arg("-F").arg(file);
    }
    command
        .arg("--")
        .arg(alias)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let output = tokio::time::timeout(RESOLVE_TIMEOUT, command.output())
        .await
        .map_err(|_| "reading the OpenSSH config took too long".to_string())?
        .map_err(|e| format!("could not run ssh to read the OpenSSH config: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let reason = stderr.lines().last().unwrap_or("no error message").trim();
        return Err(format!("could not read the OpenSSH config: {reason}"));
    }
    let home = std::env::home_dir().unwrap_or_default();
    Ok(parse(
        &String::from_utf8_lossy(&output.stdout),
        &home,
        std::env::var_os("SSH_AUTH_SOCK").map(PathBuf::from),
    ))
}

/// Parses `ssh -G` output: one `keyword value` pair per line, keywords in
/// lowercase, list settings repeated or space-separated.
fn parse(output: &str, home: &Path, auth_sock: Option<PathBuf>) -> HostConfig {
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
    for line in output.lines() {
        let Some((key, value)) = line.split_once(' ') else {
            continue;
        };
        let value = value.trim();
        match key {
            "hostname" => config.hostname = value.to_string(),
            "port" => config.port = value.parse().unwrap_or(22),
            "user" => config.user = value.to_string(),
            "identityfile" => config.identity_files.push(expand(value)),
            "identityagent" => {
                config.identity_agent = match value {
                    "none" => None,
                    "SSH_AUTH_SOCK" => auth_sock.clone(),
                    path => Some(expand(path)),
                }
            }
            "identitiesonly" => config.identities_only = is_yes(value),
            "userknownhostsfile" => {
                config.user_known_hosts = value.split_whitespace().map(expand).collect()
            }
            "globalknownhostsfile" => {
                config.global_known_hosts = value.split_whitespace().map(expand).collect()
            }
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
    let home = home.to_string_lossy();
    let value = value.replace("%d", &home);
    match value.strip_prefix("~/") {
        Some(rest) => Path::new(home.as_ref()).join(rest),
        None if value == "~" => PathBuf::from(home.as_ref()),
        None => PathBuf::from(value),
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
        parse(output, Path::new("/home/edward"), Some("/run/agent".into()))
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

    #[test]
    fn aliases_that_look_like_options_are_refused() {
        assert!(check_alias("prod-web").is_ok());
        assert!(check_alias("user@host.example").is_ok());
        assert!(check_alias("").is_err());
        assert!(check_alias("-F/tmp/evil").is_err());
        assert!(check_alias("-oProxyCommand=touch /tmp/x").is_err());
        assert!(check_alias("web extra").is_err());
        assert!(check_alias("web\n-F").is_err());
    }
}
