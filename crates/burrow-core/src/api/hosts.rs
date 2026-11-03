//! What monitors and commands see on an SSH host: alerts, the services
//! there and their actions, what a terminal runs instead of a shell,
//! trends, monitor history and the result of a command.

use serde::{Deserialize, Serialize};

use super::{NodeId, monitor};

/// Identifies an alert. Unique for as long as the core runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AlertId(pub u64);

/// Something wrong on an SSH host monitored in the background. It is
/// raised once, and cleared once the condition ends.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Alert {
    pub id: AlertId,
    pub node: NodeId,
    #[serde(flatten)]
    pub kind: AlertKind,
    /// When it was raised, in Unix milliseconds by this computer's clock.
    pub since: u64,
}

/// What an [`Alert`] is about. Names come from the server, cleaned.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AlertKind {
    /// A systemd service failed.
    UnitFailed { unit: String },
    /// A disk is over 90% full, by a minute's average. It clears below 85%.
    DiskFull { mount: String, percent: f64 },
    /// Under 10% of memory is available, by a minute's average. It clears
    /// above 15%.
    MemoryLow { percent: f64 },
    /// The host or its agent stopped answering for a minute, after the
    /// monitor had connected.
    Unreachable { error: String },
    /// A container exited with an error, died or keeps restarting.
    /// `status` is what `docker ps` says, such as "Exited (1) 2 minutes ago".
    ContainerFailed { container: String, status: String },
    /// A PM2 app errored: PM2 gave up restarting it.
    AppFailed { app: String },
}

/// Something that runs on a server, as a monitor lists it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Service {
    /// A systemd service.
    Unit { name: String },
    /// A container, of `docker` or `podman`.
    Container { runtime: String, name: String },
    /// A PM2 app, with the `pm2` program the monitor found.
    Pm2App { pm2: String, name: String },
}

impl Service {
    pub fn name(&self) -> &str {
        match self {
            Service::Unit { name }
            | Service::Container { name, .. }
            | Service::Pm2App { name, .. } => name,
        }
    }

    /// The actions it takes, in the order a menu shows them.
    pub fn actions(&self) -> &'static [ServiceAction] {
        use ServiceAction::*;
        match self {
            Service::Unit { .. } => &[Start, Stop, Restart, Reload, Enable, Disable],
            Service::Container { .. } => &[Start, Stop, Restart],
            Service::Pm2App { .. } => &[Start, Stop, Restart, Reload],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceAction {
    Start,
    Stop,
    Restart,
    Reload,
    /// Start at boot. Not started now.
    Enable,
    Disable,
}

impl ServiceAction {
    /// The verb the programs take, which is also its name in the API.
    pub fn verb(self) -> &'static str {
        match self {
            ServiceAction::Start => "start",
            ServiceAction::Stop => "stop",
            ServiceAction::Restart => "restart",
            ServiceAction::Reload => "reload",
            ServiceAction::Enable => "enable",
            ServiceAction::Disable => "disable",
        }
    }
}

/// What an [`Command::OpenTerminal`](crate::api::Command::OpenTerminal) session runs instead of a shell.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TerminalRun {
    /// Follows a service's log: its journal, `docker logs` or `pm2 logs`.
    Logs { service: Service },
    /// Runs an action, with sudo free to ask for a password.
    Control {
        service: Service,
        action: ServiceAction,
    },
}

/// How far back [`Command::MonitorTrend`](crate::api::Command::MonitorTrend) looks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrendRange {
    Hour,
    Day,
    Week,
}

/// What a monitor has seen, for [`Command::MonitorHistory`](crate::api::Command::MonitorHistory).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MonitorHistory {
    /// Whether the server runs systemd, so whether `units` will come.
    pub systemd: bool,
    /// The samples of about the last hour, oldest first.
    pub samples: Vec<monitor::Sample>,
    /// The services, as last listed.
    pub units: Option<Vec<monitor::Unit>>,
    /// The containers and PM2 apps, as last listed.
    #[serde(default)]
    pub apps: Option<monitor::Apps>,
}

/// What a command run with [`Command::Exec`](crate::api::Command::Exec) did. Output is decoded as
/// UTF-8, with invalid bytes replaced, and each stream is cut at 1 MiB.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecResult {
    /// The exit status, when the command exited.
    pub status: Option<u32>,
    /// The signal that ended it, when one did.
    pub signal: Option<String>,
    pub stdout: String,
    pub stderr: String,
    /// Output went past the limit and the rest was dropped.
    pub truncated: bool,
    /// The command was still running at the timeout. It was sent SIGTERM
    /// (on servers that take signals) and the output so far is here.
    pub timed_out: bool,
}
