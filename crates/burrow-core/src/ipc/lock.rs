//! The daemon's lock file: holding it is what makes a daemon the running
//! one, so stopping it and starting one both go through it.

use std::path::Path;
use std::time::{Duration, Instant};

use super::start::START_TIMEOUT;
use crate::error::{Error, Result};

/// Held by the running daemon, so a second one gives up.
pub(crate) const LOCK: &str = "burrowd.lock";

/// How many times a starting daemon tries for the lock, and how long it
/// waits between tries: about a second in all.
const LOCK_TRIES: u32 = 50;
const LOCK_WAIT: Duration = Duration::from_millis(20);

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

#[cfg(test)]
mod tests {
    use super::*;

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
        // `spawn` can return while the child is still inside `exec`, with
        // its arguments not yet in /proc; `stop` would then take it for a
        // daemon still starting. A real daemon is long past that.
        let cmdline = format!("/proc/{}/cmdline", old.id());
        while std::fs::read(&cmdline).unwrap().is_empty() {
            std::thread::sleep(Duration::from_millis(1));
        }
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
}
