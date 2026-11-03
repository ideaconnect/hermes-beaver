//! GPU busy fraction from DRM fdinfo (`/proc/<pid>/fdinfo/<fd>`), for any vendor.
//!
//! Every process holding a `/dev/dri/*` fd exposes per-client engine usage. Two
//! formats exist (see the kernel's drm-usage-stats documentation):
//! - nanoseconds: `drm-engine-<class>: <ns> ns`, optionally `drm-engine-capacity-<class>`;
//! - cycles (Intel xe): `drm-cycles-<class>` and `drm-total-cycles-<class>`.
//!
//! Only processes we may inspect are visible (our own user's, without extra privileges).

use std::collections::HashMap;
use std::path::Path;

/// Cumulative usage of one engine class for one DRM client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineCounter {
    Ns { busy_ns: u64, capacity: u64 },
    Cycles { cycles: u64, total_cycles: u64 },
}

/// One DRM client: `(pdev, client-id)` → engine class → counter.
pub type Clients = HashMap<(String, String), HashMap<String, EngineCounter>>;

/// Parses one fdinfo file. Returns the client key and its engines, or `None` for
/// fds that aren't DRM clients or carry no engine stats.
pub fn parse_fdinfo(text: &str) -> Option<((String, String), HashMap<String, EngineCounter>)> {
    let mut client_id = None;
    let mut pdev = String::new();
    let mut ns: HashMap<String, u64> = HashMap::new();
    let mut capacity: HashMap<String, u64> = HashMap::new();
    let mut cycles: HashMap<String, u64> = HashMap::new();
    let mut total: HashMap<String, u64> = HashMap::new();

    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        let num = || value.split_whitespace().next()?.parse::<u64>().ok();
        if key == "drm-client-id" {
            client_id = Some(value.to_owned());
        } else if key == "drm-pdev" {
            pdev = value.to_owned();
        } else if let Some(class) = key.strip_prefix("drm-engine-capacity-") {
            if let Some(n) = num() {
                capacity.insert(class.to_owned(), n);
            }
        } else if let Some(class) = key.strip_prefix("drm-engine-") {
            if value.ends_with("ns")
                && let Some(n) = num()
            {
                ns.insert(class.to_owned(), n);
            }
        } else if let Some(class) = key.strip_prefix("drm-total-cycles-") {
            if let Some(n) = num() {
                total.insert(class.to_owned(), n);
            }
        } else if let Some(class) = key.strip_prefix("drm-cycles-")
            && let Some(n) = num()
        {
            cycles.insert(class.to_owned(), n);
        }
    }

    let mut engines: HashMap<String, EngineCounter> = ns
        .into_iter()
        .map(|(class, busy_ns)| {
            let capacity = capacity.get(&class).copied().unwrap_or(1).max(1);
            (class, EngineCounter::Ns { busy_ns, capacity })
        })
        .collect();
    for (class, c) in cycles {
        if let Some(&t) = total.get(&class) {
            engines.insert(
                class,
                EngineCounter::Cycles {
                    cycles: c,
                    total_cycles: t,
                },
            );
        }
    }
    if engines.is_empty() {
        return None;
    }
    Some(((pdev, client_id?), engines))
}

/// Busiest engine class between two snapshots taken `elapsed_ns` apart, 0..1.
/// Clients that appeared or vanished in between are ignored.
pub fn busy(prev: &Clients, cur: &Clients, elapsed_ns: u64) -> f32 {
    if elapsed_ns == 0 {
        return 0.0;
    }
    let mut ns_busy: HashMap<&str, f64> = HashMap::new();
    let mut cycles_busy: HashMap<&str, (u64, u64)> = HashMap::new();
    for (key, engines) in cur {
        let Some(before) = prev.get(key) else {
            continue;
        };
        for (class, counter) in engines {
            match (before.get(class), counter) {
                (
                    Some(EngineCounter::Ns { busy_ns: a, .. }),
                    EngineCounter::Ns {
                        busy_ns: b,
                        capacity,
                    },
                ) => {
                    let d = b.saturating_sub(*a) as f64 / *capacity as f64;
                    *ns_busy.entry(class).or_default() += d;
                }
                (
                    Some(EngineCounter::Cycles {
                        cycles: c0,
                        total_cycles: t0,
                    }),
                    EngineCounter::Cycles {
                        cycles: c1,
                        total_cycles: t1,
                    },
                ) => {
                    let e = cycles_busy.entry(class).or_default();
                    e.0 += c1.saturating_sub(*c0);
                    // The total-cycles clock is GPU-wide; take the largest delta seen.
                    e.1 = e.1.max(t1.saturating_sub(*t0));
                }
                _ => {}
            }
        }
    }
    let from_ns = ns_busy.values().map(|d| d / elapsed_ns as f64);
    let from_cycles = cycles_busy
        .values()
        .filter(|(_, t)| *t > 0)
        .map(|(c, t)| *c as f64 / *t as f64);
    from_ns
        .chain(from_cycles)
        .fold(0.0_f64, f64::max)
        .clamp(0.0, 1.0) as f32
}

/// Whether this machine has any DRM device at all (otherwise the gauge is hidden).
pub fn available() -> bool {
    Path::new("/dev/dri").is_dir()
}

/// Reads all visible DRM clients, skipping our own process.
pub fn snapshot() -> Clients {
    let own = std::process::id().to_string();
    let mut clients = Clients::new();
    let Ok(procs) = std::fs::read_dir("/proc") else {
        return clients;
    };
    for proc_entry in procs.flatten() {
        let pid = proc_entry.file_name();
        let Some(pid) = pid.to_str() else { continue };
        if !pid.bytes().all(|b| b.is_ascii_digit()) || pid == own {
            continue;
        }
        let Ok(fds) = std::fs::read_dir(proc_entry.path().join("fd")) else {
            continue; // not ours to inspect
        };
        for fd in fds.flatten() {
            let Ok(target) = std::fs::read_link(fd.path()) else {
                continue;
            };
            if !target.starts_with("/dev/dri/") {
                continue;
            }
            let info = proc_entry.path().join("fdinfo").join(fd.file_name());
            if let Some((key, engines)) = std::fs::read_to_string(info)
                .ok()
                .as_deref()
                .and_then(parse_fdinfo)
            {
                clients.insert(key, engines);
            }
        }
    }
    clients
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ns_client(id: &str, render_ns: u64) -> String {
        format!(
            "pos:\t0\nflags:\t02100002\ndrm-driver:\ti915\ndrm-pdev:\t0000:00:02.0\n\
             drm-client-id:\t{id}\ndrm-engine-render:\t{render_ns} ns\n\
             drm-engine-copy:\t0 ns\ndrm-engine-capacity-video:\t2\ndrm-engine-video:\t0 ns\n"
        )
    }

    fn clients(files: &[String]) -> Clients {
        files.iter().filter_map(|f| parse_fdinfo(f)).collect()
    }

    #[test]
    fn parses_ns_format_with_capacity() {
        let (key, engines) = parse_fdinfo(&ns_client("7", 500)).unwrap();
        assert_eq!(key, ("0000:00:02.0".into(), "7".into()));
        assert_eq!(
            engines["render"],
            EngineCounter::Ns {
                busy_ns: 500,
                capacity: 1
            }
        );
        assert_eq!(
            engines["video"],
            EngineCounter::Ns {
                busy_ns: 0,
                capacity: 2
            }
        );
    }

    #[test]
    fn parses_xe_cycles_format() {
        let text = "drm-driver:\txe\ndrm-pdev:\t0000:03:00.0\ndrm-client-id:\t42\n\
                    drm-cycles-rcs:\t1000\ndrm-total-cycles-rcs:\t50000\n\
                    drm-cycles-bcs:\t0\ndrm-total-cycles-bcs:\t50000\n";
        let (_, engines) = parse_fdinfo(text).unwrap();
        assert_eq!(
            engines["rcs"],
            EngineCounter::Cycles {
                cycles: 1000,
                total_cycles: 50000
            }
        );
    }

    #[test]
    fn non_drm_fd_is_ignored() {
        assert!(parse_fdinfo("pos:\t0\nflags:\t02\nmnt_id:\t25\n").is_none());
    }

    #[test]
    fn busy_sums_clients_per_engine_and_takes_the_busiest() {
        let s = 1_000_000_000u64;
        let prev = clients(&[ns_client("1", 0), ns_client("2", 0)]);
        // Over 10 s: client 1 render busy 3 s, client 2 render busy 2 s → 50 %.
        let cur = clients(&[ns_client("1", 3 * s), ns_client("2", 2 * s)]);
        let b = busy(&prev, &cur, 10 * s);
        assert!((b - 0.5).abs() < 1e-6, "{b}");
    }

    #[test]
    fn new_clients_are_ignored_until_seen_twice() {
        let s = 1_000_000_000u64;
        let prev = clients(&[ns_client("1", 0)]);
        let cur = clients(&[ns_client("1", s), ns_client("9", 500 * s)]);
        let b = busy(&prev, &cur, 10 * s);
        assert!((b - 0.1).abs() < 1e-6, "{b}");
    }

    #[test]
    fn cycles_busy_uses_total_cycles() {
        let a = "drm-pdev:\tp\ndrm-client-id:\t1\ndrm-cycles-rcs:\t0\ndrm-total-cycles-rcs:\t0\n";
        let b =
            "drm-pdev:\tp\ndrm-client-id:\t1\ndrm-cycles-rcs:\t250\ndrm-total-cycles-rcs:\t1000\n";
        let v = busy(
            &clients(&[a.to_owned()]),
            &clients(&[b.to_owned()]),
            1_000_000,
        );
        assert!((v - 0.25).abs() < 1e-6, "{v}");
    }

    #[test]
    fn real_i915_fixtures() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fdinfo/");
        let read = |n: &str| std::fs::read_to_string(format!("{dir}{n}")).unwrap();
        let t0 = clients(&[read("i915-glxgears-t0.txt")]);
        let t10 = clients(&[read("i915-glxgears-t10.txt")]);
        // Measured on the target: Δrender 4,479,429,126 ns over 10.01 s → 44.75 %.
        let b = busy(&t0, &t10, 10_010_000_000);
        assert!((b - 0.4475).abs() < 0.001, "{b}");
        let (_, idle) = parse_fdinfo(&read("i915-idle.txt")).unwrap();
        assert!(idle.contains_key("render"));
    }

    #[test]
    fn result_is_clamped() {
        let s = 1_000_000_000u64;
        let prev = clients(&[ns_client("1", 0)]);
        let cur = clients(&[ns_client("1", 20 * s)]);
        assert_eq!(busy(&prev, &cur, 10 * s), 1.0);
    }
}
