# hermes-beaver: plan

A lightweight, full-screen dashboard that runs next to Hermes Agent and shows what it is doing.
It shows the current tasks and queue, what needs the owner's feedback, recently finished work,
system load, and an ASCII animal (`ui.animal`: beaver or ground squirrel) whose mood follows the agent's state.

v1 target: **bober-1**. Its address and other host-specific values live only in the gitignored
`secrets/` folder (template: `secrets.example/`). The app always runs
on the same machine as Hermes, and nothing in it is specific to this host, because the hardware may
be replaced.

---

## 1. Environment facts (checked 2026-11-03)

**bober-1**
- Ubuntu 26.04.1, kernel 7.0, glibc 2.43 (identical to the WSL build machine). 2 cores, 7.5 GB RAM, SSD `sda`.
- GPU: Intel JasperLake UHD, `i915` driver. `perf_event_paranoid = 4`.
- Monitor: Dell G3223Q on HDMI-1 (supports 4K; X runs it at 1920×1080@60). DDC/CI is supported, `/dev/i2c-*` is present, and `ddcutil` is not installed.
- Desktop: LightDM autologin → XFCE session for `bartosz` on `:0`. There is no `~/.config/autostart` yet.
- Installed already: `libegl1`, `libfontconfig1`, `libxkbcommon-x11-0`, DejaVu Sans Mono.

**Hermes Agent** (v0.22.5, `~/.hermes/hermes-agent`, runs as `bartosz`)
- One gateway process runs **both** Telegram turns and cron agent turns. One MCP server is therefore shared by all of them.
- MCP servers are configured under `mcp_servers:` in `~/.hermes/config.yaml`.
  - stdio is supported, and the gateway reconciles new servers within about 60 s, so no restart is needed.
  - The model sees tools as `mcp__<server>__<tool>`.
  - A failing tool returns an error to the model and never fails the turn.
  - Tool calls carry **no** session or job identity.
- If a server is **named** in a cron job's `enabled_toolsets`, that list becomes an allowlist, and the job's preflight blocks while the server is down. Servers that are not named are merged in automatically and never block. **So the server is never named in those lists.**
- `~/.hermes/SOUL.md` is loaded for Telegram and cron. Skill `SKILL.md` files are re-read on every cron run. An open Telegram session keeps its old system prompt until `/new`.
- Work sources (the cron job ids are in `secrets/host-notes.md`):
  - Bober slot 1 and slot 2: two cron jobs, `bober-issue-to-pr` and `bober-issue-to-pr-2`;
  - `github-pr-auto-review`: a cron job;
  - the Telegram DM.

- Bober's queue is computed in memory by its gate script on each 5-minute tick, and is not stored anywhere.
- Health sources. All are mode 0600 and owned by `bartosz`, so the dashboard must run as `bartosz`:
  - `~/.hermes/state/gateway.heartbeat` and `~/.hermes/gateway_state.json`
  - `~/.hermes/cron/executions.db`, table `executions(id, job_id, status, claimed_at, started_at, finished_at, error, …)`
  - `~/.hermes/cron/jobs.json` (`last_status`, `last_error`, …)
  - `~/.hermes/bober/state.json` (`consecutive_failures`)
- `~/.hermes/scripts` and `~/.hermes/skills` are **not** under version control.

---

## 2. Decisions

| Area | Decision |
|---|---|
| Stack | One Rust binary, `hermes-beaver`, with a Slint UI. OpenGL (`femtovg`) renderer by default; `SLINT_BACKEND=winit-software` is the fallback. |
| Display | Full screen at native 1920×1080 with large text, readable from a distance. Display-only: `Esc`/`q` quits, `F11` toggles full screen, the mouse cursor is hidden. |
| Screens | An app shell plus **one** pluggable screen in v1. How screens switch is decided when a second screen exists. |
| Layout | 2×2 grid with the left column at 60%. A header shows the hostname, the clock and a slot for the health banner. Lists that overflow end with "+N more". |
| Top-left | Running tasks, then the queue. Each shows source (GitHub, Telegram or Direct), title, ref `owner/repo#N`, and status with elapsed time. |
| Bottom-left | **Needs you** first, in this order: `waiting_feedback` → skipped → failed (including stalled) → PRs ready for review, oldest first. Below that, about 8 **recently finished** tasks with outcome and relative time. |
| Top-right | CPU, RAM, disk (busy % plus read/write MB/s) and GPU busy %. Each is a bar plus a 30-minute sparkline of 10 s window averages. |
| GPU | Vendor-neutral, read from DRM fdinfo (`/proc/<pid>/fdinfo`). No capabilities needed. The app's own PID is excluded. Root-owned clients such as Xorg are not visible (accepted). NVML is added only if NVIDIA hardware appears. |
| Bottom-right | ASCII animal with 4 states (**working / idle / needs-you / broken**) and "**N** tasks finished since \<go-live date\>". **No slot display.** |
| Task data | **Pushed**, not scraped. Two entry points share one code path that writes a SQLite store: MCP over stdio (`hermes-beaver mcp`, spawned by Hermes) for the model, and `hermes-beaver call <tool> '<json>'` for scripts. The GUI only reads the store, except for reconciliation, which writes through the same tool code path. |
| Who reports | The **Bober lifecycle is reported by code**: the gate tick sends the queue and task starts, `--mark` sends finishes, and closure detection sends closes. In Bober runs the model only reports `waiting_feedback`. For Telegram and Direct, the model reports start, update and finish, as instructed in SOUL.md. |
| What a task is | A GitHub task is one Bober job (`new`, `update` or `go`) or one pr-review run. A Telegram or Direct task is one request that needs real work (tool use). Gate-only ticks, `humor` and `bober-queue-notes` are excluded. |
| Health | Read **directly** (read-only) from Hermes files, because a broken agent cannot report its own failure. It drives a red banner and the *broken* beaver. |
| Reconciliation | A GitHub task still `working` after its cron execution ended is closed from that execution's status. A Telegram or Direct task with no update for 30 minutes shows as `stale?`. No Hermes plugin is used. |
| Counter | Counts tasks finished with outcome `done`, starting at go-live. It only increases. **No backfill.** |
| Masking | Every displayed title or note has URLs cut to the host (`hooks.example.com/…`) and long token-like strings replaced with `••••`. |
| Refresh | One 10 s tick. The clock ticks every second, seconds dimmed, so a frozen screen shows at once. Hermes files are re-read only when their mtime changes. |
| Brightness | Monitor backlight via **DDC/CI** (`ddcutil`): 20% when idle, 80% for 60 s on an event. Falls back to dimming the picture when DDC is unavailable: RandR gamma under X, a dark overlay since the move to Wayland (§14). |
| Wake events | A task queued, started or finished, a new feedback item, or a health state change. Metric updates never wake the screen. |
| Hermes changes | This repo **never writes into `~/.hermes`**. It ships `MCP.md` and `BOBER.md`; Bober applies the changes to its own scripts, skills, SOUL.md and config. |
| Deploy | Build on WSL. `make deploy` reads `secrets/deploy.env` (the whole `secrets/` folder is gitignored), copies files to the host, and runs an idempotent `install.sh`. |

---

## 3. Architecture

```
 Hermes gateway (Telegram + cron agents)
   │  stdio MCP: mcp__beaver__*
   ▼
 hermes-beaver mcp ───────┐
                          ├──► ~/.local/state/hermes-beaver/events.db  (SQLite, WAL)
 Bober gate scripts       │                       │
   └► hermes-beaver call ─┘                       │ read-only
                                                  ▼
 Hermes files (read-only) ─────────────►  hermes-beaver (GUI)  ◄──── /proc, /sys, fdinfo
   heartbeat, executions.db,                      │
   jobs.json, bober/state.json                    └──► ddcutil setvcp 10 (or a dim overlay)
```

**Subcommands**
- *(none)*: run the GUI.
- `mcp`: MCP stdio server.
- `call <tool> <json>`: run one tool. Exit 0 on success; non-zero plus a stderr message on failure.
- `demo`: GUI against a throwaway store filled with fake tasks, health states and synthetic load. Used for development under WSLg.

**Crate layout**
```
src/main.rs              CLI dispatch (clap)
src/config.rs            ~/.config/hermes-beaver/config.toml (+ defaults)
src/store/               schema + migrations, writer (used by tools), read queries for views
src/tools/               tool schemas + handlers (shared by mcp and call)
src/mcp.rs               rmcp stdio server exposing src/tools
src/metrics/             cpu.rs, mem.rs, disk.rs, gpu.rs (fdinfo), history.rs (ring buffer, 180 samples)
src/hermes/              read-only Hermes readers: heartbeat, executions, jobs, bober state
src/health.rs            health rules → HealthState
src/reconcile.rs         closes orphaned GitHub tasks from the execution ledger
src/brightness/          ddc.rs, dim.rs, controller.rs (wake timer, fade steps)
src/mask.rs              URL/token masking
src/ui/                  app shell, screens/main.slint, components (bar, sparkline, task row), art/<animal>/*.txt
src/demo.rs              fake data generator
deploy/                  install.sh, hermes-beaver.service, sway.conf, hermes-beaver-kiosk.{service,pam}
MCP.md, BOBER.md         hand-off docs for Bober
Makefile
secrets.example/         committed template for the gitignored secrets/ (deploy.env, config.toml)
```

**Key crates:** `slint` (features `renderer-femtovg` and `renderer-software`, backend `winit`), `rmcp` (stdio server), `rusqlite` (`bundled`), `serde`/`serde_json`, `clap`, `toml`, `time`.

**Config** (`~/.config/hermes-beaver/config.toml`; every key is optional):
```toml
[hermes]
home = "~/.hermes"
agent_jobs = []        # host-specific cron job ids; set in secrets/config.toml
heartbeat_max_age_secs = 180

[tasks]
stale_after_mins = 30
finished_rows = 8

[brightness]
method = "auto"        # auto | ddc | gamma | off
idle = 20
active = 80
active_secs = 60
exit_level = 80        # restored when the app exits

[metrics]
disk = "auto"          # block device behind /
```

---

## 4. Store and tools

**Tables**
- `tasks`: `id` (ULID), `source` (`github|telegram|direct`), `kind` (`new|update|go|review|chat`), `title`, `ref`, `url`, `pr_url`, `cron_job_id`, `status` (`queued|working|waiting_feedback|done|failed|skipped`), `note`, `summary`, `created_at`, `started_at`, `updated_at`, `finished_at`, `closed_at`, `reconciled` (bool).
- `queue_items`: `position`, `ref`, `mode`, `title`, `url`, `updated_at`. `queue_set` replaces the whole table atomically.
- `events`: `id`, `ts`, `kind`, `task_id`, `payload`. Append-only. Feeds wake events and the counter.
- `meta`: `go_live_at` (written on first open), `schema_version`.

**Tools** (identical over MCP and `call`; each tool's description says when to call it)

| Tool | Arguments | Effect |
|---|---|---|
| `task_start` | `source`, `title`, `kind?`, `ref?`, `url?`, `cron_job_id?` | Creates a `working` task and returns `id`. For GitHub, if an unfinished task with the same `ref` already exists, it returns that task (idempotent). |
| `task_update` | `id` **or** `ref`, `status` (`working`\|`waiting_feedback`), `note?` | `waiting_feedback` puts the task in "Needs you", with `note` as the question. |
| `task_finish` | `id` **or** `ref`, `outcome` (`done`\|`failed`\|`skipped`), `summary?`, `pr_url?` | Finishes the task. `done` increments the counter. |
| `queue_set` | `items: [{ref, mode, title, url}]` | Full snapshot. An event is recorded only if the snapshot differs from the previous one. |
| `task_closed` | `ref` | Sets `closed_at` on that ref's tasks, which removes them from "ready for review" and the failed or skipped groups. |

A `ref` resolves to the most recent unfinished task with that ref. Writers use WAL with `busy_timeout = 5s`.

**Derived views**
- **Needs you**, in this order:
  1. tasks in `waiting_feedback`;
  2. the latest `skipped` per ref;
  3. the latest `failed` per ref, where not closed and not followed by a later `done`;
  4. the latest `done` per ref that has a `pr_url` and is not closed (PR ready for review), oldest first.
- **Finished**: the latest `done|failed|skipped` tasks.
- **Stale**: `working` and `updated_at` older than `stale_after_mins`. This is shown in the UI and not stored.
- **Counter**: `count(done)` and `go_live_at`.

**Reconciliation**
- Applies to a GitHub task that is `working`, has `cron_job_id`, and is older than a 2-minute grace period.
- Find the execution of that job with `started_at ≤ task.started_at ≤ finished_at`.
- If that execution has finished, finish the task with `completed→done` or `failed→failed`, set `reconciled = true`, and use the summary "closed from cron ledger".

**Health rules** (any match turns on the red banner and the broken animal):
1. The heartbeat is older than `heartbeat_max_age_secs`: "Hermes gateway not responding".
2. The last 2 executions of `agent_jobs` both have status `failed`: show the first line of `error`, masked.
3. Bober `consecutive_failures > 0`.

---

## 5. Metrics

- **CPU:** from `/proc/stat` (aggregate line): `1 − Δ(idle+iowait)/Δtotal` over the window.
- **RAM:** from `/proc/meminfo`: `1 − MemAvailable/MemTotal`.
- **Disk:** resolve the device behind `/` (`sda` today). In `/proc/diskstats`, busy % = `Δio_ticks / Δms`, and read/write MB/s = `Δsectors × 512 / Δt`.
- **GPU (fdinfo):**
  1. List `/proc/*/fd` entries that link to `/dev/dri/*`, then read only those fdinfo files.
  2. Deduplicate clients by `drm-client-id` and skip our own PID.
  3. Handle both formats:
     - nanosecond counters `drm-engine-<class>: N ns` (i915, amdgpu, most ARM): busy = Σ ΔN / (Δt × `drm-engine-capacity-<class>`, default 1);
     - cycle counters `drm-cycles-<class>` / `drm-total-cycles-<class>` (Intel `xe`): busy = Δcycles / Δtotal-cycles.
  4. The gauge shows the busiest engine class. If no DRM device exists, the gauge is hidden.
- **History:** an in-memory ring buffer of 180 samples per metric. It is lost on restart, which is acceptable.

---

## 6. Brightness

- **Startup detection:**
  - `method = auto`: run `ddcutil detect --brief`, cache the bus number, then read VCP `0x10`. On success use DDC; otherwise use dim.
  - The Dell's on-screen menu must have DDC/CI enabled. The detection message tells you if it isn't.
- **DDC:** `ddcutil --bus N --noverify setvcp 10 <level>`. Fade 20→80 in 3 steps over about 1 s. Each event causes about 2 writes, which protects the monitor's EEPROM.
- **Dim:** a black overlay drawn by the dashboard itself, with opacity `1 − level/100`. It needs nothing from the compositor. (Under X this was `xrandr --brightness`, and `gamma` still parses as its name.)
- **Controller:**
  - A new wake event sets the level to active and restarts the timer.
  - After `active_secs` with no new events, fade back to idle.
  - On exit (SIGTERM included), set `exit_level`.

---

## 7. Deployment

- **`secrets/`** is gitignored as a whole. `secrets.example/` is the committed template.
  - `secrets/deploy.env`:
    ```
    DEPLOY_HOST=root@<host address>
    DEPLOY_USER=<desktop user>
    ```
  - `secrets/config.toml`: the host's dashboard config, such as `agent_jobs`.
  - `secrets/host-notes.md`: private notes about the host.
- **`make deploy`:**
  1. `cargo build --release` on WSL. The OS and glibc match the target, so no cross toolchain is needed.
  2. `scp` the binary and `deploy/` to a temporary directory on the host.
  3. `ssh` and run `install.sh` as root.
- **`install.sh`** (idempotent; prints each action):
  1. `apt-get install --no-install-recommends` the missing ones of `ddcutil`, `sway` and `wlopm`. Without recommends, no Xwayland. Add `DEPLOY_USER` to the `i2c` group and reload the udev rules.
  2. Install the binary to `~DEPLOY_USER/.local/bin/hermes-beaver` with an atomic rename.
  3. Install `secrets/config.toml` as `~/.config/hermes-beaver/config.toml` if it exists. Otherwise write a default config, but only if none exists.
  4. Install `~/.config/systemd/user/hermes-beaver.service` (`Restart=always`, `RestartSec=5`), `deploy/sway.conf` as `~/.config/hermes-beaver/sway.conf`, and `secrets/sway.conf` as `~/.config/hermes-beaver/sway.d/host.conf`. Check the sway config with `sway --validate` on the headless backend, and stop if it fails.
  5. Install the kiosk session: `/etc/pam.d/hermes-beaver-kiosk` and `/etc/systemd/system/hermes-beaver-kiosk.service`, which is aliased as `display-manager.service`. Disable, stop and mask the display manager it replaces (LightDM), then enable the kiosk.
  6. Unset what the X session exported into the user manager (`DISPLAY`, `XAUTHORITY`, the XFCE `XDG_*` paths).
  7. Start the kiosk if it isn't running. Restart it if its files changed, which makes sway restart the dashboard. Otherwise restart just the dashboard.
  8. If a `hermes-beaver mcp` process is running, `pkill` it so Hermes respawns the new binary. **Schema migrations must therefore be backward compatible.**

---

## 8. Hand-off documents (for Bober)

- **`MCP.md`**: what Bober needs to use the server.
  - The `config.yaml` snippet:
    ```yaml
    mcp_servers:
      beaver:
        command: /home/bartosz/.local/bin/hermes-beaver
        args: [mcp]
    ```
  - A warning **never** to add it to a job's `enabled_toolsets`.
  - Tool reference with schemas and examples.
  - `call` CLI usage for scripts.
  - `id`/`ref` semantics.
  - The rule "never put secrets in titles or notes".
- **`BOBER.md`**: the exact changes Bober makes to itself.
  - **Gate scripts** (both slots):
    - `queue_set` with the full computed queue on every tick;
    - `task_start` on dispatch, with `ref`, `mode`, `title`, `url` and its own `cron_job_id`;
    - `task_closed` when an issue or PR is seen closed or merged.
  - **`--mark` handler:**
    - `done` and `updated` → `task_finish(done, pr_url)`;
    - `skipped` → `task_finish(skipped)`;
    - `failed` → `task_finish(failed)`;
    - a stalled update (2 or more attempts) → `task_finish(failed, "stalled")`.
  - **pr-review script:** the same lifecycle with `kind = review`.
  - **Skills:** in `bober-issue-to-pr` and `github-auto-pr-review`, call `task_update(ref, waiting_feedback, note)` when blocked on the owner.
  - **`SOUL.md`:** a "Dashboard reporting" section for Telegram and CLI. It covers when to call `task_start`, `task_update` and `task_finish`, and says not to call them from Bober cron runs (the scripts do that).
  - **Robustness:** every script call uses a 5 s timeout, and its failure is ignored. The dashboard must never break Bober.
  - **Reminder:** run `/new` in Telegram afterwards.

---

## 9. Phases

Each phase ends with something visible on the real screen.

**Phase 0: Spikes (de-risk)**
- A Slint full-screen window on bober-1 `:0` with femtovg and with software rendering. Record idle CPU and RSS for each.
- An fdinfo GPU reading under load (`glxgears` or a Godot demo), cross-checked against `intel_gpu_top` (install `intel-gpu-tools` temporarily).
- `ddcutil detect` and `setvcp 10` on the Dell. Measure how long one call takes.
- Confirm the Bober gate script runs inside the cron execution window (reconciliation depends on this), and measure the heartbeat's write cadence.

**Phase 1: Skeleton, metrics and deploy**
- Crate, CLI, config, metrics collectors with unit tests against fixture `/proc` files.
- Slint shell with header and 2×2 layout. The metrics block is real; the other blocks are placeholders.
- `demo` subcommand.
- `secrets.example/`, a `Makefile`, and a minimal `install.sh` (binary, unit, autostart).
- *Done when:* it runs under WSLg in demo mode, and `make deploy` brings it up full screen on bober-1, surviving a reboot.

**Phase 2: Store, tools, MCP and `call`**
- Schema and migrations, tool handlers with unit tests, `call` subcommand, `rmcp` stdio server.
- An MCP smoke test that runs `initialize` → `tools/list` → `tools/call` over stdio.
- *Done when:* `call` and MCP produce identical store rows, and the smoke test passes in CI (`cargo test`).

**Phase 3: Task panels and beaver**
- Top-left and bottom-left views from the store, the masking module (with tests), the stale rule, the counter and `go_live_at`.
- Four ASCII frames per animal, chosen by state. The beaver is original; the squirrel's working frame is the art from idct.tech, and its other moods change the eyes and the bulb.
- *Done when:* demo mode shows every list state, including overflow ("+N more"), empty lists, and each beaver state.

**Phase 4: Health and reconciliation**
- Hermes readers (read-only SQLite via `mode=ro` URI), health rules, banner, reconciliation, with fixture-based tests.
- *Done when:* on bober-1, today's credential failure (if still present) shows the banner and the broken beaver.

**Phase 5: Brightness**
- DDC and gamma backends, controller and wake events. `install.sh` gains `ddcutil` and the `i2c` group.
- *Done when:* the screen sits at 20%, a `call task_start …` raises it to 80% for 60 s, and with `method = gamma` the behaviour matches.

**Phase 6: Hand-off and go-live**
- Write `MCP.md` and `BOBER.md`. You give them to Bober.
- End-to-end check:
  - a Telegram request appears as a task and finishes;
  - the Bober queue appears within one tick;
  - a dispatched issue moves from queue → working → done → "ready for review";
  - closing the issue removes it;
  - the counter increases.
- *Done when:* the end-to-end check passes and the store records `go_live_at`.

---

## 10. Testing

- **Unit tests:** metric parsers (fixture files for `/proc/stat`, `meminfo`, `diskstats`, and i915/amdgpu/xe fdinfo samples), masking, tool handlers, derived views, health rules, reconciliation. They use an in-memory or temp SQLite and fixture Hermes files.
- **Integration test:** the MCP stdio smoke test.
- **UI:** checked manually in `demo` mode, under WSLg and on the target screen.
- **Footprint check before go-live:** idle CPU < 1% of one core, RSS within a few tens of MB.

---

## 11. Risks and limits

- **Hermes internals can change on `hermes update`:** the MCP config, `executions.db` schema and heartbeat location. The readers fail soft (the health banner says "cannot read X") instead of crashing.
- **The model may forget to report Telegram tasks.** The `stale?` rule makes this visible. Bober's lifecycle does not depend on the model.
- **fdinfo does not see root-owned GPU clients** (Xorg).
- **DDC/CI can be switched off in the monitor's on-screen menu.** The app then falls back to gamma.
- **The two writers (MCP child and `call`) share SQLite.** WAL with busy timeouts covers this. Tool calls stay small.

## 12. Out of scope for v1

Screen switching and additional screens; NVIDIA/NVML and other vendor-specific GPU code; history backfill; slot display;
collecting data from other machines; any interaction beyond quit and full-screen toggle.

---

## 13. Changes made during implementation

Spike results and implementation details that changed the plan above:

- **Renderer: software by default.** Spike #1 measured, full-screen at 1080p on the target, 2 minutes each:

  | Renderer | CPU | RSS |
  |---|---|---|
  | OpenGL | 0.025% | 95 MB |
  | Software | 0.025% | 23.5 MB |

  Brightness moved to DDC/CI, so no in-app animation needs the GPU. `[ui] renderer = "opengl"` or `SLINT_BACKEND` switches back.
- **GPU gauge: whole-GPU sysfs counters first, fdinfo as the fallback.** Spike #2 showed that fdinfo read as the desktop user misses the root-owned Xorg, which did 55–75% of the render work under load. The gauge would have read 22–55 points low.
  - The app now reads, without privileges: i915 `rc6_residency_ms`, xe `idle_residency_ms`, or amdgpu `gpu_busy_percent`.
  - fdinfo is used only when none of those exists.
  - Still no vendor-specific code paths beyond probing these files.
- **MCP server is hand-written** (line-delimited JSON-RPC: `initialize`, `ping`, `tools/list`, `tools/call`) instead of `rmcp`. This avoids a tokio runtime. Tested against Hermes's own Python MCP client (SDK 2.3.0, protocol 2025-11-25).
- **Task ids are integers**, not ULIDs.
- **DDC writes use `--skip-ddc-checks`** after a startup probe. Spike #3 measured about 60 ms per write instead of about 410 ms, with every read-back correct.
- **Reconciliation matches by `cron_job_id`.** Spike #4 confirmed every gate dispatch falls inside its own job's execution window. The two slots' windows overlap, so the job id is required. The ledger keeps about 19 hours.
- **"Needs you" groups 2–4 are GitHub-only.** Only refs have a close lifecycle. A failed Telegram task shows under "Recently finished".
- **The animal's needs-you sign ignores PRs waiting for review.** There are usually some, and the sign would never come down. The caption still counts them.

## 14. Added after v1

- **Monitor standby.** There's no point lighting an empty room.
  - **Tools:**
    - `display_off`, with an optional `until`;
    - `display_on`;
    - `display_vacation`.

    Like the other tools, they only write to the store: the request with its time, in `meta` keys `display`, `display_since` and `display_until`, plus an event.
  - **Daily schedule:** `[display] off_at` / `on_at` in the config.
  - **Who wins:** the latest of the owner's request and the last scheduled switch. A request holds until the next switch.
    - An `off` with `until` holds until that time, even across a switch on, then hands back to the schedule.
    - Vacation ignores the schedule until `display_on`.
  - **Replies:** every result says when the state next changes by itself, so Hermes can confirm "off until 09:00".
  - **`timezone`** in the config sets `TZ` at startup. The target runs on UTC while the owner is in Europe/Warsaw; this fixes the clock too, without touching the host's zone, which Hermes also uses.
  - **Switching** happens on the GUI's tick, through the compositor (originally X DPMS; see *Wayland kiosk* below):
    - `wlopm --off '*'` puts the monitor into standby, and `wlopm --on '*'` wakes it.
    - Checked on the target: the HDMI connector's DPMS goes Off and On, and DDC/CI still answers in standby.
    - A key press or pointer move wakes it without changing the request. After 600 s without input it goes back into standby; that was the X DPMS default timeout. The key that wakes the monitor does nothing else, so an Esc doesn't also quit.
  - **Brightness:** in standby, events don't trigger the brightness boost. Waking gives the usual active minute.
  - **Restarts:** a restart re-applies standby. Stopping the dashboard wakes the monitor.
  - **Hermes:**
    - quick commands `/sleep`, `/awake` and `/vacation` (zero tokens; `/wake` is a Hermes built-in) run `hermes-beaver display …`, which prints one line;
    - a SOUL.md rule covers natural-language requests.
- **Ticking seconds.** The clock shows dimmed seconds, so a glance tells whether the dashboard is alive (the 10 s refresh runs on the same thread). The seconds are scheduled just after each wall-clock second, so they never skip.
- **Patched Slint core.** In Slint 1.18.x the software renderer stopped repainting a Text that changes rarely after about 1024 updates to other text ([slint#14435](https://github.com/slint-ui/slint/issues/14435)): the clock froze while the rest of the screen kept updating. `vendor/i-slint-core` is 1.18.1 with the upstream fix ([slint#14543](https://github.com/slint-ui/slint/pull/14543)), wired in through `[patch.crates-io]`; see `vendor/README.md`.
- **Wayland kiosk** (2026-11-07, on `susel`: Ubuntu 26.10, 12 cores, AMD Phoenix iGPU on `amdgpu`). LightDM, Xorg and XFCE are replaced by sway 1.12 running only the dashboard.
  - **Compositor: sway.** It's GPU accelerated: wlroots' GLES2 renderer on radeonsi. Its config can turn Xwayland off (`xwayland disable`) and set the output mode (1920×1080@60), and it implements `wlr-output-power-management` for standby. cage is smaller, but it can't do the first two from its config. The config ([`deploy/sway.conf`](deploy/sway.conf)) has no bar, no key bindings, no borders and no wallpaper (`swaybg_command -`). Ctrl+Alt+F*n* still switches to a text console.
  - **No X11 anywhere.** Xwayland isn't installed and is disabled in the config. The binary is built with Slint's `backend-winit-wayland` only. Standby uses `wlopm` and dimming draws an overlay, so the app never runs `xset` or `xrandr`. The only X11 code left in the binary is arboard's clipboard, which nothing calls.
  - **Session:** a system unit runs sway as the desktop user on tty7, with a logind session through PAM (`pam_systemd`). That session gives sway the GPU and the input devices, and libseat switches to the VT itself. `Restart=always`. No greeter or display manager is involved.
  - **Startup:** sway's `exec` imports `WAYLAND_DISPLAY` into the user manager and restarts `hermes-beaver.service`. Sway splits commands at `;` even after `exec`, so the steps are chained with `&&`. `sway --validate` doesn't see deferred commands.
  - **Fullscreen:** sway fullscreens the window with a rule on the app_id `hermes-beaver`. The app's own request loses a race: Slint follows sway's first configure, which comes before the window maps and isn't fullscreen yet, and withdraws the request.
  - **Measured on the target** (PSS): sway 95 MB, almost all of it the radeonsi driver; the dashboard 26 MB. Its RSS is about 38 MB, 16 MB of which are the two shared frame buffers that Xorg used to hold. The OpenGL renderer works on Wayland too, at about 108 MB RSS.
