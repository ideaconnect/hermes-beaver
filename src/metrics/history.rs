//! Fixed-size history of samples for sparklines.

use std::collections::VecDeque;

#[derive(Debug, Clone)]
pub struct History {
    cap: usize,
    buf: VecDeque<f32>,
}

impl History {
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            buf: VecDeque::with_capacity(cap),
        }
    }

    pub fn push(&mut self, v: f32) {
        if self.buf.len() == self.cap {
            self.buf.pop_front();
        }
        self.buf.push_back(v);
    }

    #[cfg(test)]
    pub fn last(&self) -> Option<f32> {
        self.buf.back().copied()
    }

    /// SVG path commands for a sparkline in a 1000x100 viewbox (roughly the 10:1
    /// shape of the row; Slint keeps the viewbox aspect). The newest sample sits
    /// at x=1000; x spacing assumes a full buffer so the line scrolls left.
    pub fn sparkline_path(&self) -> String {
        if self.buf.len() < 2 || self.cap < 2 {
            return String::new();
        }
        let step = 1000.0 / (self.cap - 1) as f32;
        let start = 1000.0 - step * (self.buf.len() - 1) as f32;
        let mut out = String::with_capacity(self.buf.len() * 14);
        for (i, v) in self.buf.iter().enumerate() {
            let x = start + step * i as f32;
            let y = 100.0 - v.clamp(0.0, 1.0) * 100.0;
            out.push_str(if i == 0 { "M " } else { " L " });
            out.push_str(&format!("{x:.2} {y:.2}"));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_oldest_when_full() {
        let mut h = History::new(3);
        for v in [0.1, 0.2, 0.3, 0.4] {
            h.push(v);
        }
        assert_eq!(
            h.buf.iter().copied().collect::<Vec<_>>(),
            vec![0.2, 0.3, 0.4]
        );
        assert_eq!(h.last(), Some(0.4));
    }

    #[test]
    fn sparkline_is_right_aligned() {
        let mut h = History::new(5);
        assert_eq!(h.sparkline_path(), "");
        h.push(0.0);
        h.push(1.0);
        assert_eq!(h.sparkline_path(), "M 750.00 100.00 L 1000.00 0.00");
    }
}
