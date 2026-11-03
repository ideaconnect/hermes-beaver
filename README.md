# hermes-beaver

A lightweight, full-screen dashboard that runs next to [Hermes Agent](https://github.com/NousResearch/hermes-agent) and shows what it is doing:

- current tasks and the queue (GitHub, Telegram, direct);
- what needs your feedback, and recently finished work;
- CPU, memory, disk and GPU load, with 30-minute sparklines;
- an ASCII beaver whose mood follows the agent (working, napping, needs you, broken);
- a red banner when Hermes itself is unhealthy.

It dims the monitor backlight over DDC/CI when nothing is happening, and brightens it for a minute when something changes.

Written in Rust with [Slint](https://slint.dev): one binary of about 13 MB that idles at about 0% CPU and about 30 MB RSS. Hermes reports its work over MCP (see [MCP.md](MCP.md)). [PLAN.md](PLAN.md) has the design; the [project board](https://github.com/orgs/ideaconnect/projects/7) tracks progress.

## Commands

```sh
hermes-beaver                  # the dashboard (full-screen)
hermes-beaver demo             # made-up data, cycling through scenarios
hermes-beaver demo --scenario busy|idle|overflow|broken|empty
hermes-beaver mcp              # MCP server on stdio (Hermes starts this)
hermes-beaver call <tool> '<json>'   # one tool call, for scripts
```

Global options: `--config <path>` (default `~/.config/hermes-beaver/config.toml`) and `--store <path>` (default `~/.local/state/hermes-beaver/events.db`).

## Development

```sh
make test      # unit and integration tests
make lint      # rustfmt + clippy
make demo      # run the demo locally (works under WSLg)
```

## Deploying

Deployment targets and per-host config live in `secrets/`, which is gitignored:

```sh
cp -r secrets.example secrets
$EDITOR secrets/deploy.env secrets/config.toml
make deploy
```

`make deploy` builds a release locally, copies it to the host, and runs [`deploy/install.sh`](deploy/install.sh) there as root. The script:
- installs the binary, a systemd user unit and an XFCE autostart entry;
- keeps the screen from blanking;
- sets up `ddcutil` for brightness.

Run it again at any time; it's idempotent.

To connect Hermes, give [BOBER.md](BOBER.md) to the agent: it lists the changes to make on the Hermes side.

## Configuration

Every key is optional:

```toml
[hermes]
home = "~/.hermes"
agent_jobs = []              # cron job ids whose failures raise the health banner
heartbeat_max_age_secs = 180

[tasks]
stale_after_mins = 30        # Telegram/CLI tasks without updates show "stale?"
finished_rows = 8

[brightness]
method = "auto"              # auto (DDC/CI, else gamma) | ddc | gamma | off
idle = 20
active = 80
active_secs = 60
exit_level = 80

[metrics]
disk = "auto"                # block device behind /

[ui]
renderer = "software"        # software (~24 MB) | opengl (~95 MB); SLINT_BACKEND overrides
```
