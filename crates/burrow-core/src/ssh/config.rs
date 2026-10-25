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
    let output = ssh_g(alias, config_file, false).await?;
    let home = std::env::home_dir().unwrap_or_default();
    Ok(parse(
        &String::from_utf8_lossy(&output.stdout),
        &home,
        std::env::var_os("SSH_AUTH_SOCK").map(PathBuf::from),
    ))
}

/// What `ssh -v -G` reports about reading the config for one alias.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Trace {
    /// Every file it read, in order, including ones whose options did not
    /// apply because their `Include` sat inside a `Host` block.
    pub read: Vec<PathBuf>,
    /// Every `Host` pattern whose options applied. `Match` blocks are not
    /// reported at this level.
    pub applied: Vec<Applied>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Applied {
    pub file: PathBuf,
    pub line: u32,
    pub pattern: String,
}

impl Trace {
    /// The `Host` line that names `alias` itself. A wildcard such as `*`
    /// or `*.corp` doesn't count.
    pub fn naming(&self, alias: &str) -> Option<&Applied> {
        self.applied.iter().find(|a| a.pattern == alias)
    }
}

/// Runs `ssh -v -G` for `alias` and reads which files and `Host` lines it
/// used. This is OpenSSH's debug output rather than a stable format, so
/// output with no trace in it at all is an error, never "nothing matched".
pub(crate) async fn trace(alias: &str, config_file: Option<&Path>) -> Result<Trace, String> {
    let output = ssh_g(alias, config_file, true).await?;
    parse_trace(&String::from_utf8_lossy(&output.stderr)).ok_or_else(|| {
        format!("ssh -v printed nothing about reading the OpenSSH config for {alias}")
    })
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

/// `None` when no file was read, which means the output isn't what this
/// expects.
fn parse_trace(stderr: &str) -> Option<Trace> {
    let mut trace = Trace::default();
    for line in stderr.lines() {
        let Some(line) = line.strip_prefix("debug1: ") else {
            continue;
        };
        if let Some(file) = line.strip_prefix("Reading configuration data ") {
            trace.read.push(PathBuf::from(file));
        } else if let Some((place, pattern)) = line.split_once(": Applying options for ")
            && let Some((file, number)) = place.rsplit_once(" line ")
            && let Ok(line) = number.parse()
        {
            trace.applied.push(Applied {
                file: PathBuf::from(file),
                line,
                pattern: pattern.to_string(),
            });
        }
    }
    (!trace.read.is_empty()).then_some(trace)
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
    // `none` means no file at all, not a file named "none".
    let paths = |value: &str| match value {
        "none" => Vec::new(),
        _ => value.split_whitespace().map(expand).collect(),
    };
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

    #[test]
    fn traces_name_the_files_read_and_the_lines_applied() {
        let stderr = "\
debug1: OpenSSH_10.6p1, OpenSSL 3.6.5 29 Sep 2026
debug1: Reading configuration data /home/u/.ssh/config
debug1: Reading configuration data /home/u/.ssh/config.d/burrow.conf
debug1: /home/u/.ssh/config.d/burrow.conf line 4: Applying options for web
debug1: /home/u/.ssh/config line 2: Applying options for *.corp
debug1: /home/u/.ssh/config line 9: Applying options for *
Pseudo-terminal will not be allocated because stdin is not a terminal.
";
        let trace = parse_trace(stderr).unwrap();
        assert_eq!(
            trace.read,
            [
                PathBuf::from("/home/u/.ssh/config"),
                PathBuf::from("/home/u/.ssh/config.d/burrow.conf")
            ]
        );
        assert_eq!(trace.applied.len(), 3);
        let web = trace.naming("web").unwrap();
        assert_eq!(web.file, Path::new("/home/u/.ssh/config.d/burrow.conf"));
        assert_eq!(web.line, 4);
        // Wildcards apply to many hosts, so they don't make an alias taken.
        assert!(trace.naming("db.corp").is_none());
        assert!(trace.naming("*").is_some_and(|a| a.line == 9));
    }

    #[test]
    fn a_file_name_with_line_in_it_still_parses() {
        let stderr = "debug1: Reading configuration data /a line b/config\n\
                      debug1: /a line b/config line 3: Applying options for web\n";
        let web = parse_trace(stderr).unwrap().naming("web").cloned().unwrap();
        assert_eq!(web.file, Path::new("/a line b/config"));
        assert_eq!(web.line, 3);
    }

    #[test]
    fn output_without_a_trace_is_refused() {
        assert_eq!(parse_trace(""), None);
        assert_eq!(parse_trace("debug1: OpenSSH_10.6p1\n"), None);
        // An applied line alone, with no file read, isn't trusted either.
        assert_eq!(
            parse_trace("debug1: /c line 1: Applying options for web\n"),
            None
        );
    }

    /// Pins the debug format to the installed OpenSSH, so a change there
    /// fails here instead of making every alias look free.
    #[tokio::test]
    async fn trace_reads_real_ssh_output() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = dir.path().join("config");
        std::fs::write(&config, "Host other\n  Port 2\nHost web db\n  Port 3\n").unwrap();
        let trace = trace("web", Some(&config)).await.unwrap();
        assert_eq!(trace.read, std::slice::from_ref(&config));
        let web = trace.naming("web").unwrap();
        assert_eq!((web.file.as_path(), web.line), (config.as_path(), 3));
        assert!(trace.naming("other").is_none());
    }
}
