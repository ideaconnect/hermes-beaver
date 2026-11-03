//! Whole-GPU busy readings from sysfs, readable without privileges.
//!
//! fdinfo only shows clients we may inspect; on X11 the root-owned Xorg does much
//! of the render work (55-75 % under load in our measurements), so a per-client
//! sum reads far too low. These kernel counters cover every client. They're tried
//! in order; fdinfo stays the fallback when none exists.
//!
//! - amdgpu: `device/gpu_busy_percent` (instant, 0-100)
//! - i915: `gt/gt0/rc6_residency_ms` or `power/rc6_residency_ms` (time spent idle)
//! - xe: `device/tile0/gt0/gtidle/idle_residency_ms` (time spent idle)

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TotalSource {
    BusyPercent(PathBuf),
    IdleResidencyMs(PathBuf),
}

/// Finds the first usable counter under `drm_root` (normally `/sys/class/drm`).
pub fn probe(drm_root: &Path) -> Option<TotalSource> {
    let mut cards: Vec<PathBuf> = std::fs::read_dir(drm_root)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("card") && !n.contains('-'))
        })
        .collect();
    cards.sort();
    for card in cards {
        let busy = card.join("device/gpu_busy_percent");
        if read_u64(&busy).is_some() {
            return Some(TotalSource::BusyPercent(busy));
        }
        for rel in [
            "gt/gt0/rc6_residency_ms",
            "power/rc6_residency_ms",
            "device/tile0/gt0/gtidle/idle_residency_ms",
        ] {
            let p = card.join(rel);
            if read_u64(&p).is_some() {
                return Some(TotalSource::IdleResidencyMs(p));
            }
        }
    }
    None
}

pub fn read_u64(path: &Path) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Busy fraction from two idle-residency readings taken `elapsed_ms` apart.
pub fn busy_from_idle(prev_idle_ms: u64, cur_idle_ms: u64, elapsed_ms: f64) -> Option<f32> {
    if elapsed_ms <= 0.0 {
        return None;
    }
    let idle = cur_idle_ms.checked_sub(prev_idle_ms)? as f64;
    Some((1.0 - idle / elapsed_ms).clamp(0.0, 1.0) as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_drm(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("hb-drm-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (rel, content) in files {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, content).unwrap();
        }
        root
    }

    #[test]
    fn prefers_amd_busy_percent() {
        let root = fake_drm("amd", &[("card1/device/gpu_busy_percent", "37\n")]);
        assert_eq!(
            probe(&root),
            Some(TotalSource::BusyPercent(
                root.join("card1/device/gpu_busy_percent")
            ))
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn finds_i915_rc6_and_skips_connectors() {
        let root = fake_drm(
            "i915",
            &[
                ("card0-HDMI-A-1/status", "connected\n"),
                ("card0/gt/gt0/rc6_residency_ms", "123456\n"),
            ],
        );
        assert_eq!(
            probe(&root),
            Some(TotalSource::IdleResidencyMs(
                root.join("card0/gt/gt0/rc6_residency_ms")
            ))
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn nothing_found() {
        let root = fake_drm("none", &[("card0/device/vendor", "0x1234\n")]);
        assert_eq!(probe(&root), None);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn idle_residency_to_busy() {
        // 10 s window, 7 s idle → 30 % busy.
        let b = busy_from_idle(1000, 8000, 10_000.0).unwrap();
        assert!((b - 0.3).abs() < 1e-6);
        // Idle counter can run slightly ahead of wall time: clamp at 0.
        assert_eq!(busy_from_idle(0, 10_050, 10_000.0), Some(0.0));
        assert_eq!(busy_from_idle(10, 5, 10_000.0), None);
    }
}
