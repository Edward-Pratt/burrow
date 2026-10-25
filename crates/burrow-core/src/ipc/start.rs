//! Finding the daemon's socket, and starting the daemon when nobody has.

use std::ffi::OsString;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use super::{BUILD, Client, Role};
use crate::api::{Command, Response};
use crate::daemon::DAEMON_ARG;
use crate::engine::Paths;
use crate::error::{Error, Result};

/// The socket's name in the runtime folder.
pub(crate) const SOCKET: &str = "burrowd.sock";

/// Held by the running daemon, so a second one gives up.
pub(crate) const LOCK: &str = "burrowd.lock";

/// The variables that point Burrow away from the user's real setup.
const OVERRIDES: [&str; 3] = ["BURROW_CONFIG_DIR", "BURROW_STATE_DIR", "BURROW_SSH_CONFIG"];

/// How long a client waits for a daemon it started.
const START_TIMEOUT: Duration = Duration::from_secs(10);

/// How many times a starting daemon tries for the lock, and how long it
/// waits between tries: about a second in all.
const LOCK_TRIES: u32 = 50;
const LOCK_WAIT: Duration = Duration::from_millis(20);

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
///
/// A daemon from another build, still running after an upgrade, is
/// replaced when nothing runs in it. Otherwise the client gets it anyway,
/// and can tell from [`Client::info`] that it should say so.
pub async fn connect(role: Role) -> Result<Client> {
    let dir = runtime_dir()?;
    ensure_private_dir(&dir)?;
    let socket = dir.join(SOCKET);
    let client = connect_or_start(&socket, role).await?;
    if client.info().build == BUILD {
        return Ok(client);
    }
    let idle = matches!(
        client.execute(Command::ListSessions).await,
        Ok(Response::Sessions { sessions }) if sessions.is_empty()
    );
    if !idle {
        return Ok(client);
    }
    drop(client);
    stop(&dir).await?;
    connect_or_start(&socket, role).await
}

async fn connect_or_start(socket: &Path, role: Role) -> Result<Client> {
    match Client::connect(socket, role).await {
        Err(e) if not_running(&e) => {}
        other => return other,
    }
    let (mut child, log) = start()?;
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        tokio::time::sleep(Duration::from_millis(50)).await;
        match Client::connect(socket, role).await {
            Err(e) if not_running(&e) => {}
            other => return other,
        }
        // A second daemon started at the same time gives up quietly, so
        // an exit is only a failure once the deadline passes.
        let exited = child.try_wait().ok().flatten();
        if exited.is_some_and(|status| !status.success()) {
            return Err(did_not_start(&log, true));
        }
        if Instant::now() > deadline {
            return Err(did_not_start(&log, false));
        }
    }
}

/// Stops the daemon serving `dir` and waits until it has. `false` when none
/// was running.
///
/// The daemon holds its lock file while it runs, so the lock, not the pid
/// in it, says whether one does: a daemon that was killed leaves its pid
/// behind, and that number may belong to another process by now.
pub async fn stop(dir: &Path) -> Result<bool> {
    let path = dir.join(LOCK);
    let lock = match std::fs::File::open(&path) {
        Ok(lock) => lock,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(Error::io(&path, e)),
    };
    let free = |lock: &std::fs::File| {
        // Let go at once: a daemon starting now must still get the lock.
        lock.try_lock_shared().is_ok() && lock.unlock().is_ok()
    };
    if free(&lock) {
        return Ok(false);
    }
    let pid =
        holder(&path).ok_or_else(|| Error::invalid("lock", "burrowd is starting; try again"))?;
    // The pid is written just after the lock is taken, so for a moment it
    // can be a dead daemon's. Only a Burrow program is sent the signal.
    let command = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    if !String::from_utf8_lossy(&command).contains("burrow") {
        return Err(Error::invalid("lock", "burrowd is starting; try again"));
    }
    // SAFETY: kill only sends a signal.
    if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
        return Err(Error::io("burrowd", std::io::Error::last_os_error()));
    }
    let deadline = Instant::now() + START_TIMEOUT;
    while Instant::now() < deadline {
        // An open window starts a new daemon as soon as this one goes, and
        // it can take the lock between two looks. Its pid in the file
        // means this one stopped. A zombie pid would still pass a check
        // in /proc, though its lock is already gone.
        if free(&lock) || holder(&path).is_some_and(|holder| holder != pid) {
            return Ok(true);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Err(Error::Remote("burrowd did not stop".to_string()))
}

/// Takes the daemon's lock on `file`. `false` when another daemon holds it.
///
/// [`stop`] holds the lock shared for an instant to see whether a daemon
/// runs. A daemon starting in that instant would take that for another
/// daemon and exit, and the client that started it would wait out
/// [`START_TIMEOUT`], so it keeps trying for a while first.
pub(crate) async fn take_lock(file: &std::fs::File) -> bool {
    for _ in 1..LOCK_TRIES {
        if file.try_lock().is_ok() {
            return true;
        }
        tokio::time::sleep(LOCK_WAIT).await;
    }
    file.try_lock().is_ok()
}

/// The pid written in the lock file, or `None` while it is empty: a daemon
/// empties it just before it lets go, and writes it just after it takes
/// the lock.
fn holder(path: &Path) -> Option<libc::pid_t> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Why the daemon the client started isn't answering. One that `stopped`
/// printed its error last, so the log's last line says why; one still
/// running has only logged progress, which would mislead.
fn did_not_start(log: &Path, stopped: bool) -> Error {
    let line = if stopped { last_line(log) } else { None };
    Error::Remote(match line {
        Some(line) => format!("burrowd did not start: {line} (see {})", log.display()),
        None if stopped => format!("burrowd did not start; see {}", log.display()),
        None => format!(
            "burrowd did not answer within {} seconds; see {}",
            START_TIMEOUT.as_secs(),
            log.display()
        ),
    })
}

/// The last line of the log, without the program name the daemon prefixes
/// to its error.
fn last_line(log: &Path) -> Option<String> {
    let text = std::fs::read_to_string(log).ok()?;
    let line = text
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())?
        .trim();
    Some(line.strip_prefix("burrowd: ").unwrap_or(line).to_string())
}

fn not_running(error: &Error) -> bool {
    use std::io::ErrorKind::{ConnectionRefused, NotFound};
    matches!(error, Error::Io { source, .. } if matches!(source.kind(), NotFound | ConnectionRefused))
}

/// Starts this program again as the daemon, so it is always the same build
/// as the client. Its output goes to `burrowd.log` in the state folder.
fn start() -> Result<(tokio::process::Child, PathBuf)> {
    let state_dir = Paths::from_env()?.state_dir;
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
    fn a_stopped_daemon_is_quoted_and_a_stalled_one_is_not() {
        let temp = tempfile::TempDir::new().unwrap();
        let log = temp.path().join("burrowd.log");
        std::fs::write(&log, "INFO serving\nburrowd: secret store: gone\n\n").unwrap();
        let stopped = did_not_start(&log, true).to_string();
        assert!(stopped.starts_with("burrowd did not start: secret store: gone (see "));
        let stalled = did_not_start(&log, false).to_string();
        assert!(stalled.starts_with("burrowd did not answer within 10 seconds; see "));
        std::fs::write(&log, "").unwrap();
        let silent = did_not_start(&log, true).to_string();
        assert!(silent.starts_with("burrowd did not start; see "));
    }

    /// An open window starts a new daemon as soon as the old one goes, and
    /// the new one can take the lock before `stop` sees it free.
    #[tokio::test]
    async fn a_daemon_replaced_at_once_has_still_stopped() {
        use std::io::Write;
        use std::os::unix::process::CommandExt;
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join(LOCK);
        // Locks belong to each opened file, so `stop` opening the file
        // again finds this one taken even from the same process.
        let mut lock = std::fs::File::create(&path).unwrap();
        lock.try_lock().unwrap();
        let mut old = std::process::Command::new("sleep")
            .arg0("burrowd-test")
            .arg("60")
            .spawn()
            .unwrap();
        writeln!(lock, "{}", old.id()).unwrap();
        // The new daemon's pid replaces the old one's without the lock
        // ever coming free.
        let replace = std::thread::spawn(move || {
            use std::os::unix::fs::FileExt;
            old.wait().unwrap();
            lock.set_len(0).unwrap();
            lock.write_all_at(format!("{}\n", std::process::id()).as_bytes(), 0)
                .unwrap();
            lock
        });
        let started = Instant::now();
        assert!(stop(temp.path()).await.unwrap());
        assert!(started.elapsed() < Duration::from_secs(3));
        let lock = replace.join().unwrap();
        assert_eq!(holder(&path), Some(std::process::id() as libc::pid_t));
        lock.set_len(0).unwrap();
        assert_eq!(holder(&path), None);
    }

    #[tokio::test]
    async fn a_daemon_starting_while_stop_looks_still_takes_the_lock() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join(LOCK);
        // Held shared as `stop` holds it to look, if longer.
        let looking = std::fs::File::create(&path).unwrap();
        looking.try_lock_shared().unwrap();
        let done = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            looking.unlock().unwrap();
        });
        let daemon = std::fs::File::open(&path).unwrap();
        assert!(take_lock(&daemon).await);
        done.join().unwrap();

        // A lock another daemon holds is given up on, after about a second.
        let second = std::fs::File::open(&path).unwrap();
        let started = Instant::now();
        assert!(!take_lock(&second).await);
        assert!(started.elapsed() >= LOCK_WAIT * (LOCK_TRIES - 1));
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
