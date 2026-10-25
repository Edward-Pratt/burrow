//! A background monitor's history on disk: one point a minute, averaged
//! from its samples, kept for a week.
//!
//! Points are JSON lines in `<state>/monitor/<node>/<YYYY-MM-DD>.jsonl`,
//! one file per UTC day by this computer's clock. The server's clock isn't
//! trusted to name files or order points. Each point has the same shape as
//! a sample, so clients draw both the same way.

use std::io::{BufRead, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use burrow_agent::Sample;

use crate::api::{NodeId, TrendRange};
use crate::error::{Error, Result};

const DAY: u64 = 24 * 60 * 60 * 1000;

/// One point a minute is written.
pub(crate) const MINUTE: Duration = Duration::from_secs(60);

/// How far back points are kept.
const KEEP: u64 = 7 * DAY;

/// The most points one reply holds: a week at one every 5 minutes.
const MAX_POINTS: u64 = 2016;

/// What a point keeps of a sample's lists, and the most it may take as
/// JSON, so a server can't make Burrow keep more than about 40 MB a host.
const MAX_ITEMS: usize = 32;
const MAX_POINT: usize = 4 << 10;

/// Averages the samples of each span of time into one point.
pub(crate) struct Rollup {
    width: u64,
    span: Option<u64>,
    count: u32,
    cpu: f64,
    available: f64,
    swap_free: f64,
    /// Each interface's name, total `rx` and `tx`, and how many samples
    /// had it.
    net: Vec<(String, f64, f64, u32)>,
    last: Sample,
}

impl Rollup {
    pub fn new(width: Duration) -> Rollup {
        Rollup {
            width: (width.as_millis() as u64).max(1),
            span: None,
            count: 0,
            cpu: 0.0,
            available: 0.0,
            swap_free: 0.0,
            net: Vec::new(),
            last: Sample::default(),
        }
    }

    /// Adds `sample`, taken at `time` (Unix milliseconds). Returns the
    /// point of the span before once a sample from another span arrives.
    pub fn push(&mut self, time: u64, sample: &Sample) -> Option<Sample> {
        let span = time / self.width;
        let done = match self.span {
            Some(current) if current != span => self.take(),
            _ => None,
        };
        self.span = Some(span);
        self.count += 1;
        self.cpu += sample.cpu.percent;
        self.available += sample.memory.available as f64;
        self.swap_free += sample.memory.swap_free as f64;
        for interface in &sample.net {
            match self.net.iter_mut().find(|n| n.0 == interface.name) {
                Some(net) => {
                    net.1 += interface.rx;
                    net.2 += interface.tx;
                    net.3 += 1;
                }
                None => self
                    .net
                    .push((interface.name.clone(), interface.rx, interface.tx, 1)),
            }
        }
        self.last = sample.clone();
        done
    }

    /// The point of the span being filled, if it has any samples.
    pub fn finish(mut self) -> Option<Sample> {
        self.take()
    }

    fn take(&mut self) -> Option<Sample> {
        let span = self.span?;
        let count = f64::from(self.count);
        let mut point = std::mem::take(&mut self.last);
        point.time = span * self.width;
        point.cpu.percent = self.cpu / count;
        point.memory.available = (self.available / count) as u64;
        point.memory.swap_free = (self.swap_free / count) as u64;
        point.net = std::mem::take(&mut self.net)
            .into_iter()
            .map(|(name, rx, tx, n)| burrow_agent::Interface {
                name,
                rx: rx / f64::from(n),
                tx: tx / f64::from(n),
            })
            .collect();
        *self = Rollup::new(Duration::from_millis(self.width));
        Some(fit(point))
    }
}

/// `point` cut down to [`MAX_ITEMS`] disks and interfaces, and then until
/// it fits in [`MAX_POINT`].
fn fit(mut point: Sample) -> Sample {
    point.disks.truncate(MAX_ITEMS);
    point.net.truncate(MAX_ITEMS);
    while serde_json::to_vec(&point).map_or(0, |json| json.len()) > MAX_POINT {
        if point.disks.len() >= point.net.len() {
            if point.disks.pop().is_none() {
                break;
            }
        } else {
            point.net.pop();
        }
    }
    point
}

/// Where points are kept, a folder per host.
pub(crate) struct Trends {
    dir: PathBuf,
}

impl Trends {
    pub fn new(dir: PathBuf) -> Trends {
        Trends { dir }
    }

    /// Adds a point to `node`'s file for its day, and deletes files more
    /// than a week old.
    pub fn append(&self, node: NodeId, point: &Sample) -> Result<()> {
        let dir = self.dir.join(node.to_string());
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .map_err(|e| Error::io(&dir, e))?;
        let path = dir.join(format!("{}.jsonl", date(point.time)));
        let mut line = serde_json::to_vec(point).map_err(|e| Error::Encode {
            path: path.clone(),
            message: e.to_string(),
        })?;
        line.push(b'\n');
        // The folder is Burrow's own, but a link put there still isn't
        // written through.
        std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .and_then(|mut file| file.write_all(&line))
            .map_err(|e| Error::io(&path, e))?;

        let oldest = date(point.time.saturating_sub(KEEP));
        for name in day_files(&dir) {
            if name.as_str() < oldest.as_str() {
                let _ = std::fs::remove_file(dir.join(format!("{name}.jsonl")));
            }
        }
        Ok(())
    }

    /// `node`'s points over `range` until `now`, oldest first, averaged
    /// into at most [`MAX_POINTS`]. A broken line is skipped.
    pub fn points(&self, node: NodeId, range: TrendRange, now: u64) -> Vec<Sample> {
        let length = match range {
            TrendRange::Hour => 60 * 60 * 1000,
            TrendRange::Day => DAY,
            TrendRange::Week => KEEP,
        };
        let since = now.saturating_sub(length);
        let dir = self.dir.join(node.to_string());
        let first = date(since);
        let mut points = Vec::new();
        for name in day_files(&dir) {
            if name.as_str() < first.as_str() {
                continue;
            }
            let path = dir.join(format!("{name}.jsonl"));
            let Ok(file) = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&path)
            else {
                continue;
            };
            for line in std::io::BufReader::new(file).lines() {
                let Ok(line) = line else { break };
                if let Ok(point) = serde_json::from_str::<Sample>(&line)
                    && point.time >= since
                    && point.time <= now
                {
                    points.push(point);
                }
            }
        }
        points.sort_by_key(|p| p.time);

        let width = length.div_ceil(MAX_POINTS).max(MINUTE.as_millis() as u64);
        let mut rollup = Rollup::new(Duration::from_millis(width));
        let mut bucketed: Vec<Sample> = points
            .iter()
            .filter_map(|point| rollup.push(point.time, point))
            .collect();
        bucketed.extend(rollup.finish());
        bucketed
    }

    /// Deletes the points of hosts that no longer exist.
    pub fn forget(&self, nodes: &[NodeId]) {
        for node in nodes {
            let _ = std::fs::remove_dir_all(self.dir.join(node.to_string()));
        }
    }
}

/// The dates of the day files in `dir`, oldest first.
fn day_files(dir: &std::path::Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|entry| {
            let name = entry.ok()?.file_name().into_string().ok()?;
            Some(name.strip_suffix(".jsonl")?.to_string())
        })
        .collect();
    names.sort();
    names
}

/// The UTC date of `time` (Unix milliseconds), as `YYYY-MM-DD`, from
/// Howard Hinnant's `civil_from_days`.
fn date(time: u64) -> String {
    let z = (time / DAY) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// This computer's clock, in Unix milliseconds.
pub(crate) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use burrow_agent::{Disk, Interface};

    fn sample(cpu: f64, available: u64, rx: f64) -> Sample {
        let mut sample = Sample::default();
        sample.cpu.percent = cpu;
        sample.cpu.cores = 4;
        sample.memory.total = 1000;
        sample.memory.available = available;
        sample.net = vec![Interface {
            name: "eth0".into(),
            rx,
            tx: 0.0,
        }];
        sample
    }

    #[test]
    fn dates_are_utc_days() {
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(951_782_400_000), "2000-02-29");
        assert_eq!(date(1_792_948_331_514), "2026-10-25");
    }

    #[test]
    fn a_minute_of_samples_becomes_one_averaged_point() {
        let mut rollup = Rollup::new(MINUTE);
        assert_eq!(rollup.push(60_000, &sample(10.0, 100, 1.0)), None);
        assert_eq!(rollup.push(90_000, &sample(30.0, 300, 3.0)), None);
        let point = rollup.push(120_000, &sample(50.0, 500, 5.0)).unwrap();
        assert_eq!(point.time, 60_000);
        assert_eq!(point.cpu.percent, 20.0);
        assert_eq!(point.cpu.cores, 4);
        assert_eq!(point.memory.available, 200);
        assert_eq!(point.memory.total, 1000);
        assert_eq!(point.net[0].rx, 2.0);
        let last = rollup.finish().unwrap();
        assert_eq!((last.time, last.cpu.percent), (120_000, 50.0));
    }

    #[test]
    fn a_point_stays_small_whatever_the_server_sends() {
        let mut big = sample(0.0, 0, 0.0);
        let name = "x".repeat(256);
        big.disks = vec![
            Disk {
                mount: name.clone(),
                fs: name.clone(),
                total: 1,
                available: 1,
            };
            256
        ];
        big.net = vec![
            Interface {
                name,
                rx: 0.0,
                tx: 0.0,
            };
            256
        ];
        let mut rollup = Rollup::new(MINUTE);
        rollup.push(0, &big);
        let point = rollup.finish().unwrap();
        assert!(serde_json::to_vec(&point).unwrap().len() <= MAX_POINT);
        assert!(!point.disks.is_empty() && !point.net.is_empty());
    }

    #[test]
    fn points_are_kept_a_week_and_read_back_by_range() {
        let temp = tempfile::TempDir::new().unwrap();
        let trends = Trends::new(temp.path().join("monitor"));
        let node = NodeId::new();
        let now = 20 * DAY;
        let old = Sample {
            time: now - 9 * DAY,
            ..sample(1.0, 0, 0.0)
        };
        trends.append(node, &old).unwrap();
        for minute in 0..3 {
            let point = Sample {
                time: now - DAY / 2 + minute * 60_000,
                ..sample(minute as f64, 0, 0.0)
            };
            trends.append(node, &point).unwrap();
        }
        let dir = temp.path().join("monitor").join(node.to_string());
        assert_eq!(day_files(&dir), vec![date(now - DAY / 2)]);

        assert!(trends.points(node, TrendRange::Hour, now).is_empty());
        let day = trends.points(node, TrendRange::Day, now);
        assert_eq!(day.len(), 3);
        assert_eq!(day[2].cpu.percent, 2.0);
        // A week is drawn in 5-minute points.
        let week = trends.points(node, TrendRange::Week, now);
        assert_eq!(week.len(), 1);
        assert_eq!(week[0].cpu.percent, 1.0);

        trends.forget(&[node]);
        assert!(!dir.exists());
    }

    #[test]
    fn a_link_in_the_folder_is_not_written_through() {
        let temp = tempfile::TempDir::new().unwrap();
        let trends = Trends::new(temp.path().join("monitor"));
        let node = NodeId::new();
        let dir = temp.path().join("monitor").join(node.to_string());
        std::fs::create_dir_all(&dir).unwrap();
        let target = temp.path().join("target");
        std::fs::write(&target, "").unwrap();
        std::os::unix::fs::symlink(&target, dir.join("1970-01-01.jsonl")).unwrap();
        assert!(trends.append(node, &sample(0.0, 0, 0.0)).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "");
    }
}
