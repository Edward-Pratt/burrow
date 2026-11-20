//! The monitor tool: a summary of a host's stats rather than every sample.

use std::time::Duration;

use burrow_core::{Command, Response, SessionId};
use serde_json::{Value, json};

use super::{Bridge, Called, pretty};

/// How long the monitor tool waits for a new monitor's first sample.
const FIRST_SAMPLE: Duration = Duration::from_secs(5);

impl Bridge {
    /// The latest stats and a summary of the history, rather than every
    /// sample, which would fill the assistant's context.
    pub(super) async fn monitor(
        &self,
        session: SessionId,
        all_units: bool,
    ) -> Result<String, Called> {
        // A monitor that just started has no sample yet.
        let deadline = tokio::time::Instant::now() + FIRST_SAMPLE;
        let history = loop {
            let history = match self.execute(Command::MonitorHistory { session }).await? {
                Response::Monitor { history } => history,
                other => return Ok(pretty(&other)),
            };
            if !history.samples.is_empty() || tokio::time::Instant::now() >= deadline {
                break history;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        };
        let mut summary = monitor_summary(&history, all_units);
        // A host monitored in the background has alerts too.
        let running = self.sessions_list().await?;
        let info = running.iter().find(|s| s.session == session);
        if let Some(info) = info.filter(|info| info.background) {
            let alerts = match self.execute(Command::ListAlerts).await? {
                Response::Alerts { alerts } => alerts,
                _ => Vec::new(),
            };
            let alerts: Vec<_> = alerts.into_iter().filter(|a| a.node == info.node).collect();
            summary["background"] = json!(true);
            summary["alerts"] = json!(alerts);
        }
        Ok(serde_json::to_string_pretty(&summary).unwrap_or_default())
    }
}

/// The latest sample, the lowest, mean and highest CPU and memory use
/// over the history, and the services: the failed ones, or all of them.
fn monitor_summary(history: &burrow_core::MonitorHistory, all_units: bool) -> Value {
    let samples = &history.samples;
    let spread = |value: &dyn Fn(&burrow_core::monitor::Sample) -> f64| {
        let values: Vec<f64> = samples.iter().map(value).collect();
        if values.is_empty() {
            return Value::Null;
        }
        let mean = values.iter().sum::<f64>() / values.len() as f64;
        let low = values.iter().copied().fold(f64::INFINITY, f64::min);
        let high = values.iter().copied().fold(0.0, f64::max);
        let round = |v: f64| (v * 10.0).round() / 10.0;
        json!({"min": round(low), "mean": round(mean), "max": round(high)})
    };
    let memory_used = |s: &burrow_core::monitor::Sample| {
        let total = s.memory.total.max(1) as f64;
        (total - s.memory.available as f64) * 100.0 / total
    };
    let minutes = match (samples.first(), samples.last()) {
        (Some(first), Some(last)) => last.time.saturating_sub(first.time) as f64 / 60_000.0,
        _ => 0.0,
    };
    let units = history.units.as_ref().map(|units| {
        let failed: Vec<_> = units.iter().filter(|u| u.active == "failed").collect();
        if all_units {
            json!({"count": units.len(), "failed": failed.len(), "units": units})
        } else {
            json!({"count": units.len(), "failed": failed})
        }
    });
    json!({
        "latest": samples.last(),
        "history": {
            "samples": samples.len(),
            "minutes": (minutes * 10.0).round() / 10.0,
            "cpu_percent": spread(&|s| s.cpu.percent),
            "memory_used_percent": spread(&memory_used),
        },
        "systemd": history.systemd,
        "services": units,
        "apps": history.apps,
    })
}
