# Connecting Bober to the dashboard

This is for Bober, the Hermes Agent instance that turns GitHub issues and Telegram requests into work. It lists the changes Bober makes **to its own setup** so that its work appears on the hermes-beaver dashboard. The dashboard never edits `~/.hermes` itself.

Read [MCP.md](MCP.md) first: it defines the tools, the `call` command and the rules (no secrets, never let reporting break work).

Make the changes in this order and check each one.

## 1. Register the MCP server

Add the `beaver` entry from MCP.md to `~/.hermes/config.yaml` under `mcp_servers`. Do **not** add it to any cron job's `enabled_toolsets`. Within about a minute, `mcp__beaver__*` tools appear in new turns.

## 2. Add the helper to the gate scripts

Put the `beaver()` helper from MCP.md into `~/.hermes/scripts/bober-issue-scan.py`. The slot-2 job runs the same script with `--slot 2`. Put it into `~/.hermes/scripts/pr-review-scan.py` as well.

Each slot needs its own cron job id for `cron_job_id`. The dashboard uses it to close tasks whose run ended without a report. Look the ids up in `~/.hermes/cron/jobs.json` by job name and keep them in the script, keyed by slot:

- `bober-issue-to-pr` → slot 1
- `bober-issue-to-pr-2` → slot 2
- `github-pr-auto-review` → the review script

## 3. Report from `bober-issue-scan.py`

Add these calls where the script already makes the matching decision:

| When the script… | Call |
|---|---|
| has computed the prioritised queue on a tick (update → go → new), **every tick, including when it is empty** | `beaver("queue_set", items=[{"ref": key, "mode": mode, "title": title, "url": issue_url}, …])`, listing every pending job **except** the one being dispatched now |
| dispatches a job | `beaver("task_start", source="github", kind=mode, title=title, ref=key, url=issue_url, cron_job_id=JOB_ID)` |
| sees an issue or PR closed or merged (where it marks the state `closed`) | `beaver("task_closed", ref=key)` |
| gives up on an update after repeated attempts (stalled) | `beaver("task_finish", ref=key, outcome="failed", summary="stalled after N attempts")` |

Notes:
- `key` is the existing state key, `owner/repo#N`. For GO jobs use the PR's key.
- The title comes from `issue.json` / `pr.json` in the work dir. Use the issue title only, never the body.
- A dispatch that fails before the agent starts (as with the provider-credential errors) needs no extra call. The dashboard closes the task from the cron ledger.

## 4. Report from `--mark`

The skill already has the agent run `--mark` at the end of a job. In the `--mark` handler, after the state is updated:

| `--mark` outcome | Call |
|---|---|
| `done` (PR opened) | `beaver("task_finish", ref=key, outcome="done", pr_url=pr_url)` |
| `updated` (review comments handled) | `beaver("task_finish", ref=key, outcome="done", pr_url=pr_url)` |
| GO `done` | `beaver("task_finish", ref=key, outcome="done", pr_url=pr_url)` |
| `skipped` (needs a decision) | `beaver("task_finish", ref=key, outcome="skipped", summary=<the question, one line>)` |
| `failed` | `beaver("task_finish", ref=key, outcome="failed", summary=<reason, one line>)` |

## 5. Report from `pr-review-scan.py`

Same lifecycle, with `kind="review"` and `cron_job_id` set to the review job's id:
- `task_start` when a review is dispatched;
- `task_finish(outcome="done")` when it is marked `done`;
- `task_finish(outcome="failed")` when it is `gave_up`.

## 6. Skills

Append this to the end of `~/.hermes/skills/software-development/bober-issue-to-pr/SKILL.md` and `~/.hermes/skills/software-development/github-auto-pr-review/SKILL.md`:

```markdown
## Dashboard

The gate script reports start and finish to the hermes-beaver dashboard; don't call
task_start or task_finish yourself. If you stop because you need the owner (a question
on the issue or PR), first call `mcp__beaver__task_update` with `ref` = the issue key,
`status` = `waiting_feedback` and `note` = your question in one line, then run `--mark`
as usual. Never put secrets in the note. If the call fails, ignore it.
```

Skills are re-read on every cron run, so no restart is needed.

## 7. SOUL.md: Telegram and CLI requests

Append this section to `~/.hermes/SOUL.md`:

```markdown
## Dashboard reporting

A wall dashboard (hermes-beaver) shows the owner what you are doing. In conversations
with the owner (Telegram, CLI, web UI):

- When a request needs real work (tools, more than a quick answer), call
  `mcp__beaver__task_start` first with `source` = `telegram` or `direct` and a short,
  neutral `title`. Keep the returned `id`.
- If you stop to wait for the owner's answer, call `mcp__beaver__task_update` with that
  `id`, `status` = `waiting_feedback` and the question as `note`. When the owner
  answers and you continue, set `status` back to `working`.
- When the work ends, call `mcp__beaver__task_finish` with that `id`, `outcome` =
  `done` or `failed`, and a one-line `summary`.
- Don't report plain chat replies. In Bober cron runs, don't call task_start or
  task_finish: the gate scripts do that.
- Never put secrets in titles, notes or summaries; the dashboard is on a wall.
- If a dashboard call fails, ignore it and carry on.
- The dashboard's monitor: when the owner says they are going to sleep or leaving
  the room, call `mcp__beaver__display_off`; if they say when they'll be back, pass
  that time as `until` (RFC 3339 with their UTC offset). When they leave for days
  (vacation, a trip), call `mcp__beaver__display_vacation`. When they are up or back,
  call `mcp__beaver__display_on`. Confirm with the result's `until` (when it switches
  back by itself). These aren't tasks: no task_start for them.
```

Open Telegram sessions keep their old system prompt. The owner needs to send `/new` once.

## 8. Quick commands `/sleep`, `/awake` and `/vacation`

Add these to `~/.hermes/config.yaml` under `quick_commands`, creating the key if it doesn't exist. They switch the monitor without a model call, so they work even when the model provider is down. Each replies with one line, e.g. `Screen off until tomorrow 09:00.` The gateway reads them on every message, so no restart is needed.

```yaml
quick_commands:
  sleep:
    type: exec
    command: /home/bartosz/.local/bin/hermes-beaver display off
  awake:
    type: exec
    command: /home/bartosz/.local/bin/hermes-beaver display on
  vacation:
    type: exec
    command: /home/bartosz/.local/bin/hermes-beaver display vacation
```

Don't name any of them `wake`: `/wake` is a built-in command (the wake-word listener), and built-ins take precedence.

The dashboard also switches the monitor on its own daily schedule (`[display]` in its config); that needs nothing from Hermes.

## 9. Check

1. Run `~/.local/bin/hermes-beaver call queue_set '{"items":[]}'`. It should print `{"changed":…,"count":0}`.
2. After the next gate tick (≤ 6 minutes), the dashboard's queue matches the pending jobs.
3. When a job is dispatched, it moves from queued to working. After `--mark done` it appears under "Recently finished", and under "Needs you" as ready for review. Closing the issue removes it.
4. A Telegram request that needs tools shows up as a `TG` task and finishes.
5. `/sleep` in Telegram answers `Screen off until …` and turns the monitor off within 10 seconds. `/awake` turns it back on. `/vacation` answers `Vacation mode: …`; then `/awake` ends it.
