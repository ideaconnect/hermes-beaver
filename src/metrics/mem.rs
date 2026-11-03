//! Memory use from `/proc/meminfo`.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemInfo {
    pub total_kb: u64,
    pub available_kb: u64,
}

impl MemInfo {
    pub fn used_fraction(&self) -> f32 {
        if self.total_kb == 0 {
            return 0.0;
        }
        (1.0 - self.available_kb as f64 / self.total_kb as f64).clamp(0.0, 1.0) as f32
    }

    pub fn used_kb(&self) -> u64 {
        self.total_kb.saturating_sub(self.available_kb)
    }
}

pub fn parse(text: &str) -> Option<MemInfo> {
    let field = |name: &str| -> Option<u64> {
        text.lines()
            .find_map(|l| l.strip_prefix(name)?.strip_prefix(':'))?
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    };
    Some(MemInfo {
        total_kb: field("MemTotal")?,
        available_kb: field("MemAvailable")?,
    })
}

pub fn read() -> Option<MemInfo> {
    parse(&std::fs::read_to_string("/proc/meminfo").ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "MemTotal:        7864320 kB\nMemFree:          312000 kB\nMemAvailable:    5242880 kB\nBuffers:           1 kB\n";

    #[test]
    fn parses_and_computes_used() {
        let m = parse(SAMPLE).unwrap();
        assert_eq!(m.total_kb, 7_864_320);
        assert_eq!(m.available_kb, 5_242_880);
        assert_eq!(m.used_kb(), 2_621_440);
        assert!((m.used_fraction() - 1.0 / 3.0).abs() < 1e-4);
    }

    #[test]
    fn missing_field_gives_none() {
        assert_eq!(parse("MemTotal: 10 kB\n"), None);
    }
}
