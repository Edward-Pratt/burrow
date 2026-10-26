//! `burrow-agent`: reports a server's state to Burrow as JSON lines.
//!
//! Burrow uploads it to `~/.local/share/burrow/` and runs `burrow-agent
//! stream` over SSH. It never listens on the network and only reads: it
//! runs as the SSH user and changes nothing on the server. It stops when
//! Burrow closes its stdin or stops reading.

mod apps;
mod proc;
mod run;
mod units;

use std::collections::BTreeMap;
use std::ffi::CString;
use std::io::{self, Read, Write};
use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime};

use burrow_agent::{Apps, Cpu, Interface, Message, PROTO, Sample, VERSION};

/// How often units, containers and PM2 apps are listed, whatever the
/// sample interval.
const UNITS_EVERY: Duration = Duration::from_secs(10);

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args[..] {
        ["--version"] => {
            println!("burrow-agent {VERSION} proto {PROTO}");
            ExitCode::SUCCESS
        }
        ["stream"] => stream(Duration::from_secs(2)),
        ["stream", "--interval", seconds] => match seconds.parse::<u64>() {
            Ok(seconds @ 1..=60) => stream(Duration::from_secs(seconds)),
            _ => usage(),
        },
        _ => usage(),
    }
}

fn usage() -> ExitCode {
    eprintln!("usage: burrow-agent stream [--interval 1-60] | --version");
    ExitCode::from(2)
}

fn stream(interval: Duration) -> ExitCode {
    // Burrow closing the channel is the signal to stop, even between
    // samples, when no write would notice.
    std::thread::spawn(|| {
        let mut sink = [0; 256];
        while matches!(io::stdin().read(&mut sink), Ok(n) if n > 0) {}
        std::process::exit(0);
    });

    let systemd = units::available();
    let mut out = io::stdout().lock();
    let hello = Message::Hello {
        version: VERSION.to_string(),
        proto: PROTO,
        systemd,
    };
    if send(&mut out, &hello).is_err() {
        return ExitCode::SUCCESS;
    }

    let mut before = Reading::take();
    let mut units_due = Instant::now();
    loop {
        // Units first, so they are there when the first sample is drawn.
        if Instant::now() >= units_due {
            units_due = Instant::now() + UNITS_EVERY;
            if systemd
                && let Some(units) = units::list()
                && send(&mut out, &Message::Units { units }).is_err()
            {
                break;
            }
            let apps = Apps {
                containers: apps::containers(),
                pm2: apps::pm2(),
            };
            if send(&mut out, &Message::Apps(apps)).is_err() {
                break;
            }
        }
        std::thread::sleep(interval);
        let after = Reading::take();
        if send(&mut out, &Message::Sample(after.sample(&before))).is_err() {
            break;
        }
        before = after;
    }
    // Writing failed: nobody is reading any more.
    ExitCode::SUCCESS
}

fn send(out: &mut impl Write, message: &Message) -> io::Result<()> {
    serde_json::to_writer(&mut *out, message)?;
    out.write_all(b"\n")?;
    out.flush()
}

/// The counters that rates are worked out from.
struct Reading {
    at: Instant,
    cpu: proc::CpuTimes,
    cores: u32,
    net: BTreeMap<String, (u64, u64)>,
}

impl Reading {
    fn take() -> Reading {
        let (cpu, cores) = proc::stat(&read("/proc/stat"));
        Reading {
            at: Instant::now(),
            cpu,
            cores,
            net: proc::net_dev(&read("/proc/net/dev")),
        }
    }

    /// Everything since `before`, with the things that aren't rates read
    /// now.
    fn sample(&self, before: &Reading) -> Sample {
        let seconds = self.at.duration_since(before.at).as_secs_f64().max(0.001);
        let net = self
            .net
            .iter()
            .map(|(name, (rx, tx))| {
                let (rx0, tx0) = before.net.get(name).copied().unwrap_or((*rx, *tx));
                Interface {
                    name: name.clone(),
                    rx: rx.saturating_sub(rx0) as f64 / seconds,
                    tx: tx.saturating_sub(tx0) as f64 / seconds,
                }
            })
            .collect();
        let mut disks = proc::mounts(&read("/proc/self/mounts"));
        disks.retain_mut(|disk| match statvfs(&disk.mount) {
            Some((total, available)) if total > 0 => {
                (disk.total, disk.available) = (total, available);
                true
            }
            _ => false,
        });
        Sample {
            time: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as u64),
            uptime: proc::uptime(&read("/proc/uptime")),
            load: proc::loadavg(&read("/proc/loadavg")),
            cpu: Cpu {
                percent: proc::cpu_percent(before.cpu, self.cpu),
                cores: self.cores,
            },
            memory: proc::meminfo(&read("/proc/meminfo")),
            disks,
            net,
        }
    }
}

/// A file's text, or nothing: a reading that fails shows as zero rather
/// than stopping the agent.
fn read(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// A filesystem's size and the space left for unprivileged users, in
/// bytes.
fn statvfs(path: &str) -> Option<(u64, u64)> {
    let path = CString::new(path).ok()?;
    // SAFETY: `path` is NUL-terminated and `stat` is a plain struct the
    // call fills in.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    let block = stat.f_frsize as u64;
    Some((
        (stat.f_blocks as u64).saturating_mul(block),
        (stat.f_bavail as u64).saturating_mul(block),
    ))
}
