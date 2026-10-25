//! burrowd: the core as a process of its own, so tunnels keep running with
//! the window closed and every client shares one core.
//!
//! systemd can start it from `burrowd.socket` and hand it the listening
//! socket. Without systemd, the first client to find no daemon starts its
//! own program again with [`DAEMON_ARG`]. Either way it stops by itself
//! once nothing has been connected or running for [`IDLE`].

use std::ffi::OsStr;
use std::io::Write;
use std::os::fd::FromRawFd;
use std::path::PathBuf;
use std::time::Duration;

use tokio::net::UnixListener;
use tokio::signal::unix::{SignalKind, signal};

use crate::engine::{Core, CoreOptions};
use crate::error::{Error, Result};
use crate::ipc::{self, LOCK, SOCKET};

/// The argument that makes a client program run as the daemon instead.
pub const DAEMON_ARG: &str = "--burrowd";

/// How long the daemon waits with no clients and no sessions before it
/// stops.
const IDLE: Duration = Duration::from_secs(60);

/// The first file descriptor systemd passes, by its protocol.
const LISTEN_FDS_START: i32 = 3;

/// Runs the daemon instead of the client when the program was started with
/// [`DAEMON_ARG`]. Clients call this first thing in `main`, before they
/// start any threads.
pub fn run_if_asked() {
    if std::env::args_os().nth(1).as_deref() == Some(OsStr::new(DAEMON_ARG)) {
        main();
    }
}

/// Runs the daemon until it stops, then exits.
pub fn main() -> ! {
    match run() {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            eprintln!("burrowd: {e}");
            std::process::exit(1);
        }
    }
}

fn run() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
    // Both change the environment, which is only safe before there are
    // other threads.
    let activated = take_activated_socket();
    find_agent();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .thread_name("burrowd")
        .enable_all()
        .build()
        .map_err(|e| Error::Task(e.to_string()))?;
    runtime.block_on(serve(activated))
}

async fn serve(activated: Option<std::os::unix::net::UnixListener>) -> Result<()> {
    let dir = ipc::runtime_dir()?;
    ipc::ensure_private_dir(&dir)?;
    let lock_path = dir.join(LOCK);
    // Opened without truncating: the pid in it belongs to whoever holds it.
    let mut lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| Error::io(&lock_path, e))?;
    if lock.try_lock().is_err() {
        tracing::info!("another burrowd is already running for {}", dir.display());
        return Ok(());
    }
    // The pid, for scripts that need to find the daemon.
    lock.set_len(0)
        .and_then(|()| writeln!(lock, "{}", std::process::id()))
        .map_err(|e| Error::io(&lock_path, e))?;
    // A socket left behind is removed only by the daemon holding the lock,
    // so it never takes the place of a live one.
    let (listener, bound) = match activated {
        Some(listener) => {
            listener
                .set_nonblocking(true)
                .map_err(|e| Error::io("systemd socket", e))?;
            let listener =
                UnixListener::from_std(listener).map_err(|e| Error::io("systemd socket", e))?;
            (listener, None)
        }
        None => {
            let path = dir.join(SOCKET);
            match std::fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    return Err(Error::io(&path, e));
                }
                _ => {}
            }
            let listener = UnixListener::bind(&path).map_err(|e| Error::io(&path, e))?;
            (listener, Some(path))
        }
    };

    let core = Core::open(CoreOptions::from_env()?)?;
    core.autostart();
    let _watcher = core
        .watch()
        .inspect_err(|e| tracing::warn!("live reload is off: {e}"))
        .ok();
    tracing::info!("burrowd {} serving on {}", ipc::BUILD, dir.display());

    let mut terminate = signal(SignalKind::terminate()).map_err(|e| Error::io("SIGTERM", e))?;
    let mut interrupt = signal(SignalKind::interrupt()).map_err(|e| Error::io("SIGINT", e))?;
    tokio::select! {
        () = ipc::serve(core, listener, Some(IDLE)) => tracing::info!("stopping: idle"),
        _ = terminate.recv() => tracing::info!("stopping: SIGTERM"),
        _ = interrupt.recv() => tracing::info!("stopping: SIGINT"),
    }
    if let Some(path) = bound {
        let _ = std::fs::remove_file(path);
    }
    // A pid left behind could later name some other process.
    let _ = lock.set_len(0);
    drop(lock);
    Ok(())
}

/// The socket systemd passed, if it started this process for one. The
/// variables are removed so programs the daemon starts don't take it too.
fn take_activated_socket() -> Option<std::os::unix::net::UnixListener> {
    let pid = std::env::var("LISTEN_PID").ok()?;
    let fds = std::env::var("LISTEN_FDS").ok()?;
    // SAFETY: no other threads exist yet (see `run`).
    unsafe {
        std::env::remove_var("LISTEN_PID");
        std::env::remove_var("LISTEN_FDS");
        std::env::remove_var("LISTEN_FDNAMES");
    }
    if pid.parse() != Ok(std::process::id()) || fds.parse::<u32>().ok()? < 1 {
        return None;
    }
    // SAFETY: systemd passes the listening socket as fd 3 and nothing else
    // in this process has taken it.
    Some(unsafe { std::os::unix::net::UnixListener::from_raw_fd(LISTEN_FDS_START) })
}

/// Started by systemd, the daemon may not have `SSH_AUTH_SOCK`, which
/// desktop sessions set only for programs they start. The socket of
/// OpenSSH's own `ssh-agent.socket` user unit is used then, when it exists.
fn find_agent() {
    if std::env::var_os("SSH_AUTH_SOCK").is_some() {
        return;
    }
    let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") else {
        return;
    };
    let agent = PathBuf::from(runtime).join("ssh-agent.socket");
    if agent.exists() {
        tracing::info!("SSH_AUTH_SOCK is not set; using {}", agent.display());
        // SAFETY: no other threads exist yet (see `run`).
        unsafe { std::env::set_var("SSH_AUTH_SOCK", agent) };
    }
}
