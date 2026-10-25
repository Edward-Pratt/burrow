//! Reading the files in `/proc`. Each parser takes the file's text, so the
//! tests can feed it saved copies.

use std::collections::BTreeMap;

use burrow_agent::{Disk, Memory};

/// CPU time from the first line of `/proc/stat`, in clock ticks.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CpuTimes {
    pub busy: u64,
    pub total: u64,
}

/// The aggregate CPU line and the number of per-core lines.
pub fn stat(text: &str) -> (CpuTimes, u32) {
    let mut times = CpuTimes::default();
    let mut cores = 0;
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        match fields.next() {
            Some("cpu") => {
                // user nice system idle iowait irq softirq steal. Guest time
                // is already counted in user and nice, so it's left out.
                let values: Vec<u64> = fields.take(8).map(|f| f.parse().unwrap_or(0)).collect();
                let idle =
                    values.get(3).copied().unwrap_or(0) + values.get(4).copied().unwrap_or(0);
                times.total = values.iter().sum();
                times.busy = times.total - idle;
            }
            Some(name) if name.starts_with("cpu") => cores += 1,
            _ => {}
        }
    }
    (times, cores)
}

/// Busy time between two readings, as a percentage.
pub fn cpu_percent(before: CpuTimes, after: CpuTimes) -> f64 {
    let total = after.total.saturating_sub(before.total);
    if total == 0 {
        return 0.0;
    }
    let busy = after.busy.saturating_sub(before.busy).min(total);
    busy as f64 * 100.0 / total as f64
}

pub fn meminfo(text: &str) -> Memory {
    let mut memory = Memory::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        // The file says kB and means KiB.
        let bytes = value
            .trim()
            .trim_end_matches("kB")
            .trim()
            .parse::<u64>()
            .unwrap_or(0)
            * 1024;
        match key {
            "MemTotal" => memory.total = bytes,
            "MemAvailable" => memory.available = bytes,
            "SwapTotal" => memory.swap_total = bytes,
            "SwapFree" => memory.swap_free = bytes,
            _ => {}
        }
    }
    memory
}

pub fn loadavg(text: &str) -> [f64; 3] {
    let mut load = [0.0; 3];
    for (slot, field) in load.iter_mut().zip(text.split_whitespace()) {
        *slot = field.parse().unwrap_or(0.0);
    }
    load
}

pub fn uptime(text: &str) -> f64 {
    text.split_whitespace()
        .next()
        .and_then(|f| f.parse().ok())
        .unwrap_or(0.0)
}

/// Bytes received and sent by each interface but loopback and container
/// veth pairs. A veth's traffic also crosses its bridge, and a host with
/// many containers would otherwise send hundreds of them.
pub fn net_dev(text: &str) -> BTreeMap<String, (u64, u64)> {
    let mut counters = BTreeMap::new();
    // Two header lines, then `name: rx_bytes … (8 rx fields) tx_bytes …`.
    for line in text.lines().skip(2) {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        if name == "lo" || name.starts_with("veth") {
            continue;
        }
        let fields: Vec<u64> = rest
            .split_whitespace()
            .map(|f| f.parse().unwrap_or(0))
            .collect();
        if let (Some(rx), Some(tx)) = (fields.first(), fields.get(8)) {
            counters.insert(name.to_string(), (*rx, *tx));
        }
    }
    counters
}

/// The filesystems worth showing, from `/proc/self/mounts`, without sizes.
///
/// Only those backed by a device are kept. Network filesystems are left
/// out on purpose: `statvfs` on a hung NFS mount never returns.
pub fn mounts(text: &str) -> Vec<Disk> {
    let mut seen = Vec::new();
    let mut disks = Vec::new();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let (Some(source), Some(mount), Some(fs)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let backed = source.starts_with("/dev/") || fs == "zfs";
        // Snap and image mounts are read-only and always full.
        if !backed || matches!(fs, "squashfs" | "iso9660" | "erofs") {
            continue;
        }
        // Bind mounts and btrfs subvolumes show the same device again.
        if seen.contains(&source) {
            continue;
        }
        seen.push(source);
        disks.push(Disk {
            mount: unescape(mount),
            fs: fs.to_string(),
            ..Disk::default()
        });
    }
    disks
}

/// Undoes the octal escapes (`\040` for a space) the kernel writes for
/// whitespace and backslashes in mount points.
fn unescape(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let digits = bytes.get(i + 1..i + 4);
        let octal = digits
            .filter(|d| bytes[i] == b'\\' && d.iter().all(|b| (b'0'..=b'7').contains(b)))
            .map(|d| d.iter().fold(0u32, |n, b| n * 8 + u32::from(b - b'0')));
        match octal.and_then(|n| u8::try_from(n).ok()) {
            Some(byte) => {
                out.push(byte);
                i += 4;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_counts_busy_time_and_cores() {
        let text = "cpu  100 5 50 800 40 3 2 0 7 0\n\
                    cpu0 50 2 25 400 20 1 1 0 3 0\n\
                    cpu1 50 3 25 400 20 2 1 0 4 0\n\
                    intr 12345\n";
        let (times, cores) = stat(text);
        assert_eq!(cores, 2);
        assert_eq!(times.total, 1000);
        assert_eq!(times.busy, 160);
    }

    #[test]
    fn cpu_percent_is_the_busy_share_of_the_interval() {
        let before = CpuTimes {
            busy: 100,
            total: 1000,
        };
        let after = CpuTimes {
            busy: 150,
            total: 1200,
        };
        assert_eq!(cpu_percent(before, after), 25.0);
        // A counter that went backwards (a wrong reading) is not a crash.
        assert_eq!(cpu_percent(after, before), 0.0);
    }

    #[test]
    fn meminfo_reads_bytes() {
        let text = "MemTotal:       16000000 kB\n\
                    MemFree:         1000000 kB\n\
                    MemAvailable:    8000000 kB\n\
                    SwapTotal:       2000000 kB\n\
                    SwapFree:        1500000 kB\n";
        let memory = meminfo(text);
        assert_eq!(memory.total, 16_000_000 * 1024);
        assert_eq!(memory.available, 8_000_000 * 1024);
        assert_eq!(memory.swap_total, 2_000_000 * 1024);
        assert_eq!(memory.swap_free, 1_500_000 * 1024);
    }

    #[test]
    fn loadavg_and_uptime() {
        assert_eq!(loadavg("0.52 0.58 0.59 1/389 12345\n"), [0.52, 0.58, 0.59]);
        assert_eq!(uptime("350735.47 234388.90\n"), 350735.47);
        assert_eq!(uptime(""), 0.0);
    }

    #[test]
    fn net_dev_skips_loopback_and_veths() {
        let text = "Inter-|   Receive                                                |  Transmit\n \
             face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n    \
             lo: 1000 10 0 0 0 0 0 0 1000 10 0 0 0 0 0 0\n  \
             veth1a2b: 1 1 0 0 0 0 0 0 1 1 0 0 0 0 0 0\n  \
             eth0: 5000 50 0 0 0 0 0 0 7000 70 0 0 0 0 0 0\n";
        let counters = net_dev(text);
        assert_eq!(counters.len(), 1);
        assert_eq!(counters["eth0"], (5000, 7000));
    }

    #[test]
    fn mounts_keeps_devices_once() {
        let text = "proc /proc proc rw 0 0\n\
                    /dev/nvme0n1p2 / btrfs rw 0 0\n\
                    /dev/nvme0n1p2 /home btrfs rw 0 0\n\
                    /dev/nvme0n1p1 /boot vfat rw 0 0\n\
                    /dev/loop0 /snap/core squashfs ro 0 0\n\
                    server:/export /mnt/nfs nfs4 rw 0 0\n\
                    tank/data /tank zfs rw 0 0\n\
                    /dev/sdb1 /mnt/my\\040disk ext4 rw 0 0\n";
        let mounts: Vec<_> = mounts(text).into_iter().map(|d| d.mount).collect();
        assert_eq!(mounts, ["/", "/boot", "/tank", "/mnt/my disk"]);
    }

    #[test]
    fn unescape_leaves_other_backslashes() {
        assert_eq!(unescape("a\\134b"), "a\\b");
        assert_eq!(unescape("a\\9b"), "a\\9b");
        assert_eq!(unescape("end\\04"), "end\\04");
    }
}
