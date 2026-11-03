//! Read queries behind the dashboard panels.

use anyhow::Result;
use rusqlite::{Row, params};

use super::Store;

#[derive(Debug, Clone, PartialEq)]
pub struct TaskRow {
    pub id: i64,
    pub source: String,
    pub kind: Option<String>,
    pub title: String,
    pub reference: Option<String>,
    pub url: Option<String>,
    pub pr_url: Option<String>,
    pub cron_job_id: Option<String>,
    pub status: String,
    pub note: Option<String>,
    pub summary: Option<String>,
    pub started_at: i64,
    pub updated_at: i64,
    pub finished_at: Option<i64>,
    pub reconciled: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct QueueRow {
    pub reference: Option<String>,
    pub mode: Option<String>,
    pub title: String,
}

/// Why a task is in the "Needs you" list. Order = display priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum NeedKind {
    Feedback,
    Skipped,
    Failed,
    Review,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NeedRow {
    pub kind: NeedKind,
    pub task: TaskRow,
}

/// Everything the task panels need, read in one go.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    pub running: Vec<TaskRow>,
    pub queue: Vec<QueueRow>,
    pub needs: Vec<NeedRow>,
    pub finished: Vec<TaskRow>,
    pub done_count: i64,
    pub go_live_at: i64,
    pub last_event_id: i64,
}

const COLS: &str = "id, source, kind, title, ref, url, pr_url, cron_job_id, status, note, summary,
                    started_at, updated_at, finished_at, reconciled";

fn task_row(r: &Row) -> rusqlite::Result<TaskRow> {
    Ok(TaskRow {
        id: r.get(0)?,
        source: r.get(1)?,
        kind: r.get(2)?,
        title: r.get(3)?,
        reference: r.get(4)?,
        url: r.get(5)?,
        pr_url: r.get(6)?,
        cron_job_id: r.get(7)?,
        status: r.get(8)?,
        note: r.get(9)?,
        summary: r.get(10)?,
        started_at: r.get(11)?,
        updated_at: r.get(12)?,
        finished_at: r.get(13)?,
        reconciled: r.get(14)?,
    })
}

impl Store {
    fn tasks(&self, where_order: &str, p: impl rusqlite::Params) -> Result<Vec<TaskRow>> {
        let sql = format!("SELECT {COLS} FROM tasks t {where_order}");
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(p, task_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Working or waiting tasks, oldest first.
    pub fn running(&self) -> Result<Vec<TaskRow>> {
        self.tasks(
            "WHERE status IN ('working', 'waiting_feedback') ORDER BY started_at, id",
            [],
        )
    }

    pub fn queue(&self) -> Result<Vec<QueueRow>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT ref, mode, title FROM queue_items ORDER BY position")?;
        let rows = stmt.query_map([], |r| {
            Ok(QueueRow {
                reference: r.get(0)?,
                mode: r.get(1)?,
                title: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Tasks needing the owner, in display order:
    /// 1. waiting for feedback (oldest first);
    /// 2. GitHub refs whose latest task was skipped (newest first);
    /// 3. GitHub refs whose latest task failed (newest first);
    /// 4. GitHub refs whose latest task is done with a PR, not closed (oldest first).
    ///
    /// Only ref'd tasks have a close lifecycle, so groups 2-4 are GitHub-only;
    /// a failed Telegram task just shows in "finished".
    pub fn needs_you(&self) -> Result<Vec<NeedRow>> {
        let mut out: Vec<NeedRow> = self
            .tasks(
                "WHERE status = 'waiting_feedback' ORDER BY updated_at, id",
                [],
            )?
            .into_iter()
            .map(|task| NeedRow {
                kind: NeedKind::Feedback,
                task,
            })
            .collect();

        let latest = self.tasks(
            "WHERE t.ref IS NOT NULL AND t.closed_at IS NULL
               AND t.status IN ('skipped', 'failed', 'done')
               AND t.id = (SELECT MAX(u.id) FROM tasks u WHERE u.ref = t.ref)",
            [],
        )?;
        let mut grouped: Vec<NeedRow> = Vec::new();
        for mut task in latest {
            let kind = match task.status.as_str() {
                "skipped" => NeedKind::Skipped,
                "failed" => NeedKind::Failed,
                _ => {
                    if task.pr_url.is_none() {
                        task.pr_url =
                            self.latest_pr_url(task.reference.as_deref().unwrap_or(""))?;
                    }
                    if task.pr_url.is_none() {
                        continue;
                    }
                    NeedKind::Review
                }
            };
            grouped.push(NeedRow { kind, task });
        }
        grouped.sort_by(|a, b| {
            let fa = a.task.finished_at.unwrap_or(0);
            let fb = b.task.finished_at.unwrap_or(0);
            a.kind.cmp(&b.kind).then(if a.kind == NeedKind::Review {
                fa.cmp(&fb)
            } else {
                fb.cmp(&fa)
            })
        });
        out.extend(grouped);
        Ok(out)
    }

    fn latest_pr_url(&self, reference: &str) -> Result<Option<String>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT pr_url FROM tasks WHERE ref = ?1 AND pr_url IS NOT NULL ORDER BY id DESC LIMIT 1",
        )?;
        let mut rows = stmt.query(params![reference])?;
        Ok(match rows.next()? {
            Some(r) => r.get(0)?,
            None => None,
        })
    }

    pub fn finished(&self, limit: usize) -> Result<Vec<TaskRow>> {
        self.tasks(
            "WHERE finished_at IS NOT NULL ORDER BY finished_at DESC, id DESC LIMIT ?1",
            [limit as i64],
        )
    }

    pub fn done_count(&self) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM tasks WHERE status = 'done'",
            [],
            |r| r.get(0),
        )?)
    }

    /// Unfinished tasks started by a cron job, for reconciliation.
    pub fn open_cron_tasks(&self) -> Result<Vec<TaskRow>> {
        self.tasks(
            "WHERE status IN ('working', 'waiting_feedback') AND cron_job_id IS NOT NULL ORDER BY id",
            [],
        )
    }

    pub fn snapshot(&self, finished_limit: usize) -> Result<Snapshot> {
        Ok(Snapshot {
            running: self.running()?,
            queue: self.queue()?,
            needs: self.needs_you()?,
            finished: self.finished(finished_limit)?,
            done_count: self.done_count()?,
            go_live_at: self.go_live_at()?,
            last_event_id: self.last_event_id()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::tools::call_at;

    fn at(s: &mut Store, ts: i64, name: &str, args: serde_json::Value) -> serde_json::Value {
        call_at(s, name, args, ts).unwrap_or_else(|e| panic!("{name}: {e}"))
    }

    fn gh(s: &mut Store, ts: i64, r: &str) -> i64 {
        at(
            s,
            ts,
            "task_start",
            json!({"source": "github", "title": r, "ref": r}),
        )["id"]
            .as_i64()
            .unwrap()
    }

    fn kinds(s: &Store) -> Vec<(NeedKind, String)> {
        s.needs_you()
            .unwrap()
            .into_iter()
            .map(|n| (n.kind, n.task.title))
            .collect()
    }

    #[test]
    fn needs_you_orders_groups_and_items() {
        let mut s = Store::open_in_memory().unwrap();
        // Review items: finished at 10 and 20 → oldest first.
        gh(&mut s, 1, "o/r#1");
        at(
            &mut s,
            10,
            "task_finish",
            json!({"ref": "o/r#1", "outcome": "done", "pr_url": "u1"}),
        );
        gh(&mut s, 2, "o/r#2");
        at(
            &mut s,
            20,
            "task_finish",
            json!({"ref": "o/r#2", "outcome": "done", "pr_url": "u2"}),
        );
        // Failed at 30 and 40 → newest first.
        gh(&mut s, 3, "o/r#3");
        at(
            &mut s,
            30,
            "task_finish",
            json!({"ref": "o/r#3", "outcome": "failed"}),
        );
        gh(&mut s, 4, "o/r#4");
        at(
            &mut s,
            40,
            "task_finish",
            json!({"ref": "o/r#4", "outcome": "failed"}),
        );
        // Skipped.
        gh(&mut s, 5, "o/r#5");
        at(
            &mut s,
            50,
            "task_finish",
            json!({"ref": "o/r#5", "outcome": "skipped"}),
        );
        // Done without a PR: not in the list.
        gh(&mut s, 6, "o/r#6");
        at(
            &mut s,
            60,
            "task_finish",
            json!({"ref": "o/r#6", "outcome": "done"}),
        );
        // Waiting for feedback (Telegram).
        let t = at(
            &mut s,
            7,
            "task_start",
            json!({"source": "telegram", "title": "chat"}),
        )["id"]
            .clone();
        at(
            &mut s,
            70,
            "task_update",
            json!({"id": t, "status": "waiting_feedback", "note": "?"}),
        );

        assert_eq!(
            kinds(&s),
            vec![
                (NeedKind::Feedback, "chat".into()),
                (NeedKind::Skipped, "o/r#5".into()),
                (NeedKind::Failed, "o/r#4".into()),
                (NeedKind::Failed, "o/r#3".into()),
                (NeedKind::Review, "o/r#1".into()),
                (NeedKind::Review, "o/r#2".into()),
            ]
        );
    }

    #[test]
    fn later_task_for_same_ref_supersedes_and_keeps_pr_url() {
        let mut s = Store::open_in_memory().unwrap();
        gh(&mut s, 1, "o/r#1");
        at(
            &mut s,
            2,
            "task_finish",
            json!({"ref": "o/r#1", "outcome": "failed"}),
        );
        assert_eq!(kinds(&s)[0].0, NeedKind::Failed);
        // A retry succeeds with a PR: failed entry disappears, review appears.
        gh(&mut s, 3, "o/r#1");
        assert!(kinds(&s).is_empty(), "running tasks are not in needs-you");
        at(
            &mut s,
            4,
            "task_finish",
            json!({"ref": "o/r#1", "outcome": "done", "pr_url": "https://pr/1"}),
        );
        // An update job finishes without repeating the PR URL: still reviewable.
        at(
            &mut s,
            5,
            "task_start",
            json!({"source": "github", "title": "upd", "ref": "o/r#1", "kind": "update"}),
        );
        at(
            &mut s,
            6,
            "task_finish",
            json!({"ref": "o/r#1", "outcome": "done"}),
        );
        let needs = s.needs_you().unwrap();
        assert_eq!(needs.len(), 1);
        assert_eq!(needs[0].kind, NeedKind::Review);
        assert_eq!(needs[0].task.pr_url.as_deref(), Some("https://pr/1"));
        // Closing the issue removes it.
        at(&mut s, 7, "task_closed", json!({"ref": "o/r#1"}));
        assert!(s.needs_you().unwrap().is_empty());
    }

    #[test]
    fn telegram_failures_only_show_in_finished() {
        let mut s = Store::open_in_memory().unwrap();
        let id = at(
            &mut s,
            1,
            "task_start",
            json!({"source": "telegram", "title": "t"}),
        )["id"]
            .clone();
        at(
            &mut s,
            2,
            "task_finish",
            json!({"id": id, "outcome": "failed"}),
        );
        assert!(s.needs_you().unwrap().is_empty());
        assert_eq!(s.finished(8).unwrap().len(), 1);
    }

    #[test]
    fn snapshot_counts_done_and_lists_running_and_queue() {
        let mut s = Store::open_in_memory().unwrap();
        at(
            &mut s,
            1,
            "queue_set",
            json!({"items": [{"ref": "o/r#9", "mode": "new", "title": "Nine"}]}),
        );
        gh(&mut s, 2, "o/r#1");
        let t = at(
            &mut s,
            3,
            "task_start",
            json!({"source": "direct", "title": "d"}),
        )["id"]
            .clone();
        at(
            &mut s,
            4,
            "task_finish",
            json!({"id": t, "outcome": "done"}),
        );
        let snap = s.snapshot(8).unwrap();
        assert_eq!(snap.running.len(), 1);
        assert_eq!(snap.queue.len(), 1);
        assert_eq!(snap.queue[0].mode.as_deref(), Some("new"));
        assert_eq!(snap.finished.len(), 1);
        assert_eq!(snap.done_count, 1);
        assert!(snap.last_event_id > 0);
    }

    #[test]
    fn finished_is_limited_and_newest_first() {
        let mut s = Store::open_in_memory().unwrap();
        for i in 0..5 {
            let id = at(
                &mut s,
                i,
                "task_start",
                json!({"source": "direct", "title": format!("t{i}")}),
            )["id"]
                .clone();
            at(
                &mut s,
                100 + i,
                "task_finish",
                json!({"id": id, "outcome": "done"}),
            );
        }
        let f = s.finished(3).unwrap();
        assert_eq!(
            f.iter().map(|t| t.title.as_str()).collect::<Vec<_>>(),
            ["t4", "t3", "t2"]
        );
    }
}
