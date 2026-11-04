//! Turns store snapshots, metrics and health into the rows the screen shows.
//! Pure functions, so every display rule is unit-tested.

use chrono::TimeZone;
use slint::SharedString;

use super::{DoneItem, MetricItem, NeedItem, TaskItem};
use crate::config::{Animal, UiConfig};
use crate::health::Health;
use crate::mask::mask;
use crate::metrics::Metrics;
use crate::store::views::{NeedKind, Snapshot, TaskRow};

// Tones (see ui/theme.slint).
pub const WORKING: i32 = 1;
pub const ATTENTION: i32 = 2;
pub const DANGER: i32 = 3;
pub const OK: i32 = 4;
pub const DIM: i32 = 5;

/// Rows that fit in the tasks panel at 1080p (plus one hint line).
pub const TASK_ROWS: usize = 5;
/// Two-line rows in the needs-you section.
pub const NEED_ROWS: usize = 3;

/// "45s" → "<1m", "12m", "3h 5m", "2d 4h".
pub fn duration(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        "<1m".into()
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86_400 {
        format!("{}h {}m", s / 3600, (s % 3600) / 60)
    } else {
        format!("{}d {}h", s / 86_400, (s % 86_400) / 3600)
    }
}

/// "just now", "12m ago", "2d ago" (coarser than `duration`).
pub fn ago(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        "just now".into()
    } else if s < 3600 {
        format!("{}m ago", s / 60)
    } else if s < 86_400 {
        format!("{}h ago", s / 3600)
    } else {
        format!("{}d ago", s / 86_400)
    }
}

fn badge(source: &str) -> &'static str {
    match source {
        "github" => "GH",
        "telegram" => "TG",
        _ => "CLI",
    }
}

fn kind_label(kind: Option<&str>) -> Option<String> {
    match kind? {
        "new" => Some("new PR".into()),
        "update" => Some("update".into()),
        "go" => Some("GO".into()),
        "review" => Some("review".into()),
        "chat" | "" => None,
        other => Some(mask(other)),
    }
}

fn join(parts: impl IntoIterator<Item = Option<String>>) -> String {
    parts
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("  ·  ")
}

fn pr_label(url: &str) -> String {
    match url.rsplit_once("/pull/") {
        Some((_, n)) if n.chars().all(|c| c.is_ascii_digit()) && !n.is_empty() => {
            format!("PR #{n}")
        }
        _ => "PR".into(),
    }
}

fn hint_more(n: usize) -> String {
    if n == 0 {
        String::new()
    } else {
        format!("+{n} more")
    }
}

fn task_item(t: &TaskRow, now: i64, stale_after_secs: i64) -> TaskItem {
    let (status, tone) = if t.status == "waiting_feedback" {
        (
            format!("needs you · {}", duration(now - t.updated_at)),
            ATTENTION,
        )
    } else if t.cron_job_id.is_none() && now - t.updated_at > stale_after_secs {
        (format!("stale? · {}", duration(now - t.updated_at)), DIM)
    } else {
        (
            format!("working · {}", duration(now - t.started_at)),
            WORKING,
        )
    };
    TaskItem {
        badge: badge(&t.source).into(),
        title: mask(&t.title).into(),
        meta: join([
            t.reference.as_deref().map(mask),
            kind_label(t.kind.as_deref()),
            t.note.as_deref().map(mask),
        ])
        .into(),
        status: status.into(),
        tone,
    }
}

/// Running tasks, then queued ones, capped to the panel; returns (rows, hint).
pub fn task_items(snap: &Snapshot, now: i64, stale_after_secs: i64) -> (Vec<TaskItem>, String) {
    let mut rows: Vec<TaskItem> = snap
        .running
        .iter()
        .map(|t| task_item(t, now, stale_after_secs))
        .collect();
    rows.extend(snap.queue.iter().map(|q| {
        TaskItem {
            badge: if q.reference.is_some() { "GH" } else { "CLI" }.into(),
            title: mask(&q.title).into(),
            meta: join([
                q.reference.as_deref().map(mask),
                kind_label(q.mode.as_deref()),
            ])
            .into(),
            status: "queued".into(),
            tone: DIM,
        }
    }));
    if rows.is_empty() {
        return (rows, "Nothing running and nothing queued".into());
    }
    let extra = rows.len().saturating_sub(TASK_ROWS);
    rows.truncate(TASK_ROWS);
    (rows, hint_more(extra))
}

/// "Needs you" rows; returns (rows, hint).
pub fn need_items(snap: &Snapshot, now: i64) -> (Vec<NeedItem>, String) {
    let rows: Vec<NeedItem> = snap
        .needs
        .iter()
        .take(NEED_ROWS)
        .map(|n| {
            let t = &n.task;
            let r = t.reference.as_deref().map(mask);
            let finished_ago = duration(now - t.finished_at.unwrap_or(t.updated_at));
            let (label, tone, detail, age) = match n.kind {
                NeedKind::Feedback => (
                    "WAITING",
                    ATTENTION,
                    join([
                        r,
                        Some(
                            t.note
                                .as_deref()
                                .map(mask)
                                .unwrap_or_else(|| "waiting for your reply".into()),
                        ),
                    ]),
                    duration(now - t.updated_at),
                ),
                NeedKind::Skipped => (
                    "DECIDE",
                    ATTENTION,
                    join([
                        r,
                        Some(
                            t.summary
                                .as_deref()
                                .map(mask)
                                .unwrap_or_else(|| "needs a decision".into()),
                        ),
                    ]),
                    finished_ago,
                ),
                NeedKind::Failed => (
                    "FAILED",
                    DANGER,
                    join([
                        r,
                        Some(
                            t.summary
                                .as_deref()
                                .map(mask)
                                .unwrap_or_else(|| "failed".into()),
                        ),
                    ]),
                    finished_ago,
                ),
                NeedKind::Review => (
                    "REVIEW",
                    WORKING,
                    join([
                        r,
                        Some(format!(
                            "{} ready for review",
                            pr_label(t.pr_url.as_deref().unwrap_or(""))
                        )),
                    ]),
                    finished_ago,
                ),
            };
            NeedItem {
                label: label.into(),
                title: mask(&t.title).into(),
                detail: detail.into(),
                age: age.into(),
                tone,
            }
        })
        .collect();
    if snap.needs.is_empty() {
        return (rows, "Nothing needs you right now".into());
    }
    let hidden = &snap.needs[rows.len()..];
    let reviews = hidden.iter().filter(|n| n.kind == NeedKind::Review).count();
    let hint = match (hidden.len(), reviews) {
        (0, _) => String::new(),
        (n, 0) => format!("+{n} more"),
        (n, r) if r == n => format!("+{r} more PRs to review"),
        (n, r) => format!("+{n} more ({r} PRs to review)"),
    };
    (rows, hint)
}

/// How many finished rows fit under the needs-you section.
pub fn done_rows(needs_shown: usize, needs_hint: bool, configured: usize) -> usize {
    // Pixel budget of the bottom-left panel at 1080p (see ui/main.slint).
    const BUDGET: i64 = 348;
    const NEED_ROW: i64 = 66;
    const HINT: i64 = 35;
    const DONE_ROW: i64 = 41;
    let used = needs_shown as i64 * NEED_ROW + if needs_hint { HINT } else { 0 };
    (((BUDGET - used) / DONE_ROW).max(1) as usize).min(configured.max(1))
}

pub fn done_items(snap: &Snapshot, now: i64, max_rows: usize) -> (Vec<DoneItem>, String) {
    let rows: Vec<DoneItem> = snap
        .finished
        .iter()
        .take(max_rows)
        .map(|t| {
            let (outcome, tone) = match t.status.as_str() {
                "failed" => ("failed".to_owned(), DANGER),
                "skipped" => ("skipped".to_owned(), ATTENTION),
                _ => {
                    let o = match (t.kind.as_deref(), t.pr_url.is_some()) {
                        (Some("new"), true) => "PR opened",
                        (Some("update"), _) => "updated",
                        (Some("go"), _) => "GO done",
                        (Some("review"), _) => "reviewed",
                        (_, true) => "PR opened",
                        _ => "done",
                    };
                    (o.to_owned(), OK)
                }
            };
            let outcome = if t.reconciled {
                format!("{outcome} (auto)")
            } else {
                outcome
            };
            let title = match &t.reference {
                Some(r) => format!("{}  ·  {}", mask(&t.title), mask(r)),
                None => mask(&t.title),
            };
            DoneItem {
                badge: badge(&t.source).into(),
                title: title.into(),
                outcome: outcome.into(),
                age: ago(now - t.finished_at.unwrap_or(now)).into(),
                tone,
            }
        })
        .collect();
    let hint = if rows.is_empty() {
        "Nothing finished yet".into()
    } else {
        String::new()
    };
    (rows, hint)
}

fn pct(v: Option<f32>) -> String {
    v.map(|v| format!("{:.0}%", v * 100.0))
        .unwrap_or_else(|| "—".into())
}

fn gb(kb: u64) -> String {
    format!("{:.1}", kb as f64 / 1024.0 / 1024.0)
}

fn mbs(bps: f64) -> String {
    format!("{:.1}", bps / 1_000_000.0)
}

pub fn metric_items(m: &Metrics, gpu_shown: bool, loadavg: Option<String>) -> Vec<MetricItem> {
    let s = &m.latest;
    vec![
        MetricItem {
            label: "CPU".into(),
            value: s.cpu.unwrap_or(0.0),
            text: pct(s.cpu).into(),
            detail: loadavg
                .map(|l| format!("load {l}"))
                .unwrap_or_default()
                .into(),
            spark: m.cpu.sparkline_path().into(),
            shown: true,
        },
        MetricItem {
            label: "RAM".into(),
            value: s.mem.map(|x| x.used_fraction()).unwrap_or(0.0),
            text: pct(s.mem.map(|x| x.used_fraction())).into(),
            detail: s
                .mem
                .map(|x| format!("{} / {} GB", gb(x.used_kb()), gb(x.total_kb)))
                .unwrap_or_default()
                .into(),
            spark: m.mem.sparkline_path().into(),
            shown: true,
        },
        MetricItem {
            label: "DISK".into(),
            value: s.disk.map(|d| d.busy).unwrap_or(0.0),
            text: pct(s.disk.map(|d| d.busy)).into(),
            detail: s
                .disk
                .map(|d| format!("R {}  W {} MB/s", mbs(d.read_bps), mbs(d.write_bps)))
                .unwrap_or_default()
                .into(),
            spark: m.disk.sparkline_path().into(),
            shown: true,
        },
        MetricItem {
            label: "GPU".into(),
            value: s.gpu.unwrap_or(0.0),
            text: pct(s.gpu).into(),
            detail: SharedString::new(),
            spark: m.gpu.sparkline_path().into(),
            shown: gpu_shown,
        },
    ]
}

/// First three fields of /proc/loadavg.
pub fn loadavg() -> Option<String> {
    let s = std::fs::read_to_string("/proc/loadavg").ok()?;
    Some(s.split_whitespace().take(3).collect::<Vec<_>>().join(" "))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mood {
    Working,
    Idle,
    NeedsYou,
    Broken,
}

/// Broken beats needs-you beats working beats idle. PRs waiting for review
/// alone don't raise the sign (there are usually some).
pub fn mood(health: &Health, snap: &Snapshot) -> Mood {
    if !health.ok() {
        Mood::Broken
    } else if snap.needs.iter().any(|n| n.kind != NeedKind::Review) {
        Mood::NeedsYou
    } else if !snap.running.is_empty() {
        Mood::Working
    } else {
        Mood::Idle
    }
}

pub fn mascot_art(a: Animal, m: Mood) -> &'static str {
    match (a, m) {
        (Animal::Beaver, Mood::Working) => include_str!("../../ui/art/beaver/working.txt"),
        (Animal::Beaver, Mood::Idle) => include_str!("../../ui/art/beaver/idle.txt"),
        (Animal::Beaver, Mood::NeedsYou) => include_str!("../../ui/art/beaver/needs.txt"),
        (Animal::Beaver, Mood::Broken) => include_str!("../../ui/art/beaver/broken.txt"),
        (Animal::Squirrel, Mood::Working) => include_str!("../../ui/art/squirrel/working.txt"),
        (Animal::Squirrel, Mood::Idle) => include_str!("../../ui/art/squirrel/idle.txt"),
        (Animal::Squirrel, Mood::NeedsYou) => include_str!("../../ui/art/squirrel/needs.txt"),
        (Animal::Squirrel, Mood::Broken) => include_str!("../../ui/art/squirrel/broken.txt"),
    }
    .trim_end_matches('\n')
}

/// Font size of an animal's frames, in px. Larger art gets a smaller font, so that
/// every animal leaves the same room for the caption and the counter.
pub fn mascot_font_px(a: Animal) -> f32 {
    match a {
        Animal::Beaver => 22.0,
        Animal::Squirrel => 14.0,
    }
}

/// The animal panel's title: `animal_name` if set, else the animal's own name.
pub fn mascot_title(ui: &UiConfig) -> String {
    let default = match ui.animal {
        Animal::Beaver => "Beaver",
        Animal::Squirrel => "Squirrel",
    };
    ui.animal_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(default)
        .to_uppercase()
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

pub fn mascot_caption(m: Mood, snap: &Snapshot) -> (String, i32) {
    let reviews = snap
        .needs
        .iter()
        .filter(|n| n.kind == NeedKind::Review)
        .count();
    match m {
        Mood::Broken => ("Something's broken: see the banner".into(), DANGER),
        Mood::NeedsYou => {
            let n = snap
                .needs
                .iter()
                .filter(|n| n.kind != NeedKind::Review)
                .count();
            (
                format!(
                    "{} need{} you",
                    plural(n, "thing", "things"),
                    if n == 1 { "s" } else { "" }
                ),
                ATTENTION,
            )
        }
        Mood::Working => {
            let mut s = format!("Chewing on {}", plural(snap.running.len(), "task", "tasks"));
            if !snap.queue.is_empty() {
                s.push_str(&format!(", {} queued", snap.queue.len()));
            }
            (s, WORKING)
        }
        Mood::Idle if reviews > 0 => (
            format!(
                "Napping. {} waiting for your review",
                plural(reviews, "PR", "PRs")
            ),
            DIM,
        ),
        Mood::Idle => ("Napping. Nothing to do".into(), DIM),
    }
}

pub fn counter_label(go_live_at: i64) -> String {
    let date = chrono::Local
        .timestamp_opt(go_live_at, 0)
        .single()
        .map(|d| d.format("%-d %b %Y").to_string())
        .unwrap_or_default();
    format!("tasks finished since {date}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::views::{NeedRow, QueueRow};

    fn task(id: i64, source: &str, status: &str) -> TaskRow {
        TaskRow {
            id,
            source: source.into(),
            kind: None,
            title: format!("task {id}"),
            reference: None,
            url: None,
            pr_url: None,
            cron_job_id: None,
            status: status.into(),
            note: None,
            summary: None,
            started_at: 0,
            updated_at: 0,
            finished_at: None,
            reconciled: false,
        }
    }

    #[test]
    fn durations() {
        assert_eq!(duration(30), "<1m");
        assert_eq!(duration(12 * 60 + 5), "12m");
        assert_eq!(duration(3 * 3600 + 5 * 60), "3h 5m");
        assert_eq!(duration(2 * 86_400 + 4 * 3600), "2d 4h");
        assert_eq!(ago(10), "just now");
        assert_eq!(ago(7200), "2h ago");
    }

    #[test]
    fn task_status_tones() {
        let mut working = task(1, "github", "working");
        working.cron_job_id = Some("j".into());
        working.started_at = 0;
        working.updated_at = 0;
        let mut waiting = task(2, "telegram", "waiting_feedback");
        waiting.updated_at = 3000;
        let stale = task(3, "telegram", "working");
        let snap = Snapshot {
            running: vec![working, waiting, stale],
            ..Default::default()
        };
        let (rows, hint) = task_items(&snap, 3600, 1800);
        assert_eq!(hint, "");
        assert_eq!(rows[0].status, "working · 1h 0m");
        assert_eq!(rows[0].tone, WORKING);
        assert_eq!(rows[1].status, "needs you · 10m");
        assert_eq!(rows[1].tone, ATTENTION);
        // Telegram task without updates for an hour is stale; cron tasks never are.
        assert_eq!(rows[2].status, "stale? · 1h 0m");
        assert_eq!(rows[2].badge, "TG");
    }

    #[test]
    fn tasks_overflow_and_empty() {
        let (rows, hint) = task_items(&Snapshot::default(), 0, 1800);
        assert!(rows.is_empty());
        assert_eq!(hint, "Nothing running and nothing queued");

        let snap = Snapshot {
            running: (0..3).map(|i| task(i, "github", "working")).collect(),
            queue: (0..4)
                .map(|i| QueueRow {
                    reference: Some(format!("o/r#{i}")),
                    mode: Some("new".into()),
                    title: format!("q{i}"),
                })
                .collect(),
            ..Default::default()
        };
        let (rows, hint) = task_items(&snap, 0, 1800);
        assert_eq!(rows.len(), TASK_ROWS);
        assert_eq!(hint, "+2 more");
        assert_eq!(rows[3].status, "queued");
        assert_eq!(rows[3].meta, "o/r#0  ·  new PR");
    }

    #[test]
    fn needs_rows_and_hint() {
        let mut fb = task(1, "telegram", "waiting_feedback");
        fb.note = Some("Use token=ghp_abcdefghijklmnopqrstuvwxyz123456?".into());
        let mut review = task(2, "github", "done");
        review.reference = Some("o/r#2".into());
        review.pr_url = Some("https://github.com/o/r/pull/41".into());
        review.finished_at = Some(0);
        let snap = Snapshot {
            needs: vec![
                NeedRow {
                    kind: NeedKind::Feedback,
                    task: fb,
                },
                NeedRow {
                    kind: NeedKind::Review,
                    task: review.clone(),
                },
                NeedRow {
                    kind: NeedKind::Review,
                    task: review.clone(),
                },
                NeedRow {
                    kind: NeedKind::Review,
                    task: review.clone(),
                },
                NeedRow {
                    kind: NeedKind::Review,
                    task: review,
                },
            ],
            ..Default::default()
        };
        let (rows, hint) = need_items(&snap, 7200);
        assert_eq!(rows.len(), NEED_ROWS);
        assert_eq!(rows[0].label, "WAITING");
        assert!(rows[0].detail.contains("token=••••"), "{}", rows[0].detail);
        assert_eq!(rows[1].detail, "o/r#2  ·  PR #41 ready for review");
        assert_eq!(hint, "+2 more PRs to review");
        let (_, empty) = need_items(&Snapshot::default(), 0);
        assert_eq!(empty, "Nothing needs you right now");
    }

    #[test]
    fn done_rows_fit_the_panel() {
        assert_eq!(done_rows(0, true, 8), 7);
        assert_eq!(done_rows(3, true, 8), 2);
        assert_eq!(done_rows(0, true, 3), 3);
        assert_eq!(done_rows(5, true, 8), 1);
    }

    #[test]
    fn done_outcomes() {
        let mut a = task(1, "github", "done");
        a.kind = Some("new".into());
        a.pr_url = Some("u".into());
        a.reference = Some("o/r#1".into());
        a.finished_at = Some(0);
        let mut b = task(2, "github", "failed");
        b.reconciled = true;
        b.finished_at = Some(0);
        let mut c = task(3, "telegram", "done");
        c.finished_at = Some(0);
        let snap = Snapshot {
            finished: vec![a, b, c],
            ..Default::default()
        };
        let (rows, _) = done_items(&snap, 120, 8);
        assert_eq!(rows[0].outcome, "PR opened");
        assert_eq!(rows[0].title, "task 1  ·  o/r#1");
        assert_eq!(rows[1].outcome, "failed (auto)");
        assert_eq!(rows[1].tone, DANGER);
        assert_eq!(rows[2].outcome, "done");
        assert_eq!(rows[2].age, "2m ago");
    }

    #[test]
    fn mood_precedence() {
        let broken = Health {
            problems: vec!["x".into()],
        };
        let ok = Health::default();
        let mut snap = Snapshot::default();
        assert_eq!(mood(&ok, &snap), Mood::Idle);
        snap.running.push(task(1, "github", "working"));
        assert_eq!(mood(&ok, &snap), Mood::Working);
        snap.needs.push(NeedRow {
            kind: NeedKind::Review,
            task: task(2, "github", "done"),
        });
        assert_eq!(
            mood(&ok, &snap),
            Mood::Working,
            "reviews alone don't raise the sign"
        );
        snap.needs.push(NeedRow {
            kind: NeedKind::Failed,
            task: task(3, "github", "failed"),
        });
        assert_eq!(mood(&ok, &snap), Mood::NeedsYou);
        assert_eq!(mood(&broken, &snap), Mood::Broken);
        assert_eq!(mascot_caption(Mood::NeedsYou, &snap).0, "1 thing needs you");
    }

    #[test]
    fn mascot_frames_fit_the_panel() {
        // DejaVu Sans Mono's advance and line height, in em.
        const ADVANCE: f32 = 0.602;
        const LINE: f32 = 1.164;
        // The panel is about 705x400 px inside; the counter needs the rest of the width.
        const MAX_WIDTH: f32 = 480.0;
        const MAX_HEIGHT: f32 = 400.0;
        let moods = [Mood::Working, Mood::Idle, Mood::NeedsYou, Mood::Broken];
        for a in [Animal::Beaver, Animal::Squirrel] {
            let px = mascot_font_px(a);
            for m in moods {
                let art = mascot_art(a, m);
                let cols = art.lines().map(|l| l.chars().count()).max().unwrap();
                let rows = art.lines().count();
                assert!(rows >= 8, "{a:?} {m:?}");
                assert!(cols as f32 * ADVANCE * px <= MAX_WIDTH, "{a:?} {m:?}");
                assert!(rows as f32 * LINE * px <= MAX_HEIGHT, "{a:?} {m:?}");
            }
            let distinct: std::collections::HashSet<_> =
                moods.iter().map(|m| mascot_art(a, *m)).collect();
            assert_eq!(distinct.len(), moods.len(), "{a:?}");
        }
    }

    #[test]
    fn mascot_title_is_the_name_or_the_animal() {
        let mut ui = UiConfig::default();
        assert_eq!(mascot_title(&ui), "BEAVER");
        ui.animal = Animal::Squirrel;
        assert_eq!(mascot_title(&ui), "SQUIRREL");
        ui.animal_name = Some("Suseł".into());
        assert_eq!(mascot_title(&ui), "SUSEŁ");
        ui.animal_name = Some("  ".into());
        assert_eq!(mascot_title(&ui), "SQUIRREL");
    }
}
