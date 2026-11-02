//! Samples as they arrive, and the history the charts can show instead.

use std::time::{Duration, Instant};

use burrow_core::ipc::Client;
use burrow_core::monitor::Sample;
use burrow_core::{Command, Response, TrendRange};
use relm4::ComponentSender;
use relm4::gtk::prelude::*;

use crate::app::{App, AppMsg};
use crate::files::{printable, size};
use crate::runtime::runtime;

use super::charts::Meter;
use super::page::label;
use super::{Monitor, Msg};

/// What the charts can show: the live samples, or the history on disk.
pub(super) const RANGES: [(&str, Option<TrendRange>); 4] = [
    ("Live", None),
    ("1 h", Some(TrendRange::Hour)),
    ("24 h", Some(TrendRange::Day)),
    ("7 d", Some(TrendRange::Week)),
];

/// How often a chart of the history on disk is read again. A point is
/// written a minute.
pub(super) const TREND_REFRESH: Duration = Duration::from_secs(60);

impl Monitor {
    pub fn sample(&mut self, sample: &Sample, core: &Client, sender: &ComponentSender<App>) {
        self.session.show_content();
        if let Some(range) = self.range
            && self
                .trend_read
                .is_none_or(|read| read.elapsed() >= TREND_REFRESH)
        {
            self.read_trend(range, core, sender);
        }
        self.summary.set_label(&format!(
            "Up {} · load {:.2} {:.2} {:.2} · {} cores",
            duration(sample.uptime),
            sample.load[0],
            sample.load[1],
            sample.load[2],
            sample.cpu.cores,
        ));

        self.cpu.push(&[sample.cpu.percent]);
        self.cpu.set_max(100.0);
        self.scale_net();
        self.cpu_label
            .set_label(&format!("CPU {:.0}%", sample.cpu.percent));

        let rx: f64 = sample.net.iter().map(|i| i.rx).sum();
        let tx: f64 = sample.net.iter().map(|i| i.tx).sum();
        self.net.push(&[rx, tx]);
        let scale = self.scale_net();
        self.net_label.set_label(&format!(
            "Network: in {}/s, out {}/s (scale {}/s)",
            size(rx as u64),
            size(tx as u64),
            size(scale as u64)
        ));

        let memory = &sample.memory;
        self.memory
            .set(memory.total - memory.available, memory.total);
        self.swap
            .set(memory.swap_total - memory.swap_free, memory.swap_total);
        self.swap.row.set_visible(memory.swap_total > 0);

        let mounts: Vec<&str> = sample.disks.iter().map(|d| d.mount.as_str()).collect();
        if mounts
            != self
                .disk_rows
                .iter()
                .map(|(m, _)| m.as_str())
                .collect::<Vec<_>>()
        {
            while let Some(child) = self.disks.first_child() {
                self.disks.remove(&child);
            }
            self.disk_rows.clear();
            for (row, disk) in sample.disks.iter().enumerate() {
                let meter = Meter::new();
                let name = label(&printable(&disk.mount));
                name.set_tooltip_text(Some(&printable(&disk.fs)));
                let row = row as i32;
                self.disks.attach(&name, 0, row, 1, 1);
                self.disks.attach(&meter.bar, 1, row, 1, 1);
                self.disks.attach(&meter.text, 2, row, 1, 1);
                self.disk_rows.push((disk.mount.clone(), meter));
            }
        }
        for ((_, meter), disk) in self.disk_rows.iter().zip(&sample.disks) {
            meter.set(disk.total - disk.available, disk.total);
        }
    }

    /// Scales the network chart to what it shows, rounded up to a size
    /// that reads well so the scale doesn't creep, and returns the scale.
    pub(super) fn scale_net(&self) -> f64 {
        let peak = self.net.peak().max(1024.0);
        let scale = 2f64.powf(peak.log2().ceil());
        self.net.set_max(scale);
        scale
    }

    /// Asks the core for the history on disk over `range`. The reply
    /// arrives as [`Msg::Trend`].
    pub(super) fn read_trend(
        &mut self,
        range: TrendRange,
        core: &Client,
        sender: &ComponentSender<App>,
    ) {
        self.trend_read = Some(Instant::now());
        let (core, node, input) = (core.clone(), self.node, sender.input_sender().clone());
        runtime().spawn(async move {
            if let Ok(Response::Trend { points }) =
                core.execute(Command::MonitorTrend { node, range }).await
            {
                let msg = Msg::Trend(range, points);
                let _ = input.send(AppMsg::Monitor { node, msg });
            }
        });
    }

    pub(super) fn show_trend(&mut self, range: TrendRange, points: &[Sample]) {
        // The range's whole length, so a short history fills in from the
        // right as the live charts do. Gaps while Burrow was off close up.
        let width = match range {
            TrendRange::Hour => 60,
            TrendRange::Day => 24 * 60,
            TrendRange::Week => 7 * 24 * 12,
        };
        let cpu = points.iter().map(|p| p.cpu.percent).collect();
        let rx = points
            .iter()
            .map(|p| p.net.iter().map(|i| i.rx).sum())
            .collect();
        let tx = points
            .iter()
            .map(|p| p.net.iter().map(|i| i.tx).sum())
            .collect();
        self.cpu.show(Some((vec![cpu], width)));
        self.net.show(Some((vec![rx, tx], width)));
        self.scale_net();
        self.trend_note.set_label(if points.len() < 2 {
            "Not enough history yet. Burrow keeps a point a minute while it monitors this host."
        } else {
            "Averaged history from this computer. The figures above are live."
        });
        self.trend_note.set_visible(true);
    }
}

/// "3 days 4 h", "5 h 12 min" or "7 min".
pub(super) fn duration(seconds: f64) -> String {
    let minutes = (seconds / 60.0) as u64;
    let (days, hours, minutes) = (minutes / 1440, minutes / 60 % 24, minutes % 60);
    match (days, hours) {
        (0, 0) => format!("{minutes} min"),
        (0, _) => format!("{hours} h {minutes} min"),
        (1, _) => format!("1 day {hours} h"),
        _ => format!("{days} days {hours} h"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_well() {
        assert_eq!(duration(59.0), "0 min");
        assert_eq!(duration(7.0 * 60.0), "7 min");
        assert_eq!(duration(5.0 * 3600.0 + 12.0 * 60.0), "5 h 12 min");
        assert_eq!(duration(86400.0 + 4.0 * 3600.0), "1 day 4 h");
        assert_eq!(duration(3.0 * 86400.0), "3 days 0 h");
    }
}
