//! Finding the daemon's socket, and starting the daemon when nobody has.

use std::ffi::OsString;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use super::{Client, Role};
use crate::daemon::DAEMON_ARG;
use crate::engine::CoreOptions;
use crate::error::{Error, Result};

/// The socket's name in the runtime folder.
pub(crate) const SOCKET: &str = "burrowd.sock";

/// Held by the running daemon, so a second one gives up.
pub(crate) const LOCK: &str = "burrowd.lock";

/// The variables that point Burrow away from the user's real setup.
const OVERRIDES: [&str; 3] = ["BURROW_CONFIG_DIR", "BURROW_STATE_DIR", "BURROW_SSH_CONFIG"];

/// How long a client waits for a daemon it started.
const START_TIMEOUT: Duration = Duration::from_secs(10);

/// The folder holding the daemon's socket: `$BURROW_RUNTIME_DIR` when set,
/// otherwise `$XDG_RUNTIME_DIR/burrow`.
///
/// A run pointed at another config, state or OpenSSH config gets a folder
/// of its own below that, named after those settings. Otherwise it would
/// join the user's real daemon and quietly use the real setup.
pub fn runtime_dir() -> Result<PathBuf> {
    runtime_dir_from(|name| std::env::var_os(name))
}

fn runtime_dir_from(var: impl Fn(&str) -> Option<OsString>) -> Result<PathBuf> {
    if let Some(dir) = var("BURROW_RUNTIME_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let base = var("XDG_RUNTIME_DIR")
        .filter(|dir| !dir.is_empty())
        .ok_or_else(|| Error::invalid("runtime dir", "XDG_RUNTIME_DIR is not set"))?;
    let base = PathBuf::from(base).join("burrow");
    let mut hash = Sha256::new();
    let mut overridden = false;
    for name in OVERRIDES {
        if let Some(value) = var(name) {
            // The same folder however it was spelled, or from where.
            let value = std::path::absolute(&value).unwrap_or(PathBuf::from(value));
            hash.update(format!("{name}={}\0", value.display()));
            overridden = true;
        }
    }
    if !overridden {
        return Ok(base);
    }
    let hash = data_encoding::HEXLOWER.encode(&hash.finalize());
    Ok(base.join(format!("dev-{}", &hash[..12])))
}

/// Creates `dir` if needed and checks that only this user can reach it.
/// Commands sent to the socket can carry secrets, so a folder others can
/// enter could let them put their own socket in its place.
pub fn ensure_private_dir(dir: &Path) -> Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| Error::io(dir, e))?;
    let meta = std::fs::symlink_metadata(dir).map_err(|e| Error::io(dir, e))?;
    let reason = if !meta.is_dir() {
        "it is not a folder"
    } else if meta.uid() != super::own_uid() {
        "it belongs to another user"
    } else if meta.mode() & 0o077 != 0 {
        "other users can open it"
    } else {
        return Ok(());
    };
    Err(Error::PermissionDenied(format!(
        "{} is not private: {reason}",
        dir.display()
    )))
}

/// Connects to the daemon, starting one if none is running.
pub async fn connect(role: Role) -> Result<Client> {
    let dir = runtime_dir()?;
    ensure_private_dir(&dir)?;
    let socket = dir.join(SOCKET);
    match Client::connect(&socket, role).await {
        Err(e) if not_running(&e) => {}
        other => return other,
    }
    let (mut child, log) = start()?;
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        tokio::time::sleep(Duration::from_millis(50)).await;
        match Client::connect(&socket, role).await {
            Err(e) if not_running(&e) => {}
            other => return other,
        }
        // A second daemon started at the same time gives up quietly, so
        // an exit is only a failure once the deadline passes.
        let exited = child.try_wait().ok().flatten();
        if Instant::now() > deadline || exited.is_some_and(|status| !status.success()) {
            return Err(Error::Remote(format!(
                "burrowd did not start; see {}",
                log.display()
            )));
        }
    }
}

fn not_running(error: &Error) -> bool {
    use std::io::ErrorKind::{ConnectionRefused, NotFound};
    matches!(error, Error::Io { source, .. } if matches!(source.kind(), NotFound | ConnectionRefused))
}

/// Starts this program again as the daemon, so it is always the same build
/// as the client. Its output goes to `burrowd.log` in the state folder.
fn start() -> Result<(tokio::process::Child, PathBuf)> {
    let state_dir = CoreOptions::from_env()?.state_dir;
    std::fs::create_dir_all(&state_dir).map_err(|e| Error::io(&state_dir, e))?;
    let log = state_dir.join("burrowd.log");
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&log)
        .map_err(|e| Error::io(&log, e))?;
    let err = file.try_clone().map_err(|e| Error::io(&log, e))?;
    let exe = std::env::current_exe().map_err(|e| Error::io("current program", e))?;
    let child = tokio::process::Command::new(&exe)
        .arg(DAEMON_ARG)
        .stdin(Stdio::null())
        .stdout(file)
        .stderr(err)
        // Its own process group, so Ctrl+C on the client leaves it running.
        .process_group(0)
        .spawn()
        .map_err(|e| Error::io(&exe, e))?;
    Ok((child, log))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(vars: &[(&str, &str)]) -> PathBuf {
        runtime_dir_from(|name| {
            vars.iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| OsString::from(v))
        })
        .unwrap()
    }

    #[test]
    fn a_run_pointed_elsewhere_gets_its_own_daemon() {
        let real = dir(&[("XDG_RUNTIME_DIR", "/run/user/1000")]);
        assert_eq!(real, Path::new("/run/user/1000/burrow"));

        let dev = dir(&[
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ("BURROW_CONFIG_DIR", "/tmp/a"),
        ]);
        assert_eq!(dev.parent(), Some(real.as_path()));
        assert!(
            dev.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("dev-")
        );
        let other = dir(&[
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ("BURROW_CONFIG_DIR", "/tmp/b"),
        ]);
        assert_ne!(dev, other);

        let chosen = dir(&[
            ("BURROW_RUNTIME_DIR", "/x"),
            ("BURROW_CONFIG_DIR", "/tmp/a"),
        ]);
        assert_eq!(chosen, Path::new("/x"));
    }

    #[test]
    fn the_socket_folder_must_be_private() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::TempDir::new().unwrap();
        let private = temp.path().join("run");
        ensure_private_dir(&private).unwrap();
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(ensure_private_dir(&private).is_err());

        let link = temp.path().join("link");
        std::os::unix::fs::symlink(temp.path(), &link).unwrap();
        assert!(ensure_private_dir(&link).is_err());
    }
}
