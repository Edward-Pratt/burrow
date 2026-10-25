//! Alerts on hosts monitored in the background, from fixed rules.
//!
//! Disks and memory are judged on a minute's average, so a spike doesn't
//! raise anything, and they clear a little past where they were raised,
//! so a value that hovers at the line doesn't raise one again and again.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use burrow_agent::{Sample, Unit};

use crate::api::{Alert, AlertId, AlertKind, Event, NodeId};

/// Disk use, in percent, that raises an alert, and that clears it.
const DISK_FULL: f64 = 90.0;
const DISK_CLEAR: f64 = 85.0;

/// Available memory, in percent, that raises an alert, and that clears it.
const MEMORY_LOW: f64 = 10.0;
const MEMORY_CLEAR: f64 = 15.0;

/// The alerts that hold now.
#[derive(Default)]
pub(crate) struct Alerts {
    next: AtomicU64,
    active: Mutex<Vec<Alert>>,
}

/// Which rule an alert comes from. Each rule reports everything that
/// holds for it at once, so what it no longer reports is cleared.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Rule {
    Units,
    Disks,
    Memory,
    Reachable,
}

impl Rule {
    fn of(kind: &AlertKind) -> Rule {
        match kind {
            AlertKind::UnitFailed { .. } => Rule::Units,
            AlertKind::DiskFull { .. } => Rule::Disks,
            AlertKind::MemoryLow { .. } => Rule::Memory,
            AlertKind::Unreachable { .. } => Rule::Reachable,
        }
    }
}

impl Alerts {
    pub fn list(&self) -> Vec<Alert> {
        self.active.lock().unwrap().clone()
    }

    /// Sets what `rule` finds on `node` now, at `now` (Unix milliseconds),
    /// and returns the events for what was raised and cleared. An alert
    /// that still holds keeps its first id and time.
    pub fn set(&self, node: NodeId, rule: Rule, found: Vec<AlertKind>, now: u64) -> Vec<Event> {
        let mut active = self.active.lock().unwrap();
        let mut events = Vec::new();
        active.retain(|alert| {
            let gone = alert.node == node
                && Rule::of(&alert.kind) == rule
                && !found.iter().any(|kind| same(kind, &alert.kind));
            if gone {
                events.push(Event::AlertCleared {
                    alert: alert.clone(),
                });
            }
            !gone
        });
        for kind in found {
            if active
                .iter()
                .any(|alert| alert.node == node && same(&kind, &alert.kind))
            {
                continue;
            }
            let alert = Alert {
                id: AlertId(self.next.fetch_add(1, Ordering::Relaxed) + 1),
                node,
                kind,
                since: now,
            };
            active.push(alert.clone());
            events.push(Event::AlertRaised { alert });
        }
        events
    }

    /// Clears all of `node`'s alerts, as when its monitor stops.
    pub fn clear(&self, node: NodeId) -> Vec<Event> {
        let mut active = self.active.lock().unwrap();
        let (cleared, kept): (Vec<Alert>, Vec<Alert>) =
            active.drain(..).partition(|alert| alert.node == node);
        *active = kept;
        cleared
            .into_iter()
            .map(|alert| Event::AlertCleared { alert })
            .collect()
    }

    /// Whether `node` has an alert like `kind`.
    fn has(&self, node: NodeId, kind: &AlertKind) -> bool {
        let active = self.active.lock().unwrap();
        active
            .iter()
            .any(|alert| alert.node == node && same(kind, &alert.kind))
    }

    /// The disks of a minute's `point` that are full, or still too full
    /// to clear an alert they already have.
    pub fn full_disks(&self, node: NodeId, point: &Sample) -> Vec<AlertKind> {
        point
            .disks
            .iter()
            .filter(|disk| disk.total > 0)
            .filter_map(|disk| {
                let used = disk.total.saturating_sub(disk.available) as f64;
                let percent = 100.0 * used / disk.total as f64;
                let kind = AlertKind::DiskFull {
                    mount: disk.mount.clone(),
                    percent,
                };
                let line = if self.has(node, &kind) {
                    DISK_CLEAR
                } else {
                    DISK_FULL
                };
                (percent > line).then_some(kind)
            })
            .collect()
    }

    /// Low memory in a minute's `point`, judged as for disks.
    pub fn low_memory(&self, node: NodeId, point: &Sample) -> Vec<AlertKind> {
        let memory = &point.memory;
        if memory.total == 0 {
            return Vec::new();
        }
        let percent = 100.0 * memory.available as f64 / memory.total as f64;
        let kind = AlertKind::MemoryLow { percent };
        let line = if self.has(node, &kind) {
            MEMORY_CLEAR
        } else {
            MEMORY_LOW
        };
        if percent < line {
            vec![kind]
        } else {
            Vec::new()
        }
    }
}

/// The services that failed.
pub(crate) fn failed_units(units: &[Unit]) -> Vec<AlertKind> {
    units
        .iter()
        .filter(|unit| unit.active == "failed")
        .map(|unit| AlertKind::UnitFailed {
            unit: unit.name.clone(),
        })
        .collect()
}

/// Whether two alerts are about the same thing, whatever their numbers.
fn same(a: &AlertKind, b: &AlertKind) -> bool {
    match (a, b) {
        (AlertKind::UnitFailed { unit: a }, AlertKind::UnitFailed { unit: b }) => a == b,
        (AlertKind::DiskFull { mount: a, .. }, AlertKind::DiskFull { mount: b, .. }) => a == b,
        (AlertKind::MemoryLow { .. }, AlertKind::MemoryLow { .. })
        | (AlertKind::Unreachable { .. }, AlertKind::Unreachable { .. }) => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use burrow_agent::Disk;

    fn disk(used: u64) -> Sample {
        Sample {
            disks: vec![Disk {
                mount: "/".into(),
                fs: "ext4".into(),
                total: 100,
                available: 100 - used,
            }],
            ..Sample::default()
        }
    }

    fn raised(events: &[Event]) -> usize {
        events
            .iter()
            .filter(|e| matches!(e, Event::AlertRaised { .. }))
            .count()
    }

    fn cleared(events: &[Event]) -> usize {
        events
            .iter()
            .filter(|e| matches!(e, Event::AlertCleared { .. }))
            .count()
    }

    #[test]
    fn a_full_disk_is_raised_once_and_clears_below_the_line() {
        let alerts = Alerts::default();
        let node = NodeId::new();
        let check = |used| {
            let found = alerts.full_disks(node, &disk(used));
            alerts.set(node, Rule::Disks, found, 0)
        };
        assert!(check(89).is_empty());
        assert_eq!(raised(&check(91)), 1);
        // Still full, or back between the lines: nothing new.
        assert!(check(95).is_empty());
        assert!(check(87).is_empty());
        assert_eq!(cleared(&check(84)), 1);
        assert!(check(89).is_empty());
    }

    #[test]
    fn low_memory_is_judged_on_what_is_available() {
        let alerts = Alerts::default();
        let node = NodeId::new();
        let mut point = Sample::default();
        point.memory.total = 100;
        point.memory.available = 5;
        let found = alerts.low_memory(node, &point);
        assert_eq!(raised(&alerts.set(node, Rule::Memory, found, 0)), 1);
        point.memory.available = 12;
        assert!(!alerts.low_memory(node, &point).is_empty());
        point.memory.available = 20;
        let found = alerts.low_memory(node, &point);
        assert_eq!(cleared(&alerts.set(node, Rule::Memory, found, 0)), 1);
        point.memory.total = 0;
        assert!(alerts.low_memory(node, &point).is_empty());
    }

    #[test]
    fn each_failed_unit_is_its_own_alert_and_hosts_are_apart() {
        let alerts = Alerts::default();
        let (web, db) = (NodeId::new(), NodeId::new());
        let unit = |name: &str, active: &str| Unit {
            name: name.into(),
            active: active.into(),
            ..Unit::default()
        };
        let units = [unit("a.service", "failed"), unit("b.service", "active")];
        let events = alerts.set(web, Rule::Units, failed_units(&units), 7);
        assert_eq!(raised(&events), 1);
        let units = [unit("a.service", "failed"), unit("b.service", "failed")];
        assert_eq!(
            raised(&alerts.set(web, Rule::Units, failed_units(&units), 8)),
            1
        );
        assert_eq!(alerts.list()[0].since, 7);

        alerts.set(
            db,
            Rule::Reachable,
            vec![AlertKind::Unreachable { error: "x".into() }],
            9,
        );
        assert_eq!(cleared(&alerts.set(web, Rule::Units, Vec::new(), 10)), 2);
        assert_eq!(alerts.list().len(), 1);
        assert_eq!(cleared(&alerts.clear(db)), 1);
        assert!(alerts.list().is_empty());
    }
}
