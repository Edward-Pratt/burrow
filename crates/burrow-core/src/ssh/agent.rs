//! Putting burrow-agent on a server, for monitors.
//!
//! The agent lives in `~/.local/share/burrow/burrow-agent-<version>`. Each
//! time a monitor starts, the file there is checked against the local copy
//! by hash, so a half-written or changed one is replaced rather than run.
//!
//! The user is asked once per host before the first upload. That consent
//! lives in the state folder, pinned to the host node the way a grant is,
//! because a node file must never be able to put a program on a server.
//! Later uploads, such as a new version, go ahead without asking.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use burrow_agent::VERSION;
use data_encoding::HEXLOWER;
use russh::ChannelMsg;
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::FileType;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::files::{self, join};
use super::{Connection, Prompter};
use crate::api::{Node, NodeId, PromptAnswer};
use crate::engine::Core;
use crate::error::Result;
use crate::pins::{Lookup, Pins};

/// Where packages install the agent builds, as `burrow-agent-<arch>`.
const INSTALLED: &str = "/usr/lib/burrow";

/// The servers the agent is built for, by their Rust target's name.
pub(crate) const ARCHES: [&str; 2] = ["x86_64", "aarch64"];

/// How long the checks before an upload may take.
const CHECK_TIMEOUT: Duration = Duration::from_secs(30);

/// The largest agent Burrow reads back to check.
const MAX_SIZE: u64 = 16 << 20;

/// Which hosts the user let Burrow put the agent on, and where the agent
/// builds are.
pub(crate) struct Agents {
    consents: Pins,
    /// Set by tests only: the local builds, and the server folder.
    local_dir: Mutex<Option<PathBuf>>,
    remote_dir: Mutex<Option<String>>,
}

impl Agents {
    /// Reads the state file. A broken one is treated as empty, so Burrow
    /// asks again.
    pub fn load(path: PathBuf) -> Agents {
        Agents {
            consents: Pins::load(path),
            local_dir: Mutex::default(),
            remote_dir: Mutex::default(),
        }
    }

    fn consented(&self, lookup: &impl Lookup, node: &Node) -> bool {
        self.consents.has(lookup, node)
    }

    fn consent(&self, lookup: &impl Lookup, node: &Node) -> Result<()> {
        self.consents.set(lookup, node, true)
    }

    /// Drops consents for hosts that no longer exist.
    pub fn forget(&self, nodes: &[NodeId]) {
        self.consents.forget(nodes);
    }

    /// For tests: read the agent builds from `local`, and put the agent in
    /// `remote` on servers instead of `~/.local/share/burrow`.
    pub fn set_dirs(&self, local: PathBuf, remote: String) {
        *self.local_dir.lock().unwrap() = Some(local);
        *self.remote_dir.lock().unwrap() = Some(remote);
    }

    /// The agent built for `arch`, read from the first place that has one.
    async fn local_binary(&self, arch: &str) -> std::result::Result<Vec<u8>, String> {
        let dir = self.local_dir.lock().unwrap().clone();
        let path = find_build(arch, dir).await?;
        tokio::fs::read(&path)
            .await
            .map_err(|e| format!("{}: {e}", path.display()))
    }
}

/// Where the agent built for `arch` may be, first choice first: `dir` or
/// `$BURROW_AGENT_DIR`, then `lib/burrow` beside the folder Burrow runs
/// from (so `~/.local/bin/burrowd` finds `~/.local/lib/burrow`), then where
/// packages install it, then the cargo target folder Burrow was built in.
pub(crate) fn candidates(arch: &str, dir: Option<PathBuf>) -> Vec<PathBuf> {
    let name = format!("burrow-agent-{arch}");
    let mut candidates = Vec::new();
    if let Some(dir) = dir.or_else(|| std::env::var_os("BURROW_AGENT_DIR").map(PathBuf::from)) {
        candidates.push(dir.join(&name));
    }
    let prefix = std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.parent()?.to_path_buf()));
    if let Some(prefix) = &prefix {
        candidates.push(prefix.join("lib/burrow").join(&name));
    }
    let installed = Path::new(INSTALLED).join(&name);
    if !candidates.contains(&installed) {
        candidates.push(installed);
    }
    if let Some(target) = &prefix {
        candidates.push(target.join(format!("{arch}-unknown-linux-musl/agent/burrow-agent")));
    }
    candidates
}

/// The first of [`candidates`] that exists.
pub(crate) async fn find_build(
    arch: &str,
    dir: Option<PathBuf>,
) -> std::result::Result<PathBuf, String> {
    for candidate in candidates(arch, dir) {
        match tokio::fs::try_exists(&candidate).await {
            Ok(true) => return Ok(candidate),
            Ok(false) => {}
            Err(e) => return Err(format!("{}: {e}", candidate.display())),
        }
    }
    Err(format!(
        "this Burrow has no burrow-agent for {arch} servers. If you installed it with \
         dist/install.sh, run `burrow-update` in a terminal. Otherwise build one with \
         `cargo build -p burrow-agent --profile agent --target {arch}-unknown-linux-musl`"
    ))
}

/// Makes sure the current agent is on the server, and returns its path
/// there. Without `install`, a missing or different agent is an error
/// rather than an upload: the MCP bridge may run the agent the user put
/// there, but never put one there itself.
pub(super) async fn ensure(
    core: &Core,
    node: &Node,
    connection: &Connection,
    prompter: &Prompter,
    install: bool,
) -> std::result::Result<String, String> {
    let checks = async {
        let uname = capture(connection, "uname -m").await?;
        let arch = arch(&uname).ok_or_else(|| {
            format!(
                "burrow-agent doesn't run on {} servers",
                printable(uname.trim())
            )
        })?;
        let binary = core.agents().local_binary(arch).await?;
        let sftp = files::open_sftp(connection).await?;
        let remote_dir = core.agents().remote_dir.lock().unwrap().clone();
        let dir = match remote_dir {
            Some(dir) => {
                make_dir(&sftp, &dir).await?;
                dir
            }
            None => {
                let home = sftp.canonicalize(".").await.map_err(text)?;
                let mut dir = home;
                for part in [".local", "share", "burrow"] {
                    dir = join(&dir, part);
                    make_dir(&sftp, &dir).await?;
                }
                dir
            }
        };
        // The path goes into a shell command and in front of the user.
        if dir.chars().any(char::is_control) {
            return Err(format!(
                "the server's home folder, {:?}, has control characters",
                printable(&dir)
            ));
        }
        let path = join(&dir, &format!("burrow-agent-{VERSION}"));
        let found = remote_hash(&sftp, &path).await?;
        Ok::<_, String>((binary, sftp, path, found))
    };
    let (binary, sftp, path, found) = tokio::time::timeout(CHECK_TIMEOUT, checks)
        .await
        .map_err(|_| "the server took too long to check for burrow-agent".to_string())??;
    let hash = HEXLOWER.encode(&Sha256::digest(&binary));
    if found.as_deref() == Some(hash.as_str()) {
        return Ok(path);
    }
    if !install {
        return Err(format!(
            "burrow-agent {VERSION} isn't on {}. Open a monitor on it in Burrow to install it",
            node.name
        ));
    }

    let lookup = |id| core.node(id).ok();
    if !core.agents().consented(&lookup, node) {
        let message = format!(
            "Burrow will put burrow-agent {VERSION} at {path} on {} and run it there while \
             a monitor is open. It runs as your user, only reads system stats and services, \
             and never listens on the network.\n\nSHA-256: {hash}",
            node.name
        );
        let answer = prompter
            .confirm("Install burrow-agent?", &message, "Install")
            .await;
        if answer != PromptAnswer::Confirm {
            return Err("burrow-agent was not installed".to_string());
        }
        core.agents()
            .consent(&lookup, node)
            .map_err(|e| e.to_string())?;
    }

    // A file there that didn't match goes first, so the new one gets the
    // agent's mode rather than keeping that file's.
    if found.is_some() {
        sftp.remove_file(&path).await.map_err(text)?;
    }
    let tag = format!("a{}", uuid::Uuid::new_v4().simple());
    files::put(&sftp, &path, &tag, Some(0o700), None, async |to| {
        to.write_all(&binary).await.map_err(text)
    })
    .await?;
    if remote_hash(&sftp, &path).await?.as_deref() != Some(hash.as_str()) {
        return Err(format!("{path} doesn't match the burrow-agent Burrow sent"));
    }
    remove_old(&sftp, &path).await;
    Ok(path)
}

/// The architecture name of the agent builds, from `uname -m`.
fn arch(uname: &str) -> Option<&'static str> {
    match uname.trim() {
        "x86_64" | "amd64" => Some("x86_64"),
        "aarch64" | "arm64" => Some("aarch64"),
        _ => None,
    }
}

/// Creates a folder that's missing. One that's there must be a real
/// folder: Burrow never writes through a symlink.
async fn make_dir(sftp: &SftpSession, dir: &str) -> std::result::Result<(), String> {
    match sftp.symlink_metadata(dir).await {
        Ok(meta) if meta.file_type() == FileType::Dir => Ok(()),
        Ok(meta) if meta.file_type() == FileType::Symlink => Err(format!(
            "{dir} is a symlink, which Burrow won't write through"
        )),
        Ok(_) => Err(format!("{dir} is in the way of a folder")),
        Err(_) => sftp.create_dir(dir).await.map_err(text),
    }
}

/// The SHA-256 of the regular file at `path`, or `None` if nothing is
/// there.
async fn remote_hash(
    sftp: &SftpSession,
    path: &str,
) -> std::result::Result<Option<String>, String> {
    let meta = match sftp.symlink_metadata(path).await {
        Ok(meta) => meta,
        Err(_) => return Ok(None),
    };
    match meta.file_type() {
        FileType::File => {}
        FileType::Symlink => {
            return Err(format!("{path} is a symlink, which Burrow won't run"));
        }
        _ => return Err(format!("{path} is in the way of burrow-agent")),
    }
    let file = sftp.open(path).await.map_err(text)?;
    let mut data = Vec::new();
    file.take(MAX_SIZE)
        .read_to_end(&mut data)
        .await
        .map_err(text)?;
    Ok(Some(HEXLOWER.encode(&Sha256::digest(&data))))
}

/// Removes other versions next to `current`. It's tidying, so failures
/// are ignored.
async fn remove_old(sftp: &SftpSession, current: &str) {
    let Some((dir, name)) = current.rsplit_once('/') else {
        return;
    };
    let Ok(entries) = sftp.read_dir(dir).await else {
        return;
    };
    for entry in entries {
        let other = entry.file_name();
        if other != name
            && other.starts_with("burrow-agent-")
            && !other.contains('/')
            && entry.file_type() == FileType::File
        {
            let _ = sftp.remove_file(join(dir, &other)).await;
        }
    }
}

/// Runs a short command and returns its output, which must be small.
async fn capture(connection: &Connection, command: &str) -> std::result::Result<String, String> {
    let mut channel = connection
        .handle()
        .channel_open_session()
        .await
        .map_err(text)?;
    channel.exec(true, command).await.map_err(text)?;
    let (mut output, mut status) = (Vec::new(), None);
    loop {
        match channel.wait().await {
            Some(ChannelMsg::Data { data }) if output.len() < 4096 => {
                output.extend_from_slice(&data);
            }
            Some(ChannelMsg::ExitStatus { exit_status }) => status = Some(exit_status),
            Some(ChannelMsg::Close) | None => break,
            Some(_) => {}
        }
    }
    if status != Some(0) {
        return Err(format!("`{command}` failed on the server"));
    }
    Ok(String::from_utf8_lossy(&output).into_owned())
}

/// `path` quoted for a POSIX shell.
pub(super) fn quote(path: &str) -> String {
    format!("'{}'", path.replace('\'', r"'\''"))
}

/// `text` without control characters, which a server could use to mess up
/// what Burrow shows.
pub(crate) fn printable(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

fn text(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn builds_are_looked_for_in_order_and_named_when_missing() {
        let dir = tempfile::TempDir::new().unwrap();
        let own = Some(dir.path().to_path_buf());
        let looked = candidates("riscv64", own.clone());
        assert_eq!(looked[0], dir.path().join("burrow-agent-riscv64"));
        let exe = std::env::current_exe().unwrap();
        let prefix = exe.parent().unwrap().parent().unwrap();
        assert_eq!(looked[1], prefix.join("lib/burrow/burrow-agent-riscv64"));
        assert_eq!(looked[2], Path::new(INSTALLED).join("burrow-agent-riscv64"));
        let error = find_build("riscv64", own.clone()).await.unwrap_err();
        assert!(
            error.contains("--target riscv64-unknown-linux-musl"),
            "{error}"
        );
        std::fs::write(&looked[0], b"").unwrap();
        assert_eq!(find_build("riscv64", own).await.unwrap(), looked[0]);
    }

    #[test]
    fn arch_names_the_builds() {
        assert_eq!(arch("x86_64\n"), Some("x86_64"));
        assert_eq!(arch("arm64"), Some("aarch64"));
        assert_eq!(arch("riscv64"), None);
    }

    #[test]
    fn quote_survives_quotes() {
        assert_eq!(quote("/home/o'neil/x"), r"'/home/o'\''neil/x'");
    }

    #[test]
    fn printable_drops_control_characters() {
        assert_eq!(printable("a\x1b[31mb\n"), "a[31mb");
    }
}
