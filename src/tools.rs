//! The tools, shared by the MCP server and `hermes-beaver call`.

use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::display::{Mode, Request, Schedule, next_change, wants_off};
use crate::store::{Store, now, read_display, write_display};

const MAX_TITLE: usize = 300;
const MAX_NOTE: usize = 500;

#[derive(Debug)]
pub enum ToolError {
    /// The tool name doesn't exist.
    UnknownTool(String),
    /// Bad arguments or a request that doesn't fit the current state.
    Invalid(String),
    /// Database or other internal failure.
    Internal(anyhow::Error),
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownTool(n) => write!(f, "unknown tool `{n}`"),
            Self::Invalid(m) => f.write_str(m),
            Self::Internal(e) => write!(f, "internal error: {e:#}"),
        }
    }
}

impl From<rusqlite::Error> for ToolError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Internal(e.into())
    }
}

fn invalid<T>(msg: impl Into<String>) -> Result<T, ToolError> {
    Err(ToolError::Invalid(msg.into()))
}

pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
}

pub fn definitions() -> Vec<ToolDef> {
    let id_or_ref = json!({
        "id": {"type": "integer", "description": "Task id returned by task_start."},
        "ref": {"type": "string", "description": "GitHub ref like owner/repo#123; resolves to the most recent unfinished task with that ref."}
    });
    let mut update_props = id_or_ref.as_object().unwrap().clone();
    update_props.insert(
        "status".into(),
        json!({"type": "string", "enum": ["working", "waiting_feedback"]}),
    );
    update_props.insert("note".into(), json!({"type": "string", "description": "Short status note; for waiting_feedback, the question for the owner."}));
    let mut finish_props = id_or_ref.as_object().unwrap().clone();
    finish_props.insert(
        "outcome".into(),
        json!({"type": "string", "enum": ["done", "failed", "skipped"]}),
    );
    finish_props.insert("summary".into(), json!({"type": "string", "description": "One-line result, e.g. 'PR opened' or why it failed."}));
    finish_props.insert("pr_url".into(), json!({"type": "string", "description": "URL of the pull request, if one was opened or updated."}));

    vec![
        ToolDef {
            name: "task_start",
            description: concat!(
                "Report that you started a piece of real work for the owner, so it appears on the dashboard. ",
                "Call it once when a request needs actual work (tool use), not for plain chat replies. ",
                "Returns the task id to use with task_update and task_finish. ",
                "For GitHub work pass ref (owner/repo#N); calling again for a ref with an unfinished task returns that task. ",
                "Never put secrets (tokens, keys, private URLs) in titles, notes or summaries: they are shown on a wall display."
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "source": {"type": "string", "enum": ["github", "telegram", "direct"], "description": "Where the request came from."},
                    "title": {"type": "string", "description": "Short description of the task (one line)."},
                    "kind": {"type": "string", "description": "Optional: new, update, go, review or chat."},
                    "ref": {"type": "string", "description": "GitHub ref like owner/repo#123."},
                    "url": {"type": "string", "description": "Issue or PR URL."},
                    "cron_job_id": {"type": "string", "description": "Hermes cron job id running this task (scripts only)."}
                },
                "required": ["source", "title"]
            }),
        },
        ToolDef {
            name: "task_update",
            description: concat!(
                "Update a running task. Use status waiting_feedback with a note containing your question ",
                "when you are blocked waiting for the owner; set it back to working when you continue. ",
                "Identify the task by id or by ref. ",
                "Never put secrets (tokens, keys, private URLs) in titles, notes or summaries: they are shown on a wall display."
            ),
            input_schema: json!({"type": "object", "properties": update_props, "required": ["status"]}),
        },
        ToolDef {
            name: "task_finish",
            description: concat!(
                "Report that a task ended: done, failed or skipped (needs a decision from the owner). ",
                "Identify the task by id or by ref. Pass pr_url when a pull request was opened or updated. ",
                "Never put secrets (tokens, keys, private URLs) in titles, notes or summaries: they are shown on a wall display."
            ),
            input_schema: json!({"type": "object", "properties": finish_props, "required": ["outcome"]}),
        },
        ToolDef {
            name: "queue_set",
            description: concat!(
                "Replace the list of queued (not yet started) tasks with this full snapshot. ",
                "Meant for scheduler scripts; send the whole queue every time, an empty list clears it."
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "items": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "ref": {"type": "string"},
                                "mode": {"type": "string"},
                                "title": {"type": "string"},
                                "url": {"type": "string"}
                            },
                            "required": ["title"]
                        }
                    }
                },
                "required": ["items"]
            }),
        },
        ToolDef {
            name: "task_closed",
            description: concat!(
                "Report that a GitHub issue or pull request was closed or merged, ",
                "so it leaves the review and attention lists."
            ),
            input_schema: json!({
                "type": "object",
                "properties": {"ref": {"type": "string", "description": "owner/repo#N"}},
                "required": ["ref"]
            }),
        },
        ToolDef {
            name: "display_off",
            description: concat!(
                "Turn the dashboard's monitor off (standby) because the owner is going to sleep or leaving the room. ",
                "It stays off until display_on, the optional `until` time, or the next scheduled switch on ",
                "(the dashboard may have a daily schedule, e.g. off at 23:00 and on at 09:00). New tasks don't wake it. ",
                "The result's `until` says when it comes back on by itself (null: not until display_on). ",
                "In vacation mode it changes nothing. Takes effect within 10 seconds."
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "until": {"type": "string", "description": "Optional: when to turn the monitor back on by itself, as RFC 3339 (e.g. 2026-11-04T07:00:00+01:00). Without an offset it is the dashboard host's local time."}
                }
            }),
        },
        ToolDef {
            name: "display_on",
            description: concat!(
                "Turn the dashboard's monitor on because the owner is awake or back. Also ends vacation mode. ",
                "It stays on until display_off or the next scheduled switch off; the result's `until` says when. ",
                "Takes effect within 10 seconds."
            ),
            input_schema: json!({"type": "object", "properties": {}}),
        },
        ToolDef {
            name: "display_vacation",
            description: concat!(
                "Vacation mode: turn the dashboard's monitor off until display_on is called, ignoring the daily schedule. ",
                "Use it when the owner is away for days (vacation, a trip). Takes effect within 10 seconds."
            ),
            input_schema: json!({"type": "object", "properties": {}}),
        },
    ]
}

/// Runs one tool. `args` must be a JSON object. `schedule` is the monitor's daily
/// schedule from the config; only the display tools use it.
pub fn call(
    store: &mut Store,
    schedule: &Schedule,
    name: &str,
    args: Value,
) -> Result<Value, ToolError> {
    call_with(store, schedule, name, args, now())
}

/// `call` at a given time, without a monitor schedule.
pub fn call_at(store: &mut Store, name: &str, args: Value, ts: i64) -> Result<Value, ToolError> {
    call_with(store, &Schedule::default(), name, args, ts)
}

pub fn call_with(
    store: &mut Store,
    schedule: &Schedule,
    name: &str,
    args: Value,
    ts: i64,
) -> Result<Value, ToolError> {
    let args = if args.is_null() { json!({}) } else { args };
    if !args.is_object() {
        return invalid("arguments must be a JSON object");
    }
    let tx = store
        .conn
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    let out = match name {
        "task_start" => task_start(&tx, parse(args)?, ts),
        "task_update" => task_update(&tx, parse(args)?, ts),
        "task_finish" => task_finish(&tx, parse(args)?, ts),
        "queue_set" => queue_set(&tx, parse(args)?, ts),
        "task_closed" => task_closed(&tx, parse(args)?, ts),
        "display_off" => display_off(&tx, schedule, parse(args)?, ts),
        "display_on" => set_display(&tx, schedule, Mode::On, None, ts),
        "display_vacation" => set_display(&tx, schedule, Mode::Vacation, None, ts),
        other => Err(ToolError::UnknownTool(other.to_owned())),
    }?;
    tx.commit()?;
    Ok(out)
}

fn parse<T: for<'de> Deserialize<'de>>(args: Value) -> Result<T, ToolError> {
    serde_json::from_value(args).map_err(|e| ToolError::Invalid(format!("invalid arguments: {e}")))
}

fn clean(s: Option<String>, max: usize) -> Option<String> {
    let s = s?.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.is_empty() {
        return None;
    }
    Some(if s.chars().count() > max {
        s.chars().take(max - 1).chain(['…']).collect()
    } else {
        s
    })
}

fn event(
    tx: &Transaction,
    ts: i64,
    kind: &str,
    task_id: Option<i64>,
    payload: Value,
) -> Result<(), ToolError> {
    tx.execute(
        "INSERT INTO events (ts, kind, task_id, payload) VALUES (?1, ?2, ?3, ?4)",
        params![ts, kind, task_id, payload.to_string()],
    )?;
    Ok(())
}

#[derive(Deserialize)]
struct StartArgs {
    source: String,
    title: String,
    kind: Option<String>,
    #[serde(rename = "ref")]
    reference: Option<String>,
    url: Option<String>,
    cron_job_id: Option<String>,
}

fn task_start(tx: &Transaction, a: StartArgs, ts: i64) -> Result<Value, ToolError> {
    let source = a.source.trim().to_lowercase();
    if !matches!(source.as_str(), "github" | "telegram" | "direct") {
        return invalid("source must be one of: github, telegram, direct");
    }
    let Some(title) = clean(Some(a.title), MAX_TITLE) else {
        return invalid("title must not be empty");
    };
    let reference = clean(a.reference, 200);
    if let Some(r) = &reference {
        let existing: Option<i64> = tx
            .query_row(
                "SELECT id FROM tasks WHERE ref = ?1 AND status IN ('working', 'waiting_feedback')
                 ORDER BY id DESC LIMIT 1",
                [r],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(id) = existing {
            return Ok(json!({"id": id, "existing": true}));
        }
    }
    tx.execute(
        "INSERT INTO tasks (source, kind, title, ref, url, cron_job_id, status,
                            created_at, started_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'working', ?7, ?7, ?7)",
        params![
            source,
            clean(a.kind, 20).map(|k| k.to_lowercase()),
            title,
            reference,
            clean(a.url, 500),
            clean(a.cron_job_id, 64),
            ts
        ],
    )?;
    let id = tx.last_insert_rowid();
    if let Some(r) = &reference {
        // It's running now, so it's no longer queued.
        tx.execute("DELETE FROM queue_items WHERE ref = ?1", [r])?;
    }
    event(tx, ts, "task_started", Some(id), json!({"source": source}))?;
    Ok(json!({"id": id, "existing": false}))
}

#[derive(Deserialize)]
struct Target {
    id: Option<i64>,
    #[serde(rename = "ref")]
    reference: Option<String>,
}

/// Resolves to an unfinished task: by id, or the newest unfinished task with the ref.
fn resolve_open(tx: &Transaction, t: &Target) -> Result<i64, ToolError> {
    let (id, status): (i64, String) = match (&t.id, &t.reference) {
        (Some(id), _) => match tx
            .query_row("SELECT id, status FROM tasks WHERE id = ?1", [id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?
        {
            Some(row) => row,
            None => return invalid(format!("no task with id {id}")),
        },
        (None, Some(r)) => match tx
            .query_row(
                "SELECT id, status FROM tasks WHERE ref = ?1
                 AND status IN ('working', 'waiting_feedback') ORDER BY id DESC LIMIT 1",
                [r.trim()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
        {
            Some(row) => row,
            None => return invalid(format!("no unfinished task with ref {r}")),
        },
        (None, None) => return invalid("pass either id or ref"),
    };
    if !matches!(status.as_str(), "working" | "waiting_feedback") {
        return invalid(format!("task {id} is already finished ({status})"));
    }
    Ok(id)
}

#[derive(Deserialize)]
struct UpdateArgs {
    #[serde(flatten)]
    target: Target,
    status: String,
    note: Option<String>,
}

fn task_update(tx: &Transaction, a: UpdateArgs, ts: i64) -> Result<Value, ToolError> {
    let status = a.status.trim();
    if !matches!(status, "working" | "waiting_feedback") {
        return invalid("status must be working or waiting_feedback");
    }
    let id = resolve_open(tx, &a.target)?;
    let previous: String =
        tx.query_row("SELECT status FROM tasks WHERE id = ?1", [id], |r| r.get(0))?;
    tx.execute(
        "UPDATE tasks SET status = ?1, note = ?2, updated_at = ?3 WHERE id = ?4",
        params![status, clean(a.note, MAX_NOTE), ts, id],
    )?;
    let kind = if status == "waiting_feedback" && previous != "waiting_feedback" {
        "feedback_needed"
    } else {
        "task_updated"
    };
    event(tx, ts, kind, Some(id), json!({"status": status}))?;
    Ok(json!({"id": id, "status": status}))
}

#[derive(Deserialize)]
struct FinishArgs {
    #[serde(flatten)]
    target: Target,
    outcome: String,
    summary: Option<String>,
    pr_url: Option<String>,
}

fn task_finish(tx: &Transaction, a: FinishArgs, ts: i64) -> Result<Value, ToolError> {
    let outcome = a.outcome.trim();
    if !matches!(outcome, "done" | "failed" | "skipped") {
        return invalid("outcome must be done, failed or skipped");
    }
    let id = resolve_open(tx, &a.target)?;
    finish(
        tx,
        id,
        outcome,
        clean(a.summary, MAX_NOTE),
        clean(a.pr_url, 500),
        false,
        ts,
    )?;
    Ok(json!({"id": id, "outcome": outcome}))
}

/// Shared by `task_finish` and reconciliation.
pub(crate) fn finish(
    tx: &Transaction,
    id: i64,
    outcome: &str,
    summary: Option<String>,
    pr_url: Option<String>,
    reconciled: bool,
    ts: i64,
) -> Result<(), ToolError> {
    tx.execute(
        "UPDATE tasks SET status = ?1, summary = COALESCE(?2, summary), pr_url = COALESCE(?3, pr_url),
                          finished_at = ?4, updated_at = ?4, reconciled = ?5
         WHERE id = ?6",
        params![outcome, summary, pr_url, ts, reconciled, id],
    )?;
    event(
        tx,
        ts,
        "task_finished",
        Some(id),
        json!({"outcome": outcome, "reconciled": reconciled}),
    )
}

#[derive(Deserialize, PartialEq, Debug)]
struct QueueItem {
    #[serde(rename = "ref")]
    reference: Option<String>,
    mode: Option<String>,
    title: String,
    url: Option<String>,
}

#[derive(Deserialize)]
struct QueueArgs {
    items: Vec<QueueItem>,
}

fn queue_set(tx: &Transaction, a: QueueArgs, ts: i64) -> Result<Value, ToolError> {
    let mut items = Vec::with_capacity(a.items.len());
    for (i, it) in a.items.into_iter().enumerate() {
        let Some(title) = clean(Some(it.title), MAX_TITLE) else {
            return invalid(format!("items[{i}].title must not be empty"));
        };
        items.push(QueueItem {
            reference: clean(it.reference, 200),
            mode: clean(it.mode, 20).map(|m| m.to_lowercase()),
            title,
            url: clean(it.url, 500),
        });
    }
    let current: Vec<QueueItem> = {
        let mut stmt =
            tx.prepare("SELECT ref, mode, title, url FROM queue_items ORDER BY position")?;
        stmt.query_map([], |r| {
            Ok(QueueItem {
                reference: r.get(0)?,
                mode: r.get(1)?,
                title: r.get(2)?,
                url: r.get(3)?,
            })
        })?
        .collect::<Result<_, _>>()?
    };
    if current == items {
        return Ok(json!({"changed": false, "count": items.len()}));
    }
    tx.execute("DELETE FROM queue_items", [])?;
    for (pos, it) in items.iter().enumerate() {
        tx.execute(
            "INSERT INTO queue_items (position, ref, mode, title, url, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![pos as i64, it.reference, it.mode, it.title, it.url, ts],
        )?;
    }
    event(tx, ts, "queue_changed", None, json!({"count": items.len()}))?;
    Ok(json!({"changed": true, "count": items.len()}))
}

#[derive(Deserialize)]
struct ClosedArgs {
    #[serde(rename = "ref")]
    reference: String,
}

fn task_closed(tx: &Transaction, a: ClosedArgs, ts: i64) -> Result<Value, ToolError> {
    let r = a.reference.trim();
    if r.is_empty() {
        return invalid("ref must not be empty");
    }
    let n = tx.execute(
        "UPDATE tasks SET closed_at = ?1 WHERE ref = ?2 AND closed_at IS NULL",
        params![ts, r],
    )?;
    tx.execute("DELETE FROM queue_items WHERE ref = ?1", [r])?;
    if n > 0 {
        event(tx, ts, "task_closed", None, json!({"ref": r}))?;
    }
    Ok(json!({"closed": n}))
}

#[derive(Deserialize)]
struct DisplayOffArgs {
    until: Option<String>,
}

fn display_off(
    tx: &Transaction,
    schedule: &Schedule,
    a: DisplayOffArgs,
    ts: i64,
) -> Result<Value, ToolError> {
    let until = match a.until.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None => None,
        Some(s) => {
            let t = parse_time(s)?;
            if t <= ts {
                return invalid(format!("until ({s}) is in the past"));
            }
            Some(t)
        }
    };
    let stored = read_display(tx)?;
    if stored.mode == Mode::Vacation {
        // Vacation holds until display_on.
        return Ok(display_reply(&stored, schedule, ts, false));
    }
    set_display(tx, schedule, Mode::Off, until, ts)
}

/// Stores the owner's request (and an event) and describes the result. `changed` says
/// whether the monitor switches now.
fn set_display(
    tx: &Transaction,
    schedule: &Schedule,
    mode: Mode,
    until: Option<i64>,
    ts: i64,
) -> Result<Value, ToolError> {
    let before = wants_off(&read_display(tx)?, schedule, ts);
    let next = Request {
        mode,
        since: ts,
        until,
    };
    write_display(tx, &next)?;
    let changed = wants_off(&next, schedule, ts) != before;
    let kind = format!("display_{}", mode.as_str());
    event(tx, ts, &kind, None, json!({"until": until}))?;
    Ok(display_reply(&next, schedule, ts, changed))
}

fn display_reply(req: &Request, schedule: &Schedule, ts: i64, changed: bool) -> Value {
    let off = wants_off(req, schedule, ts);
    json!({
        "display": if off { "off" } else { "on" },
        "until": next_change(req, schedule, ts).map(local_time),
        "vacation": req.mode == Mode::Vacation,
        "changed": changed,
    })
}

/// RFC 3339, or a date and time without an offset in the host's local time zone.
fn parse_time(s: &str) -> Result<i64, ToolError> {
    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(s) {
        return Ok(t.timestamp());
    }
    const LOCAL: &[&str] = &[
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d %H:%M",
    ];
    for f in LOCAL {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, f)
            && let Some(t) = naive.and_local_timezone(chrono::Local).earliest()
        {
            return Ok(t.timestamp());
        }
    }
    invalid(format!(
        "until ({s}) must be a date and time like 2026-11-04T07:00:00+01:00"
    ))
}

fn local_time(ts: i64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_opt(ts, 0)
        .single()
        .map(|t| t.to_rfc3339())
        .unwrap_or_else(|| ts.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open_in_memory().unwrap()
    }

    fn ok(s: &mut Store, name: &str, args: Value) -> Value {
        call_at(s, name, args, 1000).unwrap_or_else(|e| panic!("{name}: {e}"))
    }

    fn err(s: &mut Store, name: &str, args: Value) -> String {
        call_at(s, name, args, 1000).unwrap_err().to_string()
    }

    fn status(s: &Store, id: i64) -> String {
        s.conn
            .query_row("SELECT status FROM tasks WHERE id = ?1", [id], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn definitions_have_object_schemas() {
        let defs = definitions();
        assert_eq!(defs.len(), 8);
        for d in defs {
            assert_eq!(d.input_schema["type"], "object", "{}", d.name);
            assert!(!d.description.is_empty());
        }
    }

    #[test]
    fn start_update_finish_lifecycle() {
        let mut s = store();
        let id = ok(
            &mut s,
            "task_start",
            json!({"source": "telegram", "title": "  Fix   the\nbuild "}),
        )["id"]
            .as_i64()
            .unwrap();
        let title: String = s
            .conn
            .query_row("SELECT title FROM tasks WHERE id=?1", [id], |r| r.get(0))
            .unwrap();
        assert_eq!(title, "Fix the build");
        ok(
            &mut s,
            "task_update",
            json!({"id": id, "status": "waiting_feedback", "note": "Which branch?"}),
        );
        assert_eq!(status(&s, id), "waiting_feedback");
        ok(
            &mut s,
            "task_update",
            json!({"id": id, "status": "working"}),
        );
        ok(
            &mut s,
            "task_finish",
            json!({"id": id, "outcome": "done", "summary": "answered"}),
        );
        assert_eq!(status(&s, id), "done");
        assert!(
            err(&mut s, "task_finish", json!({"id": id, "outcome": "done"}))
                .contains("already finished")
        );
        let kinds = s.event_kinds_since(0).unwrap();
        assert_eq!(
            kinds,
            [
                "task_started",
                "feedback_needed",
                "task_updated",
                "task_finished"
            ]
        );
    }

    #[test]
    fn github_start_is_idempotent_per_ref_and_dequeues() {
        let mut s = store();
        ok(
            &mut s,
            "queue_set",
            json!({"items": [
                {"ref": "o/r#1", "mode": "new", "title": "One"},
                {"ref": "o/r#2", "mode": "new", "title": "Two"}
            ]}),
        );
        let a = ok(
            &mut s,
            "task_start",
            json!({"source": "github", "title": "One", "ref": "o/r#1", "cron_job_id": "job1"}),
        );
        let b = ok(
            &mut s,
            "task_start",
            json!({"source": "github", "title": "One again", "ref": "o/r#1"}),
        );
        assert_eq!(a["id"], b["id"]);
        assert_eq!(b["existing"], true);
        let queued: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM queue_items", [], |r| r.get(0))
            .unwrap();
        assert_eq!(queued, 1);
    }

    #[test]
    fn ref_resolves_to_newest_unfinished_task() {
        let mut s = store();
        let first = ok(
            &mut s,
            "task_start",
            json!({"source": "github", "title": "new", "ref": "o/r#5"}),
        )["id"]
            .as_i64()
            .unwrap();
        ok(
            &mut s,
            "task_finish",
            json!({"ref": "o/r#5", "outcome": "done", "pr_url": "https://github.com/o/r/pull/9"}),
        );
        assert_eq!(status(&s, first), "done");
        let second = ok(
            &mut s,
            "task_start",
            json!({"source": "github", "title": "update", "ref": "o/r#5", "kind": "update"}),
        )["id"]
            .as_i64()
            .unwrap();
        assert_ne!(first, second);
        ok(
            &mut s,
            "task_finish",
            json!({"ref": "o/r#5", "outcome": "failed"}),
        );
        assert_eq!(status(&s, second), "failed");
        assert!(
            err(
                &mut s,
                "task_finish",
                json!({"ref": "o/r#5", "outcome": "done"})
            )
            .contains("no unfinished task")
        );
    }

    #[test]
    fn unchanged_queue_records_no_event() {
        let mut s = store();
        let items = json!({"items": [{"ref": "o/r#1", "title": "One"}]});
        assert_eq!(ok(&mut s, "queue_set", items.clone())["changed"], true);
        assert_eq!(ok(&mut s, "queue_set", items)["changed"], false);
        assert_eq!(s.event_kinds_since(0).unwrap(), ["queue_changed"]);
        assert_eq!(
            ok(&mut s, "queue_set", json!({"items": []}))["changed"],
            true
        );
    }

    #[test]
    fn closed_marks_tasks_and_dequeues() {
        let mut s = store();
        ok(
            &mut s,
            "task_start",
            json!({"source": "github", "title": "x", "ref": "o/r#7"}),
        );
        ok(
            &mut s,
            "task_finish",
            json!({"ref": "o/r#7", "outcome": "done"}),
        );
        assert_eq!(
            ok(&mut s, "task_closed", json!({"ref": "o/r#7"}))["closed"],
            1
        );
        assert_eq!(
            ok(&mut s, "task_closed", json!({"ref": "o/r#7"}))["closed"],
            0
        );
    }

    #[test]
    fn validation_errors_are_clear() {
        let mut s = store();
        assert!(
            err(
                &mut s,
                "task_start",
                json!({"source": "email", "title": "x"})
            )
            .contains("source must be")
        );
        assert!(
            err(
                &mut s,
                "task_start",
                json!({"source": "direct", "title": "   "})
            )
            .contains("title must not be empty")
        );
        assert!(
            err(&mut s, "task_start", json!({"title": "x"})).contains("missing field `source`")
        );
        assert!(
            err(&mut s, "task_update", json!({"status": "working"})).contains("either id or ref")
        );
        assert!(
            err(
                &mut s,
                "task_update",
                json!({"id": 99, "status": "working"})
            )
            .contains("no task with id 99")
        );
        assert!(
            err(
                &mut s,
                "task_update",
                json!({"id": 1, "status": "sleeping"})
            )
            .contains("status must be")
        );
        assert!(err(&mut s, "nope", json!({})).contains("unknown tool"));
        assert!(err(&mut s, "task_start", json!([1, 2])).contains("JSON object"));
    }

    fn display(s: &Store) -> Request {
        read_display(&s.conn).unwrap()
    }

    #[test]
    fn display_off_and_on_without_a_schedule() {
        let mut s = store();
        let r = ok(&mut s, "display_off", json!({}));
        assert_eq!(
            r,
            json!({"display": "off", "until": null, "vacation": false, "changed": true})
        );
        assert_eq!(display(&s).mode, Mode::Off);
        // Repeating it switches nothing.
        assert_eq!(ok(&mut s, "display_off", json!({}))["changed"], false);

        let r = ok(
            &mut s,
            "display_off",
            json!({"until": "1970-01-01T02:00:00+01:00"}),
        );
        assert_eq!(r["changed"], false, "it was already off");
        assert_eq!(display(&s).until, Some(3600));
        let shown = chrono::DateTime::parse_from_rfc3339(r["until"].as_str().unwrap()).unwrap();
        assert_eq!(shown.timestamp(), 3600);

        assert_eq!(
            ok(&mut s, "display_on", json!({})),
            json!({"display": "on", "until": null, "vacation": false, "changed": true})
        );
        assert_eq!(ok(&mut s, "display_on", json!({}))["changed"], false);
        assert_eq!(
            s.event_kinds_since(0).unwrap(),
            [
                "display_off",
                "display_off",
                "display_off",
                "display_on",
                "display_on"
            ]
        );

        // Once `until` has passed the monitor is on: display_on switches nothing.
        ok(
            &mut s,
            "display_off",
            json!({"until": "1970-01-01T02:00:00+01:00"}),
        );
        let r = call_at(&mut s, "display_on", json!({}), 5000).unwrap();
        assert_eq!(r["changed"], false);
    }

    /// 2030-01-`day` `hh`:00 in the local time zone.
    fn local(day: u32, hh: u32) -> i64 {
        chrono::NaiveDate::from_ymd_opt(2030, 1, day)
            .unwrap()
            .and_hms_opt(hh, 0, 0)
            .unwrap()
            .and_local_timezone(chrono::Local)
            .earliest()
            .unwrap()
            .timestamp()
    }

    #[test]
    fn display_tools_follow_the_schedule_and_vacation() {
        let mut s = store();
        let nightly = Schedule {
            off_at: chrono::NaiveTime::from_hms_opt(23, 0, 0),
            on_at: chrono::NaiveTime::from_hms_opt(9, 0, 0),
        };
        let mut at = |name: &str, args: Value, ts: i64| {
            call_with(&mut s, &nightly, name, args, ts).unwrap_or_else(|e| panic!("{name}: {e}"))
        };
        let until = |r: &Value| {
            r["until"]
                .as_str()
                .map(|u| chrono::DateTime::parse_from_rfc3339(u).unwrap().timestamp())
        };

        // Awake at night: on, until the next night's switch.
        let r = at("display_on", json!({}), local(10, 1));
        assert_eq!(
            (r["display"].as_str(), r["changed"].as_bool()),
            (Some("on"), Some(true))
        );
        assert_eq!(until(&r), Some(local(10, 23)));
        // Off in the morning: back on at 09:00.
        let r = at("display_off", json!({}), local(11, 8));
        assert_eq!(until(&r), Some(local(11, 9)));
        // Vacation: off with no end, and display_off doesn't end it.
        let r = at("display_vacation", json!({}), local(11, 12));
        assert_eq!(r["vacation"], true);
        assert_eq!(r["changed"], true);
        assert_eq!(until(&r), None);
        let r = at(
            "display_off",
            json!({"until": "2030-01-20T10:00:00Z"}),
            local(12, 12),
        );
        assert_eq!(
            (r["vacation"].as_bool(), r["changed"].as_bool()),
            (Some(true), Some(false))
        );
        assert_eq!(display(&s).mode, Mode::Vacation);
    }

    #[test]
    fn display_off_until_validation() {
        let mut s = store();
        assert!(
            err(
                &mut s,
                "display_off",
                json!({"until": "1970-01-01T00:10:00Z"})
            )
            .contains("in the past")
        );
        assert!(
            err(&mut s, "display_off", json!({"until": "tomorrow morning"}))
                .contains("must be a date and time")
        );
        // Blank means no end time; a local time without an offset is accepted.
        assert_eq!(
            ok(&mut s, "display_off", json!({"until": " "}))["until"],
            Value::Null
        );
        assert!(parse_time("2030-01-02T07:00").is_ok());
        assert!(parse_time("2030-01-02 07:00:00").is_ok());
        assert_eq!(parse_time("2030-01-02T07:00:00Z").unwrap(), 1_893_567_600);
    }

    #[test]
    fn long_titles_are_truncated() {
        let mut s = store();
        let id = ok(
            &mut s,
            "task_start",
            json!({"source": "direct", "title": "x".repeat(1000)}),
        )["id"]
            .as_i64()
            .unwrap();
        let title: String = s
            .conn
            .query_row("SELECT title FROM tasks WHERE id=?1", [id], |r| r.get(0))
            .unwrap();
        assert_eq!(title.chars().count(), MAX_TITLE);
        assert!(title.ends_with('…'));
    }
}
