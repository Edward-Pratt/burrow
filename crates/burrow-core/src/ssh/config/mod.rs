//! Host settings from the user's OpenSSH config.
//!
//! `ssh -G <alias>` prints the settings OpenSSH itself would use, after
//! `Include`, `Host` patterns, `Match` blocks and defaults. Asking it, rather
//! than parsing `~/.ssh/config` ourselves, means Burrow connects exactly as
//! `ssh <alias>` would.

mod parse;
mod trace;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

pub(crate) use trace::{Applied, Trace, trace};

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
    let output = ssh_g(alias, config_file, false).await?;
    let home = std::env::home_dir().unwrap_or_default();
    Ok(parse::parse(
        &String::from_utf8_lossy(&output.stdout),
        &home,
        std::env::var_os("SSH_AUTH_SOCK").map(PathBuf::from),
    ))
}

async fn ssh_g(
    alias: &str,
    config_file: Option<&Path>,
    verbose: bool,
) -> Result<std::process::Output, String> {
    check_alias(alias)?;
    let mut command = Command::new("ssh");
    command.arg("-G");
    if verbose {
        command.arg("-v");
    }
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
        let reason = stderr
            .lines()
            .rfind(|l| !l.starts_with("debug"))
            .unwrap_or("no error message")
            .trim();
        return Err(format!("could not read the OpenSSH config: {reason}"));
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

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
