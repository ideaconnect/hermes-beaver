//! System load sampling: CPU, memory, disk and GPU, averaged over each tick.

pub mod cpu;
pub mod disk;
pub mod gpu;
pub mod gpu_total;
pub mod history;
pub mod mem;

use std::path::Path;
use std::time::Instant;

use history::History;

/// 30 minutes of 10-second samples.
pub const HISTORY_LEN: usize = 180;

/// One reading of everything, each value averaged since the previous tick.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sample {
    pub cpu: Option<f32>,
    pub mem: Option<mem::MemInfo>,
    pub disk: Option<disk::DiskRates>,
    /// `None` when the machine has no DRM device.
    pub gpu: Option<f32>,
}

/// Something that produces samples: the real system or the demo generator.
pub trait Source {
    fn sample(&mut self) -> Sample;
}

/// How the GPU gauge is fed on this machine.
enum GpuReader {
    None,
    /// sysfs counter covering all clients (preferred).
    Total {
        source: gpu_total::TotalSource,
        prev_idle_ms: Option<u64>,
    },
    /// Per-client fdinfo; misses clients we can't inspect (e.g. root's Xorg).
    Fdinfo {
        prev: gpu::Clients,
    },
}

/// Reads the real system from /proc and /sys.
pub struct SystemSource {
    disk_device: Option<String>,
    gpu: GpuReader,
    prev_at: Instant,
    prev_cpu: Option<cpu::CpuTimes>,
    prev_disk: Option<disk::DiskCounters>,
}

impl SystemSource {
    /// Takes the baseline readings; the first `sample()` reports averages since now.
    pub fn new(disk_setting: &str) -> Self {
        let disk_device = disk::resolve_device(disk_setting);
        let gpu = if !gpu::available() {
            GpuReader::None
        } else if let Some(source) = gpu_total::probe(Path::new("/sys/class/drm")) {
            let prev_idle_ms = match &source {
                gpu_total::TotalSource::IdleResidencyMs(p) => gpu_total::read_u64(p),
                gpu_total::TotalSource::BusyPercent(_) => None,
            };
            GpuReader::Total {
                source,
                prev_idle_ms,
            }
        } else {
            GpuReader::Fdinfo {
                prev: gpu::snapshot(),
            }
        };
        Self {
            prev_disk: disk_device.as_deref().and_then(disk::read),
            disk_device,
            gpu,
            prev_at: Instant::now(),
            prev_cpu: cpu::read(),
        }
    }

    fn sample_gpu(&mut self, elapsed_ms: f64) -> Option<f32> {
        match &mut self.gpu {
            GpuReader::None => None,
            GpuReader::Total {
                source: gpu_total::TotalSource::BusyPercent(p),
                ..
            } => Some((gpu_total::read_u64(p)? as f32 / 100.0).clamp(0.0, 1.0)),
            GpuReader::Total {
                source: gpu_total::TotalSource::IdleResidencyMs(p),
                prev_idle_ms,
            } => {
                let cur = gpu_total::read_u64(p);
                let busy = match (*prev_idle_ms, cur) {
                    (Some(a), Some(b)) => gpu_total::busy_from_idle(a, b, elapsed_ms),
                    _ => None,
                };
                *prev_idle_ms = cur;
                busy
            }
            GpuReader::Fdinfo { prev } => {
                let cur = gpu::snapshot();
                let v = gpu::busy(prev, &cur, (elapsed_ms * 1e6) as u64);
                *prev = cur;
                Some(v)
            }
        }
    }
}

impl Source for SystemSource {
    fn sample(&mut self) -> Sample {
        let now = Instant::now();
        let elapsed = now.duration_since(self.prev_at);
        self.prev_at = now;

        let cur_cpu = cpu::read();
        let cpu = match (self.prev_cpu, cur_cpu) {
            (Some(a), Some(b)) => cpu::usage(a, b),
            _ => None,
        };
        self.prev_cpu = cur_cpu;

        let cur_disk = self.disk_device.as_deref().and_then(disk::read);
        let disk = match (self.prev_disk, cur_disk) {
            (Some(a), Some(b)) => disk::rates(a, b, elapsed.as_secs_f64() * 1000.0),
            _ => None,
        };
        self.prev_disk = cur_disk;

        let gpu = self.sample_gpu(elapsed.as_secs_f64() * 1000.0);

        Sample {
            cpu,
            mem: mem::read(),
            disk,
            gpu,
        }
    }
}

/// Histories for every gauge, fed by a [`Source`].
pub struct Metrics {
    pub cpu: History,
    pub mem: History,
    pub disk: History,
    pub gpu: History,
    pub latest: Sample,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            cpu: History::new(HISTORY_LEN),
            mem: History::new(HISTORY_LEN),
            disk: History::new(HISTORY_LEN),
            gpu: History::new(HISTORY_LEN),
            latest: Sample::default(),
        }
    }
}

impl Metrics {
    pub fn record(&mut self, s: Sample) {
        if let Some(v) = s.cpu {
            self.cpu.push(v);
        }
        if let Some(m) = s.mem {
            self.mem.push(m.used_fraction());
        }
        if let Some(d) = s.disk {
            self.disk.push(d.busy);
        }
        if let Some(g) = s.gpu {
            self.gpu.push(g);
        }
        self.latest = s;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_skips_missing_values() {
        let mut m = Metrics::default();
        m.record(Sample {
            cpu: Some(0.5),
            mem: None,
            disk: None,
            gpu: None,
        });
        assert_eq!(m.cpu.last(), Some(0.5));
        assert_eq!(m.mem.last(), None);
        assert_eq!(m.gpu.last(), None);
    }

    #[test]
    fn system_source_samples_this_machine() {
        let mut src = SystemSource::new("auto");
        std::thread::sleep(std::time::Duration::from_millis(200));
        let s = src.sample();
        assert!(s.mem.is_some(), "meminfo should be readable");
        if let Some(c) = s.cpu {
            assert!((0.0..=1.0).contains(&c));
        }
    }
}
