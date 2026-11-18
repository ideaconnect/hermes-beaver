# hermes-beaver

A lightweight, full-screen dashboard that runs next to [Hermes Agent](https://github.com/NousResearch/hermes-agent) and shows what it is doing:

- current tasks and the queue (GitHub, Telegram, direct);
- what needs your feedback, and recently finished work;
- CPU, memory, disk and GPU load, with 30-minute sparklines;
- an ASCII animal, a beaver or a ground squirrel, whose mood follows the agent (working, napping, needs you, broken);
- a red banner when Hermes itself is unhealthy;
- a clock with ticking seconds, so you can tell at a glance that the screen isn't frozen;
- a read-only web page with the same dashboard, for the other computers and phones on your LAN;
- a desktop app ([Tauri](https://tauri.app), on the system's web view) that opens that page in a window of its own or full screen, with a token.

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
hermes-beaver web              # the dashboard as a read-only web page ([web] listen)
hermes-beaver web --demo [--scenario busy|…]   # the same on made-up data
```

Global options: `--config <path>` (default `~/.config/hermes-beaver/config.toml`) and `--store <path>` (default `~/.local/state/hermes-beaver/events.db`).

## Development

```sh
make test      # unit and integration tests
make lint      # rustfmt + clippy
make demo      # run the demo locally (works under WSLg)
make demo-web  # the web view on made-up data: http://127.0.0.1:8788/
make desktop   # the desktop app, a release build (see Desktop app)
make desktop-test  # its rustfmt, clippy and tests
```

## Deploying

Deployment targets and per-host config live in `secrets/`, which is gitignored:

```sh
cp -r secrets.example secrets
$EDITOR secrets/deploy.env secrets/config.toml secrets/sway.conf
make deploy
```

`make deploy` builds a release locally, copies it to the host, and runs [`deploy/install.sh`](deploy/install.sh) there as root. The script:
- installs `sway` and `ddcutil` (brightness), without recommended packages, so no Xwayland. Standby goes through sway's IPC socket;
- installs the binary, its systemd user unit, and the sway config [`deploy/sway.conf`](deploy/sway.conf) plus the host's `secrets/sway.conf` (outputs and modes);
- replaces the display manager with the kiosk session: [`hermes-beaver-kiosk.service`](deploy/hermes-beaver-kiosk.service) runs sway on tty7 as the desktop user. LightDM is disabled and masked, so no X server starts. Sway starts the dashboard;
- installs and enables the web view's user unit, [`hermes-beaver-web.service`](deploy/hermes-beaver-web.service), and restarts it. Lingering starts it at boot, without sway. When the config names a `[web] token_file` that doesn't exist yet, it generates one: 64 hex characters, readable by root and the desktop user only;
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

[web]
listen = "127.0.0.1:8788"    # `hermes-beaver web`; set the LAN address to open it from other computers
token_file = "/etc/secrets/hermes-beaver-web.token"   # require this token; default: none
```

## Web view

`hermes-beaver web` serves the dashboard as a page for other computers and phones on the LAN, at the address in `[web] listen`. It's read-only: no buttons and no TLS, so keep it on the LAN. It runs as its own process, so it keeps working while the kiosk or sway is down, and it never writes to the store (reconciliation stays with the kiosk).

With `[web] token_file` set, every request must carry the token in that file (surrounding whitespace aside; at least 16 printable characters): the [desktop app](#desktop-app) asks for it once and sends it as `Authorization: Bearer`, and a browser gets a sign-in prompt, where any user name and the token as the password let it in. The file is read when the web view starts: after changing it, `systemctl --user restart hermes-beaver-web`. If it can't be read, the web view doesn't start, rather than open up. Without `token_file` there's no login.

The page shows what the kiosk shows, plus:
- **Full lists** instead of "+N more" (finished tasks still stop at `finished_rows`). Wide windows get the kiosk's grid; narrow ones, a phone included, one column with "Needs you" first.
- The **monitor's state**: "Screen on until 23:00", "Screen in standby until 09:00", "Screen off: vacation". It's the requested and scheduled state: a key press on the dashboard machine can light the screen for a while without changing it.
- **Links** to GitHub on a task's issue or PR (only `https://github.com/` links; titles stay masked).
- The host's time, and how old the data is: amber after 30 s without an update, "*name* not responding" after 2 minutes. The browser tab's title counts what needs you, `(2) Beaver · hermes`, and gets a `⚠` while the health banner is up.

The page polls `/state.json` every 5 s. That JSON is the page's own feed, not an API: its shape changes with the page. Scripts use MCP or `call`. After a deploy, open pages reload themselves.

It answers only to an IP address or a LAN name (a single label such as `hermes`, or `*.local`, `*.lan`, `*.home`, `*.home.arpa`, `*.internal`). A request under any other host name, which is what DNS rebinding from a web page looks like, gets 403. To put it behind a reverse proxy under a public name later, have the proxy send the upstream's address as `Host`, and add a login there.

## Desktop app

[`desktop/`](desktop/) opens the web view in a window of its own: a [Tauri](https://tauri.app) app on the system's web view (WebKitGTK on Linux, WebView2 on Windows, WKWebView on macOS), so no browser ships with it. It's a crate and workspace of its own: the dashboard's build doesn't change.

- **First launch** asks for the dashboard's address (`[web] listen`, e.g. `susel:8788`) and token (the contents of `[web] token_file`; empty when the web view has none). It checks both against the dashboard before keeping them in the app's settings, `~/.config/tech.idct.hermes-beaver/settings.json` (mode 0600; `%APPDATA%\tech.idct.hermes-beaver\` on Windows). When the token stops working, it asks again; when the dashboard can't be reached, it says so and retries every 10 s.
- **Keys:** F11 toggles full screen, Esc leaves it, Ctrl+, opens the settings. It starts the way the window was last time; `--fullscreen` and `--windowed` override that, `--setup` asks for the address and token again.
- **Links** to GitHub open in your browser; the window never leaves the dashboard.
- **How:** the window loads `beaver://localhost/`, which the app answers itself: its own setup page, or the dashboard's page fetched with the token. So the page is the one a browser gets, and it never sees the token. The app defines no Tauri commands or capabilities.
- **Icon:** the web view's favicon, [`web/favicon.svg`](web/favicon.svg), in the sizes each system wants (`desktop/icons/`, made with `cargo tauri icon`).

Building needs Rust and, on Linux, the web view's headers (Debian/Ubuntu: `sudo apt install libwebkit2gtk-4.1-dev libgtk-3-dev libsoup-3.0-dev libjavascriptcoregtk-4.1-dev`). At run time it needs `libwebkit2gtk-4.1-0` on Linux and WebView2 on Windows (part of Windows 11; Tauri's Windows installers fetch it where it's missing); macOS has its web view already.

```sh
make desktop                       # desktop/target/release/hermes-beaver-desktop (about 9 MB)
cargo install tauri-cli --version "^2" --locked
cd desktop && cargo tauri build    # installers with a launcher entry and the icon: .deb, .rpm and AppImage on Linux, .msi and setup.exe on Windows, .app and .dmg on macOS
```

Build each system's installers on that system.
