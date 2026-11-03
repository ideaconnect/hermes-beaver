//! The SQLite event store shared by the MCP server, the `call` CLI and the GUI.
//!
//! Writers: `hermes-beaver mcp` (long-lived child of Hermes), `hermes-beaver call`
//! (short-lived, from scripts) and the GUI's reconciliation. WAL mode plus a busy
//! timeout lets them coexist. Migrations are additive only: an older binary may
//! still be running against a newer schema.

pub mod views;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};

use crate::display;

#[cfg(test)]
pub const SCHEMA_VERSION: i64 = 1;

const MIGRATIONS: &[&str] = &[
    // v1
    r#"
    CREATE TABLE tasks (
        id           INTEGER PRIMARY KEY AUTOINCREMENT,
        source       TEXT NOT NULL CHECK (source IN ('github', 'telegram', 'direct')),
        kind         TEXT,
        title        TEXT NOT NULL,
        ref          TEXT,
        url          TEXT,
        pr_url       TEXT,
        cron_job_id  TEXT,
        status       TEXT NOT NULL
                     CHECK (status IN ('working', 'waiting_feedback', 'done', 'failed', 'skipped')),
        note         TEXT,
        summary      TEXT,
        created_at   INTEGER NOT NULL,
        started_at   INTEGER NOT NULL,
        updated_at   INTEGER NOT NULL,
        finished_at  INTEGER,
        closed_at    INTEGER,
        reconciled   INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX tasks_ref ON tasks (ref);
    CREATE INDEX tasks_status ON tasks (status);
    CREATE TABLE queue_items (
        position    INTEGER PRIMARY KEY,
        ref         TEXT,
        mode        TEXT,
        title       TEXT NOT NULL,
        url         TEXT,
        updated_at  INTEGER NOT NULL
    );
    CREATE TABLE events (
        id       INTEGER PRIMARY KEY AUTOINCREMENT,
        ts       INTEGER NOT NULL,
        kind     TEXT NOT NULL,
        task_id  INTEGER,
        payload  TEXT
    );
    CREATE TABLE meta (
        key    TEXT PRIMARY KEY,
        value  TEXT NOT NULL
    );
    "#,
];

pub struct Store {
    pub(crate) conn: Connection,
}

/// Unix time in seconds.
pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl Store {
    /// `$XDG_STATE_HOME/hermes-beaver/events.db` (usually `~/.local/state/...`).
    pub fn default_path() -> PathBuf {
        dirs::state_dir()
            .or_else(|| dirs::home_dir().map(|h| h.join(".local/state")))
            .unwrap_or_else(|| PathBuf::from("."))
            .join("hermes-beaver")
            .join("events.db")
    }

    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        Self::init(conn)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        // WAL is a no-op for in-memory databases; that's fine.
        let _: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
        conn.execute_batch("PRAGMA synchronous = NORMAL;")?;
        let mut store = Self { conn };
        store.migrate()?;
        store
            .conn
            .execute(
                "INSERT OR IGNORE INTO meta (key, value) VALUES ('go_live_at', ?1)",
                params![now().to_string()],
            )
            .context("recording go_live_at")?;
        Ok(store)
    }

    fn migrate(&mut self) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let version: i64 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        // A newer binary may have migrated further; additive schemas stay usable.
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(version.max(0) as usize) {
            tx.execute_batch(sql)
                .with_context(|| format!("migration to v{}", i + 1))?;
            tx.pragma_update(None, "user_version", (i + 1) as i64)?;
        }
        tx.commit()?;
        Ok(())
    }

    #[cfg(test)]
    pub fn schema_version(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))?)
    }

    pub fn go_live_at(&self) -> Result<i64> {
        let v: Option<String> = self
            .conn
            .query_row("SELECT value FROM meta WHERE key = 'go_live_at'", [], |r| {
                r.get(0)
            })
            .optional()?;
        Ok(v.and_then(|s| s.parse().ok()).unwrap_or_else(now))
    }

    /// Highest event id, used by the GUI to spot new events cheaply.
    pub fn last_event_id(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COALESCE(MAX(id), 0) FROM events", [], |r| r.get(0))?)
    }

    /// Kinds of events newer than `after`.
    pub fn event_kinds_since(&self, after: i64) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT kind FROM events WHERE id > ?1 ORDER BY id")?;
        let rows = stmt.query_map([after], |r| r.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}

/// The owner's last monitor request (`display_off` / `display_on` / `display_vacation`).
/// Kept in `meta` (keys `display`, `display_since`, `display_until`), so no migration is
/// needed. An unknown value (from a newer binary) reads as the default.
pub(crate) fn read_display(conn: &Connection) -> rusqlite::Result<display::Request> {
    let get = |key: &str| {
        conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| {
            r.get::<_, String>(0)
        })
        .optional()
    };
    let Some(mode) = get("display")?.as_deref().and_then(display::Mode::parse) else {
        return Ok(display::Request::default());
    };
    let num = |v: Option<String>| v.and_then(|v| v.parse().ok());
    Ok(display::Request {
        mode,
        since: num(get("display_since")?).unwrap_or(0),
        until: match mode {
            display::Mode::Off => num(get("display_until")?),
            _ => None,
        },
    })
}

pub(crate) fn write_display(conn: &Connection, req: &display::Request) -> rusqlite::Result<()> {
    let put = |key: &str, value: Option<String>| match value {
        Some(v) => conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
            params![key, v],
        ),
        None => conn.execute("DELETE FROM meta WHERE key = ?1", [key]),
    };
    put("display", Some(req.mode.as_str().to_owned()))?;
    put("display_since", Some(req.since.to_string()))?;
    let until = req.until.filter(|_| req.mode == display::Mode::Off);
    put("display_until", until.map(|u| u.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_fresh_database_and_records_go_live() {
        let s = Store::open_in_memory().unwrap();
        assert_eq!(s.schema_version().unwrap(), SCHEMA_VERSION);
        assert!(s.go_live_at().unwrap() > 0);
        assert_eq!(s.last_event_id().unwrap(), 0);
    }

    #[test]
    fn display_request_round_trips() {
        use display::{Mode, Request};
        let s = Store::open_in_memory().unwrap();
        assert_eq!(read_display(&s.conn).unwrap(), Request::default());
        let off = Request {
            mode: Mode::Off,
            since: 100,
            until: Some(500),
        };
        write_display(&s.conn, &off).unwrap();
        assert_eq!(read_display(&s.conn).unwrap(), off);
        let vacation = Request {
            mode: Mode::Vacation,
            since: 200,
            until: Some(900), // only `off` keeps an end time
        };
        write_display(&s.conn, &vacation).unwrap();
        assert_eq!(
            read_display(&s.conn).unwrap(),
            Request {
                until: None,
                ..vacation
            }
        );
        // The first release stored only `display`; a missing `display_since` reads as 0.
        s.conn
            .execute_batch(
                "DELETE FROM meta WHERE key LIKE 'display_%'; UPDATE meta SET value = 'on' WHERE key = 'display';",
            )
            .unwrap();
        assert_eq!(read_display(&s.conn).unwrap(), Request::default());
    }

    #[test]
    fn reopening_keeps_data_and_go_live() {
        let dir = std::env::temp_dir().join(format!("hb-store-{}", std::process::id()));
        let path = dir.join("events.db");
        let first = Store::open(&path).unwrap();
        let go_live = first.go_live_at().unwrap();
        first
            .conn
            .execute("UPDATE meta SET value = '123' WHERE key = 'go_live_at'", [])
            .unwrap();
        drop(first);
        let again = Store::open(&path).unwrap();
        assert_eq!(again.go_live_at().unwrap(), 123);
        assert!(go_live > 0);
        drop(again);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn concurrent_writers_do_not_fail() {
        let dir = std::env::temp_dir().join(format!("hb-conc-{}", std::process::id()));
        let path = dir.join("events.db");
        Store::open(&path).unwrap();
        let handles: Vec<_> = (0..4)
            .map(|n| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let mut s = Store::open(&path).unwrap();
                    for i in 0..25 {
                        crate::tools::call(
                            &mut s,
                            &Default::default(),
                            "task_start",
                            serde_json::json!({"source": "direct", "title": format!("t{n}-{i}")}),
                        )
                        .unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let s = Store::open(&path).unwrap();
        let n: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM tasks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 100);
        drop(s);
        std::fs::remove_dir_all(dir).ok();
    }
}
