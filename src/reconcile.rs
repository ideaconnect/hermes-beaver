//! Closes GitHub tasks the model never finished, using Hermes's cron ledger.
//!
//! A Bober gate script starts a task inside its cron execution, so the execution
//! with the same job id satisfies `started_at <= task.started_at <= finished_at`.
//! Once that execution has finished and the task is still open, the task is
//! finished from the execution's status.

use anyhow::Result;
use rusqlite::TransactionBehavior;

use crate::hermes::Execution;
use crate::store::Store;
use crate::store::views::TaskRow;

/// Tasks younger than this are left alone (the run may still be reporting).
pub const GRACE_SECS: i64 = 120;

/// The outcome to record for an open task, if its execution has ended.
pub fn decide(task: &TaskRow, executions: &[Execution], now: i64) -> Option<&'static str> {
    if now - task.started_at < GRACE_SECS {
        return None;
    }
    let job = task.cron_job_id.as_deref()?;
    let exec = executions.iter().find(|e| {
        e.job_id == job
            && e.started_at.is_some_and(|s| s <= task.started_at)
            && e.finished_at.is_none_or(|f| task.started_at <= f)
    })?;
    exec.finished_at?;
    Some(if exec.status == "completed" {
        "done"
    } else {
        "failed"
    })
}

/// Finishes every open cron task whose execution has ended. Returns how many.
pub fn reconcile(store: &mut Store, executions: &[Execution], now: i64) -> Result<usize> {
    let decisions: Vec<(i64, &'static str)> = store
        .open_cron_tasks()?
        .iter()
        .filter_map(|t| decide(t, executions, now).map(|o| (t.id, o)))
        .collect();
    if decisions.is_empty() {
        return Ok(0);
    }
    let tx = store
        .conn
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    for (id, outcome) in &decisions {
        crate::tools::finish(
            &tx,
            *id,
            outcome,
            Some("closed from cron ledger".into()),
            None,
            true,
            now,
        )
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    tx.commit()?;
    Ok(decisions.len())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::tools::call_at;

    fn exec(job: &str, status: &str, started: i64, finished: Option<i64>) -> Execution {
        Execution {
            job_id: job.into(),
            status: status.into(),
            started_at: Some(started),
            finished_at: finished,
            error: None,
        }
    }

    fn start(s: &mut Store, ts: i64, r: &str, job: &str) -> i64 {
        call_at(
            s,
            "task_start",
            json!({"source": "github", "title": r, "ref": r, "cron_job_id": job}),
            ts,
        )
        .unwrap()["id"]
            .as_i64()
            .unwrap()
    }

    fn status(s: &Store, id: i64) -> String {
        s.conn
            .query_row("SELECT status FROM tasks WHERE id = ?1", [id], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn closes_tasks_whose_execution_ended() {
        let mut s = Store::open_in_memory().unwrap();
        let done = start(&mut s, 1000, "o/r#1", "slot1");
        let failed = start(&mut s, 1000, "o/r#2", "slot2");
        let running = start(&mut s, 1000, "o/r#3", "slot3");
        let unknown = start(&mut s, 1000, "o/r#4", "slot4");
        let execs = vec![
            exec("slot1", "completed", 990, Some(2000)),
            exec("slot2", "failed", 995, Some(1500)),
            exec("slot3", "running", 990, None),
            // slot4's execution started after the task: not a match.
            exec("slot4", "completed", 1100, Some(1200)),
            // A different job overlapping slot1's task must not interfere.
            exec("slot2", "completed", 900, Some(3000)),
        ];
        assert_eq!(reconcile(&mut s, &execs, 5000).unwrap(), 2);
        assert_eq!(status(&s, done), "done");
        assert_eq!(status(&s, failed), "failed");
        assert_eq!(status(&s, running), "working");
        assert_eq!(status(&s, unknown), "working");
        let reconciled: bool = s
            .conn
            .query_row("SELECT reconciled FROM tasks WHERE id = ?1", [done], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(reconciled);
    }

    #[test]
    fn grace_period_and_non_cron_tasks() {
        let mut s = Store::open_in_memory().unwrap();
        let young = start(&mut s, 1000, "o/r#1", "slot1");
        call_at(
            &mut s,
            "task_start",
            json!({"source": "telegram", "title": "chat"}),
            1000,
        )
        .unwrap();
        let execs = vec![exec("slot1", "completed", 990, Some(1010))];
        assert_eq!(reconcile(&mut s, &execs, 1000 + GRACE_SECS - 1).unwrap(), 0);
        assert_eq!(status(&s, young), "working");
        assert_eq!(reconcile(&mut s, &execs, 1000 + GRACE_SECS).unwrap(), 1);
    }
}
