# hermes-beaver

A lightweight, full-screen dashboard that runs next to [Hermes Agent](https://github.com/NousResearch/hermes-agent) and shows what it is doing:

- current tasks and the queue (GitHub, Telegram, direct);
- what needs your feedback, and recently finished work;
- CPU, memory, disk and GPU load, with 30-minute sparklines;
- an ASCII animal, a beaver or a ground squirrel, whose mood follows the agent (working, napping, needs you, broken);
- a red banner when Hermes itself is unhealthy;
- a clock with ticking seconds, so you can tell at a glance that the screen isn't frozen.

It dims the monitor backlight over DDC/CI (`ddcutil`, which talks to the monitor over the cable, whatever the display server) when nothing is happening, and brightens it for a minute when something changes. The monitor goes into standby on a daily schedule (e.g. 23:00 to 09:00) and whenever you tell Hermes you're going to sleep or out (`/sleep`, `/awake`). Vacation mode (`/vacation`) keeps it off until you turn it back on. A key press or mouse move on the dashboard machine also wakes it, until ten minutes pass without input.

Written in Rust with [Slint](https://slint.dev): one binary of about 17 MB that idles at about 0% CPU and about 40 MB RSS (16 MB of it are the two frame buffers it shares with the compositor). It's a Wayland client. On the host it runs in a kiosk session: [sway](https://swaywm.org) with no bar, no menus and no Xwayland, rendering on the GPU. Hermes reports its work over MCP (see [MCP.md](MCP.md)). [PLAN.md](PLAN.md) has the design; the [project board](https://github.com/orgs/ideaconnect/projects/7) tracks progress.

## Commands

```sh
hermes-beaver                  # the dashboard (full-screen)
hermes-beaver demo             # made-up data, cycling through scenarios
hermes-beaver demo --scenario busy|idle|overflow|broken|empty
hermes-beaver mcp              # MCP server on stdio (Hermes starts this)
hermes-beaver call <tool> ['<json>']  # one tool call, for scripts (JSON defaults to {})
hermes-beaver display off|on|vacation   # switch the monitor, print one line
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
$EDITOR secrets/deploy.env secrets/config.toml secrets/sway.conf
make deploy
```

`make deploy` builds a release locally, copies it to the host, and runs [`deploy/install.sh`](deploy/install.sh) there as root. The script:
- installs `sway`, `ddcutil` (brightness) and `wlopm` (standby), without recommended packages, so no Xwayland;
- installs the binary, its systemd user unit, and the sway config [`deploy/sway.conf`](deploy/sway.conf) plus the host's `secrets/sway.conf` (outputs and modes);
- replaces the display manager with the kiosk session: [`hermes-beaver-kiosk.service`](deploy/hermes-beaver-kiosk.service) runs sway on tty7 as the desktop user. LightDM is disabled and masked, so no X server starts. Sway starts the dashboard;
- restarts sway only when its config changed, otherwise just the dashboard.

Run it again at any time; it's idempotent. To go back to X: `systemctl disable --now hermes-beaver-kiosk && systemctl unmask lightdm && systemctl enable --now lightdm`, after reinstalling LightDM and a desktop if you removed them, and deploy a build with Slint's `backend-winit-x11`. When a release adds MCP tools, send `/reload-mcp` to Hermes afterwards so the model sees them.

To connect Hermes, give [BOBER.md](BOBER.md) to the agent: it lists the changes to make on the Hermes side.

## Configuration

Every key is optional:

```toml
timezone = "Europe/Warsaw"   # clock, schedule and `until` times; default: the host's zone

[hermes]
home = "~/.hermes"
agent_jobs = []              # cron job ids whose failures raise the health banner
heartbeat_max_age_secs = 180

[tasks]
stale_after_mins = 30        # Telegram/CLI tasks without updates show "stale?"
finished_rows = 8

[brightness]
method = "auto"              # auto: DDC/CI, else a dark overlay | ddc: DDC/CI only | dim | off
idle = 20
active = 80
active_secs = 60
exit_level = 80

[metrics]
disk = "auto"                # block device behind /

[display]
off_at = "23:00"             # daily standby; both default to none (no schedule)
on_at = "09:00"              # leave it out to stay off until turned on

[ui]
renderer = "software"        # software (~40 MB RSS) | opengl (~110 MB, draws on the GPU); SLINT_BACKEND overrides
animal = "beaver"            # the ASCII animal: beaver | squirrel
animal_name = "Beaver"       # its panel's title (shown in capitals); default: the animal's name
```
