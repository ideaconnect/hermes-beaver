//! CPU utilisation from the aggregate line of `/proc/stat`.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuTimes {
    /// idle + iowait
    pub idle: u64,
    /// user + nice + system + idle + iowait + irq + softirq + steal
    pub total: u64,
}

pub fn parse(stat: &str) -> Option<CpuTimes> {
    let line = stat.lines().find(|l| l.starts_with("cpu "))?;
    let v: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .take(8)
        .map(|f| f.parse().ok())
        .collect::<Option<_>>()?;
    if v.len() < 5 {
        return None;
    }
    let idle = v[3] + v[4];
    Some(CpuTimes {
        idle,
        total: v.iter().sum(),
    })
}

/// Busy fraction between two readings, `None` if no time passed.
pub fn usage(prev: CpuTimes, cur: CpuTimes) -> Option<f32> {
    let total = cur.total.checked_sub(prev.total)?;
    let idle = cur.idle.checked_sub(prev.idle)?;
    if total == 0 {
        return None;
    }
    Some((1.0 - idle as f64 / total as f64).clamp(0.0, 1.0) as f32)
}

pub fn read() -> Option<CpuTimes> {
    parse(&std::fs::read_to_string("/proc/stat").ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: &str = "cpu  100 0 50 800 50 0 0 0 0 0\ncpu0 50 0 25 400 25 0 0 0 0 0\nintr 1\n";
    const T1: &str = "cpu  160 0 70 880 70 10 10 0 0 0\ncpu0 80 0 35 440 35 5 5 0 0 0\n";

    #[test]
    fn parses_aggregate_line() {
        assert_eq!(
            parse(T0),
            Some(CpuTimes {
                idle: 850,
                total: 1000
            })
        );
    }

    #[test]
    fn usage_counts_iowait_as_idle() {
        // Δtotal = 1200-1000 = 200, Δidle = 950-850 = 100 → 50 % busy
        let u = usage(parse(T0).unwrap(), parse(T1).unwrap()).unwrap();
        assert!((u - 0.5).abs() < 1e-6, "{u}");
    }

    #[test]
    fn no_elapsed_time_gives_none() {
        let t = parse(T0).unwrap();
        assert_eq!(usage(t, t), None);
    }

    #[test]
    fn garbage_gives_none() {
        assert_eq!(parse("intr 1 2 3\n"), None);
        assert_eq!(parse("cpu  a b c d e\n"), None);
    }
}
