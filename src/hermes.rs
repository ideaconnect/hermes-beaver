//! Read-only access to Hermes Agent's own files, for health checks and
//! reconciliation. Nothing here ever writes into the Hermes home.
//!
//! - `state/gateway.heartbeat`: rewritten every 30 s by the gateway.
//! - `cron/executions.db`: the cron ledger (keeps the last ~1000 finished runs).
//! - `bober/state.json`: Bober's own state (`consecutive_failures`).

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use rusqlite::{Connection, OpenFlags};

#[derive(Debug, Clone, PartialEq)]
pub struct Execution {
    pub job_id: String,
    pub status: String,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub error: Option<String>,
}

/// What the health rules and reconciliation need, gathered in one poll.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HermesState {
    /// `None` when the heartbeat file is missing.
    pub heartbeat_age: Option<Duration>,
    /// Recent executions of the configured jobs, newest first.
    pub executions: Vec<Execution>,
    pub bober_failures: Option<i64>,
    /// Problems reading the files above ("cannot read …").
    pub read_errors: Vec<String>,
    /// Whether the Hermes home exists at all.
    pub home_exists: bool,
}

/// Polls Hermes files, re-reading the ledger only when it changed.
pub struct HermesReader {
    home: PathBuf,
    jobs: Vec<String>,
    ledger_stamp: Option<(Option<SystemTime>, Option<SystemTime>)>,
    cached_executions: Vec<Execution>,
}

const LEDGER_ROWS: usize = 400;

impl HermesReader {
    pub fn new(home: PathBuf, jobs: Vec<String>) -> Self {
        Self {
            home,
            jobs,
            ledger_stamp: None,
            cached_executions: Vec::new(),
        }
    }

    pub fn poll(&mut self) -> HermesState {
        let mut st = HermesState {
            home_exists: self.home.is_dir(),
            ..Default::default()
        };
        if !st.home_exists {
            return st;
        }
        st.heartbeat_age = heartbeat_age(&self.home.join("state/gateway.heartbeat"));

        if !self.jobs.is_empty() {
            let db = self.home.join("cron/executions.db");
            let stamp = (mtime(&db), mtime(&db.with_extension("db-wal")));
            if self.ledger_stamp != Some(stamp) {
                match read_executions(&db, &self.jobs, LEDGER_ROWS) {
                    Ok(rows) => {
                        self.cached_executions = rows;
                        self.ledger_stamp = Some(stamp);
                    }
                    Err(e) => st.read_errors.push(format!("cannot read cron ledger: {e}")),
                }
            }
            st.executions = self.cached_executions.clone();
        }

        let bober = self.home.join("bober/state.json");
        if bober.exists() {
            match read_bober_failures(&bober) {
                Ok(n) => st.bober_failures = n,
                Err(e) => st.read_errors.push(format!("cannot read Bober state: {e}")),
            }
        }
        st
    }
}

fn mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

fn heartbeat_age(p: &Path) -> Option<Duration> {
    let m = mtime(p)?;
    Some(
        SystemTime::now()
            .duration_since(m)
            .unwrap_or(Duration::ZERO),
    )
}

/// Parses Hermes timestamps: ISO-8601 with offset (`2026-11-03T13:39:01.123456+00:00`).
pub fn parse_ts(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s.trim())
        .ok()
        .map(|d| d.timestamp())
        .or_else(|| {
            chrono::NaiveDateTime::parse_from_str(s.trim(), "%Y-%m-%dT%H:%M:%S%.f")
                .ok()
                .map(|d| d.and_utc().timestamp())
        })
}

pub fn read_executions(db: &Path, jobs: &[String], limit: usize) -> Result<Vec<Execution>, String> {
    if !db.exists() {
        return Err("executions.db not found".into());
    }
    let uri = format!("file:{}?mode=ro", db.display());
    let conn = Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| e.to_string())?;
    conn.busy_timeout(Duration::from_secs(2))
        .map_err(|e| e.to_string())?;
    let placeholders = vec!["?"; jobs.len()].join(", ");
    let sql = format!(
        "SELECT job_id, status, started_at, finished_at, error FROM executions
         WHERE job_id IN ({placeholders})
         ORDER BY COALESCE(finished_at, started_at, claimed_at) DESC LIMIT {limit}"
    );
    let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(jobs.iter()), |r| {
            let started: Option<String> = r.get(2)?;
            let finished: Option<String> = r.get(3)?;
            Ok(Execution {
                job_id: r.get(0)?,
                status: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                started_at: started.as_deref().and_then(parse_ts),
                finished_at: finished.as_deref().and_then(parse_ts),
                error: r.get(4)?,
            })
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<_, _>>().map_err(|e| e.to_string())
}

fn read_bober_failures(p: &Path) -> Result<Option<i64>, String> {
    let text = std::fs::read_to_string(p).map_err(|e| e.to_string())?;
    let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    Ok(v.get("consecutive_failures")
        .and_then(serde_json::Value::as_i64))
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// (job_id, status, started_at, finished_at, error)
    type Row<'a> = (
        &'a str,
        &'a str,
        Option<&'a str>,
        Option<&'a str>,
        Option<&'a str>,
    );

    /// Builds a fake Hermes home with an executions ledger.
    pub fn fake_home(name: &str, rows: &[Row]) -> PathBuf {
        let home = std::env::temp_dir().join(format!("hb-hermes-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join("cron")).unwrap();
        std::fs::create_dir_all(home.join("state")).unwrap();
        let conn = Connection::open(home.join("cron/executions.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE executions (id TEXT PRIMARY KEY, job_id TEXT, status TEXT, claimed_at TEXT,
                                      started_at TEXT, finished_at TEXT, error TEXT, delivery_outcome TEXT);",
        )
        .unwrap();
        for (i, (job, status, started, finished, error)) in rows.iter().enumerate() {
            conn.execute(
                "INSERT INTO executions (id, job_id, status, claimed_at, started_at, finished_at, error)
                 VALUES (?1, ?2, ?3, ?4, ?4, ?5, ?6)",
                rusqlite::params![i.to_string(), job, status, started, finished, error],
            )
            .unwrap();
        }
        home
    }

    #[test]
    fn parses_hermes_timestamps() {
        assert_eq!(
            parse_ts("2026-11-03T13:39:01.123456+00:00"),
            Some(1_793_713_141)
        );
        assert_eq!(parse_ts("2026-11-03T13:39:01Z"), Some(1_793_713_141));
        assert_eq!(parse_ts("2026-11-03T13:39:01.5"), Some(1_793_713_141));
        assert_eq!(parse_ts("yesterday"), None);
    }

    #[test]
    fn reads_ledger_newest_first_for_configured_jobs() {
        let home = fake_home(
            "ledger",
            &[
                (
                    "a",
                    "completed",
                    Some("2026-11-03T10:00:00+00:00"),
                    Some("2026-11-03T10:00:20+00:00"),
                    None,
                ),
                (
                    "b",
                    "failed",
                    Some("2026-11-03T11:00:00+00:00"),
                    Some("2026-11-03T11:00:05+00:00"),
                    Some("boom"),
                ),
                (
                    "other",
                    "failed",
                    Some("2026-11-03T12:00:00+00:00"),
                    Some("2026-11-03T12:00:01+00:00"),
                    None,
                ),
                (
                    "a",
                    "running",
                    Some("2026-11-03T12:30:00+00:00"),
                    None,
                    None,
                ),
            ],
        );
        let mut r = HermesReader::new(home.clone(), vec!["a".into(), "b".into()]);
        let st = r.poll();
        assert!(st.home_exists);
        assert!(st.read_errors.is_empty(), "{:?}", st.read_errors);
        let statuses: Vec<&str> = st.executions.iter().map(|e| e.status.as_str()).collect();
        assert_eq!(statuses, ["running", "failed", "completed"]);
        assert_eq!(st.executions[1].error.as_deref(), Some("boom"));
        assert_eq!(st.executions[0].finished_at, None);
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn missing_and_garbled_files_are_reported_not_fatal() {
        let home = fake_home("garbled", &[]);
        std::fs::remove_file(home.join("cron/executions.db")).unwrap();
        std::fs::create_dir_all(home.join("bober")).unwrap();
        std::fs::write(home.join("bober/state.json"), "{not json").unwrap();
        let st = HermesReader::new(home.clone(), vec!["a".into()]).poll();
        assert_eq!(st.read_errors.len(), 2, "{:?}", st.read_errors);
        assert_eq!(st.heartbeat_age, None);
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn reads_bober_failures_and_heartbeat() {
        let home = fake_home("bober", &[]);
        std::fs::create_dir_all(home.join("bober")).unwrap();
        std::fs::write(
            home.join("bober/state.json"),
            r#"{"consecutive_failures": 3, "issues": {}}"#,
        )
        .unwrap();
        std::fs::write(home.join("state/gateway.heartbeat"), "{}").unwrap();
        let st = HermesReader::new(home.clone(), vec![]).poll();
        assert_eq!(st.bober_failures, Some(3));
        assert!(st.heartbeat_age.unwrap() < Duration::from_secs(5));
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn missing_home_is_flagged() {
        let st = HermesReader::new(PathBuf::from("/nonexistent/hermes"), vec![]).poll();
        assert!(!st.home_exists);
    }
}
