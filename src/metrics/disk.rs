//! Disk activity from `/proc/diskstats` for the block device behind `/`.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskCounters {
    pub sectors_read: u64,
    pub sectors_written: u64,
    /// Milliseconds spent doing I/O (field 10 of the stats).
    pub io_ticks_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DiskRates {
    /// Fraction of wall time the device was busy, 0..1.
    pub busy: f32,
    pub read_bps: f64,
    pub write_bps: f64,
}

pub fn parse(diskstats: &str, device: &str) -> Option<DiskCounters> {
    diskstats.lines().find_map(|line| {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 13 || f[2] != device {
            return None;
        }
        Some(DiskCounters {
            sectors_read: f[5].parse().ok()?,
            sectors_written: f[9].parse().ok()?,
            io_ticks_ms: f[12].parse().ok()?,
        })
    })
}

/// Rates between two readings taken `elapsed_ms` apart.
pub fn rates(prev: DiskCounters, cur: DiskCounters, elapsed_ms: f64) -> Option<DiskRates> {
    if elapsed_ms <= 0.0 {
        return None;
    }
    let busy_ms = cur.io_ticks_ms.checked_sub(prev.io_ticks_ms)? as f64;
    let read = cur.sectors_read.checked_sub(prev.sectors_read)? as f64 * 512.0;
    let write = cur.sectors_written.checked_sub(prev.sectors_written)? as f64 * 512.0;
    let secs = elapsed_ms / 1000.0;
    Some(DiskRates {
        busy: (busy_ms / elapsed_ms).clamp(0.0, 1.0) as f32,
        read_bps: read / secs,
        write_bps: write / secs,
    })
}

pub fn read(device: &str) -> Option<DiskCounters> {
    parse(&std::fs::read_to_string("/proc/diskstats").ok()?, device)
}

/// Resolves the configured device: `"auto"` means the whole disk behind `/`.
pub fn resolve_device(configured: &str) -> Option<String> {
    if configured != "auto" {
        return Some(configured.trim_start_matches("/dev/").to_owned());
    }
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    let source = root_mount_source(&mountinfo)?;
    let dev = std::fs::canonicalize(&source).unwrap_or_else(|_| PathBuf::from(&source));
    let name = dev.file_name()?.to_str()?.to_owned();
    Some(parent_disk(Path::new("/sys/class/block"), &name).unwrap_or(name))
}

/// The mount source of `/` from `/proc/self/mountinfo` (field after the ` - ` separator + 1).
fn root_mount_source(mountinfo: &str) -> Option<String> {
    mountinfo.lines().find_map(|line| {
        let mut parts = line.split(" - ");
        let left: Vec<&str> = parts.next()?.split_whitespace().collect();
        let right: Vec<&str> = parts.next()?.split_whitespace().collect();
        (left.get(4) == Some(&"/") && right.get(1)?.starts_with("/dev/"))
            .then(|| right[1].to_owned())
    })
}

/// For a partition (`sda3`, `nvme0n1p2`) returns its disk (`sda`, `nvme0n1`).
fn parent_disk(sys_block: &Path, name: &str) -> Option<String> {
    let entry = sys_block.join(name);
    if !entry.join("partition").exists() {
        return None;
    }
    let real = std::fs::canonicalize(&entry).ok()?;
    Some(real.parent()?.file_name()?.to_str()?.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: &str = "   8       0 sda 1000 0 20000 500 2000 0 40000 900 0 1500 1400 0 0 0 0 0 0\n   8       3 sda3 900 0 18000 450 1900 0 38000 850 0 1400 1300 0 0 0 0\n";
    const T1: &str =
        "   8       0 sda 1100 0 40480 600 2100 0 60480 1000 0 4000 1700 0 0 0 0 0 0\n";

    #[test]
    fn parses_named_device() {
        let c = parse(T0, "sda").unwrap();
        assert_eq!(c.sectors_read, 20000);
        assert_eq!(c.sectors_written, 40000);
        assert_eq!(c.io_ticks_ms, 1500);
        assert_eq!(parse(T0, "sdb"), None);
    }

    #[test]
    fn computes_rates_over_ten_seconds() {
        let r = rates(
            parse(T0, "sda").unwrap(),
            parse(T1, "sda").unwrap(),
            10_000.0,
        )
        .unwrap();
        // io_ticks +2500 ms over 10 s → 25 %; 20480 sectors * 512 B / 10 s = 1 MiB/s
        assert!((r.busy - 0.25).abs() < 1e-6);
        assert!((r.read_bps - 1_048_576.0).abs() < 1e-6);
        assert!((r.write_bps - 1_048_576.0).abs() < 1e-6);
    }

    #[test]
    fn counter_reset_gives_none() {
        assert!(
            rates(
                parse(T1, "sda").unwrap(),
                parse(T0, "sda").unwrap(),
                10_000.0
            )
            .is_none()
        );
    }

    #[test]
    fn finds_root_mount_source() {
        let mi = "22 1 0:21 / /proc rw - proc proc rw\n\
                  29 1 8:3 / / rw,relatime shared:1 - ext4 /dev/sda3 rw\n\
                  30 29 8:1 / /boot rw - vfat /dev/sda1 rw\n";
        assert_eq!(root_mount_source(mi).as_deref(), Some("/dev/sda3"));
        assert_eq!(
            root_mount_source("1 1 0:1 / / rw - overlay overlay rw\n"),
            None
        );
    }

    #[test]
    fn explicit_device_is_used_as_is() {
        assert_eq!(resolve_device("/dev/nvme0n1").as_deref(), Some("nvme0n1"));
        assert_eq!(resolve_device("sdb").as_deref(), Some("sdb"));
    }
}
