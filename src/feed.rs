//! Where both views get their data: the event store, Hermes's own files and the
//! system load, or the demo's made-up versions of them. The kiosk and the web view
//! each run one [`Feed`] on a 10 s tick.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::Result;

use crate::config::Config;
use crate::demo::{self, Demo};
use crate::health::{self, Health};
use crate::hermes::HermesReader;
use crate::metrics::{self, Metrics, Source};
use crate::reconcile;
use crate::store::Store;
use crate::view::State;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Live,
    /// Made-up data; `None` cycles through every scenario.
    Demo(Option<String>),
}

/// What a feed may do to the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Opens (and migrates) the store and reconciles: the kiosk.
    ReadWrite,
    /// Never writes, so it never races the kiosk: the web view.
    ReadOnly,
}

pub struct Feed {
    config: Config,
    access: Access,
    store: Option<Store>,
    store_path: PathBuf,
    store_error: Option<String>,
    db_stamp: Option<(Option<SystemTime>, Option<SystemTime>)>,
    hermes: Option<HermesReader>,
    demo: Option<Demo>,
    source: Box<dyn Source>,
    refreshed: bool,
    pub state: State,
}

impl Feed {
    pub fn new(config: &Config, mode: &Mode, store_path: &Path, access: Access) -> Result<Self> {
        Ok(match mode {
            Mode::Live => {
                let mut feed = Self {
                    config: config.clone(),
                    access,
                    store: None,
                    store_path: store_path.to_path_buf(),
                    store_error: None,
                    db_stamp: None,
                    hermes: Some(HermesReader::new(
                        config.hermes_home(),
                        config.hermes.agent_jobs.clone(),
                    )),
                    demo: None,
                    source: Box::new(metrics::SystemSource::new(&config.metrics.disk)),
                    refreshed: false,
                    state: State {
                        gpu_shown: metrics::gpu::available(),
                        ..Default::default()
                    },
                };
                feed.open_store();
                feed
            }
            Mode::Demo(scenario) => {
                let fixed = scenario.as_deref().map(demo::Scenario::parse).transpose()?;
                let (demo, store) = Demo::new(fixed)?;
                let mut source = demo::DemoSource::new();
                let mut metrics = Metrics::default();
                demo::prefill(&mut metrics, &mut source);
                Self {
                    config: config.clone(),
                    access,
                    store: Some(store),
                    store_path: PathBuf::new(),
                    store_error: None,
                    db_stamp: None,
                    hermes: None,
                    demo: Some(demo),
                    source: Box::new(source),
                    refreshed: false,
                    state: State {
                        metrics,
                        gpu_shown: true,
                        ..Default::default()
                    },
                }
            }
        })
    }

    pub fn store(&self) -> Option<&Store> {
        self.store.as_ref()
    }

    pub fn sample_metrics(&mut self) {
        let s = self.source.sample();
        self.state.metrics.record(s);
    }

    /// Re-reads health and, when it changed, the store. Returns whether the health
    /// changed (never on the first call).
    pub fn refresh(&mut self, now: i64) -> bool {
        // A store that didn't open (not created yet, say) is retried on every tick.
        if self.store.is_none() && self.demo.is_none() {
            self.open_store();
        }
        let mut force_reload = false;

        // Health (and reconciliation, which needs the same ledger).
        let mut health = if let Some(demo) = &mut self.demo {
            if let Some(store) = &mut self.store {
                match demo.advance(store) {
                    Ok(true) => force_reload = true,
                    Ok(false) => {}
                    Err(e) => eprintln!("hermes-beaver: demo: {e:#}"),
                }
            }
            demo.health()
        } else if let Some(hermes) = &mut self.hermes {
            let st = hermes.poll();
            if self.access == Access::ReadWrite
                && let Some(store) = &mut self.store
            {
                match reconcile::reconcile(store, &st.executions, now) {
                    Ok(0) => {}
                    Ok(n) => {
                        eprintln!("hermes-beaver: reconciled {n} task(s) from the cron ledger");
                        force_reload = true;
                    }
                    Err(e) => eprintln!("hermes-beaver: reconcile: {e:#}"),
                }
            }
            health::evaluate(
                &st,
                Duration::from_secs(self.config.hermes.heartbeat_max_age_secs),
                !self.config.hermes.agent_jobs.is_empty(),
            )
        } else {
            Health::default()
        };
        if let Some(e) = &self.store_error {
            health.problems.push(e.clone());
        }
        let changed = self.refreshed && self.state.health != health;
        self.refreshed = true;
        self.state.health = health;

        // Store: re-query only when another process (or we) wrote to it.
        let Some(store) = &self.store else {
            return changed;
        };
        let stamp = db_stamp(&self.store_path);
        if force_reload || self.demo.is_some() || self.db_stamp != Some(stamp) {
            match store.snapshot(self.config.tasks.finished_rows) {
                Ok(snap) => {
                    self.state.snapshot = snap;
                    self.db_stamp = Some(stamp);
                }
                Err(e) => eprintln!("hermes-beaver: reading the store: {e:#}"),
            }
        }
        changed
    }

    fn open_store(&mut self) {
        let opened = match self.access {
            Access::ReadWrite => Store::open(&self.store_path),
            Access::ReadOnly => Store::open_read_only(&self.store_path),
        };
        match opened {
            Ok(s) => {
                self.store = Some(s);
                self.store_error = None;
                self.db_stamp = None;
            }
            Err(e) => {
                let msg = format!("cannot open the event store: {e:#}");
                if self.store_error.as_ref() != Some(&msg) {
                    eprintln!("hermes-beaver: {msg}");
                }
                self.store_error = Some(msg);
            }
        }
    }
}

fn db_stamp(db: &Path) -> (Option<SystemTime>, Option<SystemTime>) {
    let m = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let mut wal = db.as_os_str().to_owned();
    wal.push("-wal");
    (m(db), m(Path::new(&wal)))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::hermes::tests::fake_home;
    use crate::tools::call_at;

    /// A Hermes home with a fresh heartbeat, so the only problems are the store's.
    fn healthy(home: PathBuf) -> PathBuf {
        std::fs::write(home.join("state/gateway.heartbeat"), "").unwrap();
        home
    }

    fn config(home: &Path) -> Config {
        let mut c = Config::default();
        c.hermes.home = home.display().to_string();
        c.hermes.agent_jobs = vec!["slot-1".into()];
        c
    }

    /// A GitHub task whose cron run has ended: the kiosk closes it from the ledger,
    /// the web view only shows it.
    #[test]
    fn only_the_kiosk_reconciles() {
        let dir = std::env::temp_dir().join(format!("hb-feed-{}", std::process::id()));
        let db = dir.join("events.db");
        let home = healthy(fake_home(
            "feed",
            &[(
                "slot-1",
                "completed",
                Some("2026-11-03T10:00:00+00:00"),
                Some("2026-11-03T10:30:00+00:00"),
                None,
            )],
        ));
        let started = crate::hermes::parse_ts("2026-11-03T10:05:00+00:00").unwrap();
        let now = started + 3600;
        let mut s = Store::open(&db).unwrap();
        call_at(
            &mut s,
            "task_start",
            json!({"source": "github", "title": "t", "ref": "o/r#1", "cron_job_id": "slot-1"}),
            started,
        )
        .unwrap();
        drop(s);
        let c = config(&home);

        let mut web = Feed::new(&c, &Mode::Live, &db, Access::ReadOnly).unwrap();
        web.refresh(now);
        assert_eq!(web.state.snapshot.running.len(), 1);
        assert_eq!(web.state.health.problems, Vec::<String>::new());

        let mut kiosk = Feed::new(&c, &Mode::Live, &db, Access::ReadWrite).unwrap();
        kiosk.refresh(now);
        assert!(kiosk.state.snapshot.running.is_empty());
        assert_eq!(kiosk.state.snapshot.finished[0].status, "done");

        // The web view picks up the kiosk's write on its next tick.
        web.refresh(now);
        assert!(web.state.snapshot.running.is_empty());
        drop((web, kiosk));
        std::fs::remove_dir_all(dir).ok();
        std::fs::remove_dir_all(home).ok();
    }

    /// The web view may start before anything created the store: it reports that in
    /// the banner and opens the store once it exists.
    #[test]
    fn read_only_feed_waits_for_the_store() {
        let dir = std::env::temp_dir().join(format!("hb-feed-wait-{}", std::process::id()));
        let db = dir.join("events.db");
        let home = healthy(fake_home("feed-wait", &[]));
        let c = config(&home);
        let mut web = Feed::new(&c, &Mode::Live, &db, Access::ReadOnly).unwrap();
        web.refresh(0);
        assert!(
            web.state
                .health
                .banner()
                .contains("cannot open the event store"),
            "{}",
            web.state.health.banner()
        );
        assert!(!db.exists(), "a read-only feed never creates the store");
        drop(Store::open(&db).unwrap());
        assert!(web.refresh(0), "the banner went away");
        assert!(web.state.health.ok());
        drop(web);
        std::fs::remove_dir_all(dir).ok();
        std::fs::remove_dir_all(home).ok();
    }
}
