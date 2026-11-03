# hermes-beaver MCP server

hermes-beaver is the wall dashboard next to Hermes Agent. It shows what the agent is working on, what is queued, what needs the owner and what finished. The dashboard only knows what it is told: Hermes reports its work through the tools below. Three more tools switch the monitor off while the owner sleeps or is away, and back on.

There are two ways to call the same tools. Both write to the same store (`~/.local/state/hermes-beaver/events.db`), and the dashboard picks changes up within 10 seconds.

| Caller | How |
|---|---|
| The model (Telegram, CLI and cron agent turns) | The MCP server `beaver`. Tools appear as `mcp__beaver__<tool>`. |
| Scripts (Bober gate scripts, `--mark`) | `hermes-beaver call <tool> '<json>'` |

## Register the server

Add this to `~/.hermes/config.yaml`. The gateway picks it up within about 60 seconds; no restart is needed.

```yaml
mcp_servers:
  beaver:
    command: /home/bartosz/.local/bin/hermes-beaver
    args: [mcp]
```

**Never add `beaver` to a cron job's `enabled_toolsets`.** Naming a server there turns the list into an allowlist, and the job's preflight then blocks whenever the server is down. Servers that aren't named are merged into every job automatically and never block.

## Rules

- **Never put secrets** (tokens, keys, webhook URLs, passwords) in titles, notes or summaries. They are shown on a wall display. The dashboard masks obvious ones, but don't rely on that.
- **Reporting must never break real work.** If a call fails, ignore the failure and carry on. Scripts use a 5-second timeout.
- **Keep titles short:** one line saying what the task is, e.g. `Fix the flaky checkout test`. Titles are cut at 300 characters.
- **GitHub refs use the form `owner/repo#123`.**

## Tools

### `task_start`

Report that real work started. Returns the task id.

| Argument | Required | Meaning |
|---|---|---|
| `source` | yes | `github`, `telegram` or `direct` (CLI or web UI) |
| `title` | yes | Short description |
| `kind` | no | `new`, `update`, `go`, `review` or `chat` |
| `ref` | no | `owner/repo#N` for GitHub work |
| `url` | no | Issue or PR URL |
| `cron_job_id` | no | Id of the Hermes cron job running this task (scripts only) |

Result: `{"id": 12, "existing": false}`.
- If `ref` is given and an unfinished task with that ref already exists, that task is returned with `"existing": true`. Calling it twice is safe.
- Starting a ref also removes it from the queue.

### `task_update`

Change the status of a running task.

| Argument | Required | Meaning |
|---|---|---|
| `id` or `ref` | one of them | Which task. A `ref` means the newest unfinished task with that ref. |
| `status` | yes | `working` or `waiting_feedback` |
| `note` | no | Short note. For `waiting_feedback`, the question for the owner. |

`waiting_feedback` puts the task at the top of the dashboard's "Needs you" list, with the note shown as the question. Set the status back to `working` when you continue.

### `task_finish`

Report that a task ended.

| Argument | Required | Meaning |
|---|---|---|
| `id` or `ref` | one of them | Which task (as above) |
| `outcome` | yes | `done`, `failed`, or `skipped` (needs a decision from the owner) |
| `summary` | no | One line: the result, or why it failed or was skipped |
| `pr_url` | no | The pull request that was opened or updated |

- A `done` GitHub task with a `pr_url` stays in "Needs you" as **ready for review** until `task_closed` is called for its ref.
- `skipped` and `failed` GitHub tasks also stay there until a later task for the same ref succeeds, or the ref is closed.
- Every `done` counts toward the "tasks finished" counter.

### `queue_set`

Replace the list of queued (not yet started) work with a full snapshot. Send the whole queue every time; an empty list clears it.

```json
{"items": [{"ref": "owner/repo#142", "mode": "new", "title": "Rate-limit the search endpoint", "url": "https://github.com/owner/repo/issues/142"}]}
```

`title` is required for each item; `ref`, `mode` and `url` are optional. If the snapshot equals the current queue, nothing changes.

### `task_closed`

Report that a GitHub issue or PR was closed or merged. Pass `{"ref": "owner/repo#N"}`. Every task with that ref leaves the review and attention lists, and the ref leaves the queue.

### The monitor

The dashboard may have a daily schedule in its own config, for example off at 23:00 and on at 09:00. At those times the monitor switches whatever its state. Between them, the owner's last request holds:

- `display_off` lasts until the next scheduled switch on, or until its `until` time if one is given (then the schedule takes over again);
- `display_on` lasts until the next scheduled switch off;
- vacation mode ignores the schedule and lasts until `display_on`.

New tasks never wake the monitor. Requests take effect within 10 seconds. These calls aren't tasks: don't call `task_start` for them.

Every display tool returns the same result:

```json
{"display": "off", "until": "2026-11-04T09:00:00+01:00", "vacation": false, "changed": true}
```

| Field | Meaning |
|---|---|
| `display` | `on` or `off`: what the monitor does now |
| `until` | When that changes by itself, in the dashboard's local time; `null` if only a request will change it. Use it when you confirm to the owner. |
| `vacation` | Whether vacation mode is on |
| `changed` | Whether the monitor switches because of this call |

### `display_off`

The owner is going to sleep or leaving the room.

| Argument | Required | Meaning |
|---|---|---|
| `until` | no | When to turn the monitor back on, in RFC 3339, e.g. `2026-11-04T10:00:00+01:00`. It overrides a scheduled switch on before it. A time without an offset is the dashboard's local time; a time in the past is rejected. |

In vacation mode it changes nothing (`vacation` stays `true`).

### `display_on`

The owner is up or back. No arguments. Also ends vacation mode.

### `display_vacation`

The owner is away for days. No arguments. The monitor stays off, ignoring the schedule, until `display_on`.

## `call` for scripts

```sh
hermes-beaver call task_start '{"source":"github","kind":"new","title":"Fix login","ref":"owner/repo#7"}'
# → {"existing":false,"id":3}
hermes-beaver call display_on        # the JSON argument defaults to {}
```

- On success it prints the result JSON on stdout.
- **Exit status:** `0` on success; `2` for invalid arguments, an unknown tool, or a request that doesn't fit the current state (e.g. finishing a task that is already finished); `1` for internal errors. The reason is printed on stderr.

A Python helper for scripts:

```python
import json, os, subprocess

BEAVER = os.path.expanduser("~/.local/bin/hermes-beaver")

def beaver(tool, **args):
    """Report to the dashboard. Never raises: reporting must not break real work."""
    try:
        subprocess.run([BEAVER, "call", tool, json.dumps(args)],
                       capture_output=True, timeout=5, check=False)
    except Exception:
        pass
```

## Checking it works

```sh
hermes-beaver call queue_set '{"items":[]}'      # → {"changed":…,"count":0}
```

In a conversation, `mcp__beaver__task_start` should be among your tools. If it isn't, check the `mcp_servers` entry and wait a minute.
