//! What `burrow-agent` writes: one JSON [`Message`] per line on stdout.
//!
//! The agent runs on a server, which Burrow doesn't trust, so whoever reads
//! these must treat every field as a claim: names may hold anything, and
//! numbers may be out of range.

use serde::{Deserialize, Serialize};

/// The wire format's version. A change that an older reader would get
/// wrong bumps it, and Burrow refuses an agent whose version isn't its own.
pub const PROTO: u32 = 2;

/// The agent's version, which is also its file name on servers.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    /// The first line. `systemd` is whether `systemctl` is there, so a
    /// reader knows not to wait for [`Message::Units`].
    Hello {
        version: String,
        proto: u32,
        systemd: bool,
    },
    /// Host stats, once per interval.
    Sample(Sample),
    /// Every systemd service, less often than samples because listing them
    /// runs a program.
    Units { units: Vec<Unit> },
    /// Containers and PM2 apps, as often as units.
    Apps(Apps),
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    /// Unix time in milliseconds, by the server's clock.
    pub time: u64,
    /// Seconds since the server booted.
    pub uptime: f64,
    /// The 1, 5 and 15 minute load averages.
    pub load: [f64; 3],
    pub cpu: Cpu,
    pub memory: Memory,
    pub disks: Vec<Disk>,
    pub net: Vec<Interface>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Cpu {
    /// Busy time over the last interval, 0 to 100, across all cores.
    pub percent: f64,
    pub cores: u32,
}

/// Bytes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Memory {
    pub total: u64,
    /// What programs could still get without swapping (`MemAvailable`).
    pub available: u64,
    pub swap_total: u64,
    pub swap_free: u64,
}

/// A mounted filesystem backed by a device, in bytes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Disk {
    pub mount: String,
    pub fs: String,
    pub total: u64,
    /// What an unprivileged user may still write.
    pub available: u64,
}

/// A network interface's traffic over the last interval, in bytes a second.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Interface {
    pub name: String,
    pub rx: f64,
    pub tx: f64,
}

/// A systemd service, as `systemctl list-units` shows it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Unit {
    pub name: String,
    /// `loaded`, `not-found`, `masked`…
    pub load: String,
    /// `active`, `inactive`, `failed`…
    pub active: String,
    /// `running`, `exited`, `dead`…
    pub sub: String,
    pub description: String,
}

/// What runs on the server besides systemd services.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Apps {
    /// `None` when neither Docker nor Podman is installed.
    pub containers: Option<Containers>,
    /// `None` when PM2 isn't running for the SSH user.
    pub pm2: Option<Pm2>,
}

/// Every container, running or not, as `docker ps --all` or `podman ps
/// --all` lists them for the SSH user.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Containers {
    /// `docker` or `podman`.
    pub runtime: String,
    /// Why they couldn't be listed, such as no access to Docker's socket.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub list: Vec<Container>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Container {
    /// The short id, 12 characters.
    pub id: String,
    pub name: String,
    pub image: String,
    /// `running`, `exited`, `restarting`, `paused`, `dead`…
    pub state: String,
    /// What `ps` says about it, such as "Up 3 hours" or "Exited (1) 2
    /// minutes ago".
    pub status: String,
}

impl Container {
    /// Whether it exited with an error, died or keeps restarting. One that
    /// exited cleanly is often a job that finished.
    pub fn failed(&self) -> bool {
        match self.state.as_str() {
            "dead" | "restarting" => true,
            "exited" => !self.status.starts_with("Exited (0)"),
            _ => false,
        }
    }
}

/// The apps PM2 runs for the SSH user.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Pm2 {
    /// Why they couldn't be listed, such as `pm2` not being found.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Where `pm2` is, which may not be on a plain SSH `PATH`, so acting
    /// on an app can run the same one. Empty when it wasn't found.
    #[serde(default)]
    pub path: String,
    pub apps: Vec<Pm2App>,
}

/// A PM2 app. Only these fields leave the server: what `pm2 jlist` says
/// also holds each app's environment, secrets included.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Pm2App {
    pub id: u64,
    pub name: String,
    /// `online`, `stopped`, `errored`, `launching`…
    pub status: String,
    /// Percent of one core.
    pub cpu: f64,
    /// Bytes.
    pub memory: u64,
    pub restarts: u64,
    /// When it last started, in Unix milliseconds, by the server's clock.
    pub since: u64,
}

impl Pm2App {
    /// Whether PM2 gave up restarting it. A stopped app was stopped on
    /// purpose.
    pub fn failed(&self) -> bool {
        self.status == "errored"
    }
}
