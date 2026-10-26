//! Running the programs the agent asks for what it can't read from /proc.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long a program may take. A wedged one shouldn't stall the samples,
/// which are sent from the same loop.
const TIMEOUT: Duration = Duration::from_secs(5);

/// The most kept of what a program prints.
const MAX_OUTPUT: u64 = 8 << 20;

/// What `program` prints when it succeeds, or why it didn't: the first line
/// it wrote to stderr, or that it took too long.
pub fn output(program: &Path, args: &[&str], path: Option<&str>) -> Result<String, String> {
    let mut command = Command::new(program);
    command
        .args(args)
        .env("SYSTEMD_COLORS", "0")
        .env("NO_COLOR", "1")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(path) = path {
        command.env("PATH", path);
    }
    let mut child = command.spawn().map_err(|e| e.to_string())?;
    // Read on other threads, so a full pipe can't keep the child from
    // exiting while this one waits.
    let read = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut text = String::new();
            if let Some(pipe) = pipe {
                let _ = pipe.take(MAX_OUTPUT).read_to_string(&mut text);
            }
            text
        })
    };
    let stdout = read(child.stdout.take().map(|p| Box::new(p) as _));
    let stderr = read(child.stderr.take().map(|p| Box::new(p) as _));
    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{} took too long", name(program)));
            }
        }
    };
    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();
    if status.success() {
        return Ok(stdout);
    }
    let reason = stderr.lines().map(str::trim).find(|line| !line.is_empty());
    Err(match reason {
        Some(line) => line.chars().take(200).collect(),
        None => format!("{} failed ({status})", name(program)),
    })
}

/// A program on `PATH`, the way a shell would find it.
pub fn find(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| is_program(candidate))
}

pub fn is_program(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

fn name(program: &Path) -> String {
    program.file_name().map_or_else(
        || program.display().to_string(),
        |name| name.to_string_lossy().into(),
    )
}
