//! What `burrow-agent` writes: one JSON [`Message`] per line on stdout.
//!
//! The agent runs on a server, which Burrow doesn't trust, so whoever reads
//! these must treat every field as a claim: names may hold anything, and
//! numbers may be out of range.

use serde::{Deserialize, Serialize};

/// The wire format's version. A change that an older reader would get
/// wrong bumps it, and Burrow refuses an agent whose version isn't its own.
pub const PROTO: u32 = 1;

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
