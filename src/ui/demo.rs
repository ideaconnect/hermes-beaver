//! Demo mode: a throwaway store filled with made-up tasks, synthetic load and
//! fake health, so every screen state can be checked without touching Hermes.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use serde_json::json;

use crate::health::Health;
use crate::metrics::{self, Metrics, Sample, mem::MemInfo};
use crate::store::{Store, now};
use crate::tools::call_at;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scenario {
    Busy,
    Idle,
    Overflow,
    Broken,
    Empty,
}

impl Scenario {
    pub const ALL: [Scenario; 5] = [
        Scenario::Busy,
        Scenario::Idle,
        Scenario::Overflow,
        Scenario::Broken,
        Scenario::Empty,
    ];

    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "busy" => Self::Busy,
            "idle" => Self::Idle,
            "overflow" => Self::Overflow,
            "broken" => Self::Broken,
            "empty" => Self::Empty,
            other => bail!("unknown scenario `{other}` (busy, idle, overflow, broken, empty)"),
        })
    }
}

/// How long each scenario stays on screen when cycling.
const CYCLE: Duration = Duration::from_secs(20);

pub struct Demo {
    fixed: bool,
    current: usize,
    switched_at: Instant,
    dir: PathBuf,
}

impl Demo {
    /// Creates the temp store and seeds the first scenario.
    pub fn new(fixed: Option<Scenario>) -> Result<(Self, Store)> {
        let dir = std::env::temp_dir().join(format!("hermes-beaver-demo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut store = Store::open(&dir.join("events.db"))?;
        let current = fixed
            .and_then(|f| Scenario::ALL.iter().position(|s| *s == f))
            .unwrap_or(0);
        seed(&mut store, Scenario::ALL[current])?;
        Ok((
            Self {
                fixed: fixed.is_some(),
                current,
                switched_at: Instant::now(),
                dir,
            },
            store,
        ))
    }

    pub fn scenario(&self) -> Scenario {
        Scenario::ALL[self.current]
    }

    /// Moves to the next scenario when it's time; returns whether it did.
    pub fn advance(&mut self, store: &mut Store) -> Result<bool> {
        if self.fixed || self.switched_at.elapsed() < CYCLE {
            return Ok(false);
        }
        self.current = (self.current + 1) % Scenario::ALL.len();
        self.switched_at = Instant::now();
        seed(store, self.scenario())?;
        Ok(true)
    }

    pub fn health(&self) -> Health {
        match self.scenario() {
            Scenario::Broken => Health {
                problems: vec![
                    "Agent runs failing: [blocked_config] provider credential missing".into(),
                ],
            },
            _ => Health::default(),
        }
    }
}

impl Drop for Demo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn seed(store: &mut Store, scenario: Scenario) -> Result<()> {
    store
        .conn
        .execute_batch("DELETE FROM tasks; DELETE FROM queue_items;")?;
    let t = now();
    let min = 60;
    let hour = 3600;

    // History: many finished tasks over the past weeks, for the counter.
    let history = match scenario {
        Scenario::Empty => 0,
        _ => 146,
    };
    for i in 0..history {
        let ts = t - 3 * 86_400 - i * 7 * hour;
        let id = c(
            store,
            ts - 20 * min,
            "task_start",
            json!({"source": "direct", "title": format!("Old task {i}")}),
        )?["id"]
            .clone();
        c(
            store,
            ts,
            "task_finish",
            json!({"id": id, "outcome": "done"}),
        )?;
    }

    let finished = [
        (
            "github",
            "new",
            "Add pagination to the orders endpoint",
            Some("acme/api#131"),
            "done",
            Some("https://github.com/acme/api/pull/133"),
            2 * hour,
        ),
        (
            "telegram",
            "chat",
            "Draft release notes for v2.4",
            None,
            "done",
            None,
            4 * hour,
        ),
        (
            "github",
            "update",
            "Apply review comments on onboarding",
            Some("acme/web#64"),
            "done",
            Some("https://github.com/acme/web/pull/66"),
            5 * hour,
        ),
        (
            "github",
            "new",
            "Bump base image to Ubuntu 26.04",
            Some("acme/infra#12"),
            "failed",
            None,
            7 * hour,
        ),
        (
            "github",
            "new",
            "Migrate to the new billing API",
            Some("acme/api#128"),
            "skipped",
            None,
            9 * hour,
        ),
        (
            "github",
            "new",
            "Document the deploy checklist",
            Some("acme/docs#5"),
            "done",
            Some("https://github.com/acme/docs/pull/6"),
            26 * hour,
        ),
    ];
    let finished_count = match scenario {
        Scenario::Empty => 0,
        Scenario::Idle => 3,
        _ => finished.len(),
    };
    for (source, kind, title, r, outcome, pr, ago) in finished.iter().take(finished_count) {
        let start = t - ago - 25 * min;
        c(
            store,
            start,
            "task_start",
            json!({"source": source, "kind": kind, "title": title, "ref": r}),
        )?;
        let summary = match *outcome {
            "failed" => Some("tests failed twice on CI"),
            "skipped" => Some("which plan tiers should migrate first?"),
            _ => None,
        };
        let target = match r {
            Some(r) => json!({"ref": r}),
            None => json!({"id": store_last_id(store)?}),
        };
        let mut args = target;
        args["outcome"] = json!(outcome);
        if let Some(s) = summary {
            args["summary"] = json!(s);
        }
        if let Some(p) = pr {
            args["pr_url"] = json!(p);
        }
        c(store, t - ago, "task_finish", args)?;
    }

    if matches!(
        scenario,
        Scenario::Busy | Scenario::Overflow | Scenario::Broken
    ) {
        c(
            store,
            t - 12 * min,
            "task_start",
            json!({"source": "github", "kind": "new", "title": "Fix the flaky checkout test on CI", "ref": "acme/api#139", "cron_job_id": "slot-1"}),
        )?;
        c(
            store,
            t - 41 * min,
            "task_start",
            json!({"source": "github", "kind": "update", "title": "Address review: onboarding flow copy", "ref": "acme/web#77", "cron_job_id": "slot-2"}),
        )?;
        c(
            store,
            t - 6 * min,
            "task_update",
            json!({"ref": "acme/web#77", "status": "waiting_feedback", "note": "Should the skip button stay on step 3?"}),
        )?;
        c(
            store,
            t - 3 * min,
            "task_start",
            json!({"source": "telegram", "title": "Summarise yesterday's error logs"}),
        )?;
        c(
            store,
            t - 52 * min,
            "task_start",
            json!({"source": "telegram", "title": "Check disk usage on the backup server"}),
        )?;
        c(
            store,
            t - 2 * min,
            "queue_set",
            json!({"items": [
                {"ref": "acme/api#142", "mode": "new", "title": "Rate-limit the public search endpoint"},
                {"ref": "acme/web#88", "mode": "update", "title": "Dark mode toggle: review comments"},
                {"ref": "acme/cli#19", "mode": "new", "title": "Add --json output to the status command"}
            ]}),
        )?;
    }

    if scenario == Scenario::Overflow {
        for i in 0..6 {
            let r = format!("acme/web#{}", 90 + i);
            c(
                store,
                t - 3 * hour,
                "task_start",
                json!({"source": "github", "kind": "new", "title": format!("Extra task {i}"), "ref": r}),
            )?;
            c(
                store,
                t - 2 * hour + i * min,
                "task_finish",
                json!({"ref": r, "outcome": "done", "pr_url": format!("https://github.com/acme/web/pull/{}", 200 + i)}),
            )?;
        }
    }
    Ok(())
}

fn c(store: &mut Store, ts: i64, name: &str, args: serde_json::Value) -> Result<serde_json::Value> {
    call_at(store, name, args, ts).map_err(|e| anyhow::anyhow!("{name}: {e}"))
}

fn store_last_id(store: &Store) -> Result<i64> {
    Ok(store
        .conn
        .query_row("SELECT MAX(id) FROM tasks", [], |r| r.get(0))?)
}

/// Smooth synthetic load.
pub struct DemoSource {
    t: f32,
}

impl DemoSource {
    pub fn new() -> Self {
        Self { t: 0.0 }
    }
}

impl metrics::Source for DemoSource {
    fn sample(&mut self) -> Sample {
        self.t += 1.0;
        let t = self.t;
        let wobble = |speed: f32, phase: f32| ((t * speed + phase).sin() + 1.0) / 2.0;
        Sample {
            cpu: Some(0.15 + 0.55 * wobble(0.21, 0.0) * wobble(0.07, 1.0)),
            mem: Some(MemInfo {
                total_kb: 7_864_320,
                available_kb: (7_864_320.0 * (0.62 - 0.1 * wobble(0.05, 2.0))) as u64,
            }),
            disk: Some(metrics::disk::DiskRates {
                busy: 0.03 + 0.3 * wobble(0.4, 0.5).powi(6),
                read_bps: 400_000.0 + 3_000_000.0 * f64::from(wobble(0.33, 0.0).powi(4)),
                write_bps: 900_000.0 * f64::from(wobble(0.27, 1.5)),
            }),
            gpu: Some(0.02 + 0.6 * wobble(0.11, 3.0).powi(3)),
        }
    }
}

/// Fills 30 minutes of history so sparklines show something right away.
pub fn prefill(metrics: &mut Metrics, source: &mut DemoSource) {
    for _ in 0..metrics::HISTORY_LEN {
        metrics.record(metrics::Source::sample(source));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_scenario_seeds() {
        for s in Scenario::ALL {
            let (demo, store) = Demo::new(Some(s)).unwrap();
            let snap = store.snapshot(8).unwrap();
            match s {
                Scenario::Empty => assert!(snap.running.is_empty() && snap.finished.is_empty()),
                Scenario::Idle => assert!(snap.running.is_empty() && !snap.finished.is_empty()),
                _ => {
                    assert_eq!(snap.running.len(), 4, "{s:?}");
                    assert_eq!(snap.queue.len(), 3, "{s:?}");
                    assert!(!snap.needs.is_empty(), "{s:?}");
                }
            }
            assert_eq!(demo.health().ok(), s != Scenario::Broken);
            drop(store);
            drop(demo);
        }
    }

    #[test]
    fn parse_scenarios() {
        assert_eq!(Scenario::parse("busy").unwrap(), Scenario::Busy);
        assert!(Scenario::parse("party").is_err());
    }
}
