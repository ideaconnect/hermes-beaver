//! `hermes-beaver web`: the dashboard as a read-only page for other computers on
//! the LAN. It's its own process, so it keeps working while the kiosk or sway is
//! down, and nothing it does can freeze the screen.
//!
//! - It reads what the kiosk reads, on the same 10 s tick: the store (read-only),
//!   Hermes's files, /proc and /sys. It never writes; reconciling is the kiosk's job.
//! - Three routes: `/` (one page, CSS and JS inline), `/state.json` and
//!   `/favicon.svg`. `/state.json` is the page's own feed, polled every 5 s, and not
//!   an API: its shape follows the page, and scripts use MCP or `call`. It carries
//!   the build, and an open page reloads itself when that changes.
//! - No login and no TLS: it listens where `[web] listen` says, on the LAN.

use std::net::{IpAddr, Ipv6Addr};
use std::path::Path;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow};
use chrono::{Local, TimeZone};
use serde::Serialize;
use tiny_http::Method;

use crate::config::Config;
use crate::display::{self, Schedule};
use crate::feed::{Access, Feed, Mode};
use crate::store;
use crate::view::{self, Dashboard, Fit, State};

const PAGE: &str = include_str!("../web/index.html");
const FAVICON: &str = include_str!("../web/favicon.svg");

const TICK: Duration = Duration::from_secs(10);

/// Inline script and styles only; data is fetched from this server, and links leave
/// for GitHub in a new tab.
const CSP: &str = "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; \
                   connect-src 'self'; img-src 'self'; base-uri 'none'; form-action 'none'; \
                   frame-ancestors 'none'";

pub fn serve(config: &Config, mode: &Mode, store_path: &Path) -> Result<()> {
    let addr = config.web_listen()?;
    let server = tiny_http::Server::http(addr).map_err(|e| anyhow!("listening on {addr}: {e}"))?;
    let mut feed = Feed::new(config, mode, store_path, Access::ReadOnly)?;
    feed.refresh(store::now());
    let site = Arc::new(Site::new(config, mode, build_id(), feed.state.clone()));
    eprintln!("hermes-beaver: web view on http://{addr}/");

    let server = Arc::new(server);
    let http = {
        let (server, site) = (server.clone(), site.clone());
        std::thread::Builder::new()
            .name("http".into())
            .spawn(move || {
                for req in server.incoming_requests() {
                    site.handle(req);
                }
            })?
    };

    // First metrics shortly after start, then every 10 s, as on the kiosk. Until a
    // signal arrives; `stop_tx` lives to the end, so the channel never disconnects.
    let (stop_tx, stop) = mpsc::channel();
    watch_signals(stop_tx.clone());
    let mut next = Instant::now() + Duration::from_secs(2);
    while let Err(RecvTimeoutError::Timeout) =
        stop.recv_timeout(next.saturating_duration_since(Instant::now()))
    {
        feed.sample_metrics();
        feed.refresh(store::now());
        site.publish(feed.state.clone());
        // After a suspend, carry on from now instead of catching up.
        next = (next + TICK).max(Instant::now());
    }
    server.unblock();
    let _ = http.join();
    // Dropping the feed removes the demo's store.
    drop(feed);
    Ok(())
}

/// What the responses are made of: the page's fixed parts and the latest state.
struct Site {
    config: Config,
    schedule: Schedule,
    build: String,
    page: String,
    hostname: String,
    name: String,
    mascot_title: String,
    state: Mutex<Arc<State>>,
}

/// The page's feed (internal: see the module docs).
#[derive(Serialize)]
struct PageState<'a> {
    build: &'a str,
    /// The server's clock, which the page ticks on from.
    now_ms: i64,
    /// The configured time zone's offset now, so the page shows the host's time.
    utc_offset_secs: i32,
    hostname: &'a str,
    /// The animal's name as configured, e.g. "Suseł".
    name: &'a str,
    mascot_title: &'a str,
    mascot_font_px: f32,
    /// The monitor's state, e.g. "Screen in standby until 09:00".
    screen: String,
    #[serde(flatten)]
    dashboard: Dashboard,
}

struct Reply {
    status: u16,
    headers: Vec<(&'static str, &'static str)>,
    body: Vec<u8>,
}

impl Reply {
    fn new(status: u16, content_type: &'static str, body: Vec<u8>) -> Self {
        Self {
            status,
            headers: vec![
                ("Content-Type", content_type),
                ("X-Content-Type-Options", "nosniff"),
                // Links to GitHub mustn't tell it where the dashboard lives.
                ("Referrer-Policy", "no-referrer"),
            ],
            body,
        }
    }

    fn text(status: u16, body: &str) -> Self {
        Self::new(
            status,
            "text/plain; charset=utf-8",
            body.as_bytes().to_vec(),
        )
    }

    fn with(mut self, name: &'static str, value: &'static str) -> Self {
        self.headers.push((name, value));
        self
    }
}

impl Site {
    fn new(config: &Config, mode: &Mode, build: String, state: State) -> Self {
        let host = view::hostname();
        Self {
            config: config.clone(),
            schedule: config.schedule(),
            page: PAGE.replace("__BUILD__", &build),
            build,
            hostname: match mode {
                Mode::Live => host,
                Mode::Demo(_) => format!("{host} · demo"),
            },
            name: view::animal_name(&config.ui),
            mascot_title: view::mascot_title(&config.ui),
            state: Mutex::new(Arc::new(state)),
        }
    }

    fn publish(&self, state: State) {
        *self.state.lock().unwrap_or_else(PoisonError::into_inner) = Arc::new(state);
    }

    fn handle(&self, req: tiny_http::Request) {
        let host = req
            .headers()
            .iter()
            .find(|h| h.field.equiv("Host"))
            .map(|h| h.value.as_str().to_owned());
        let reply = self.reply(req.method(), req.url(), host.as_deref(), now_ms());
        let mut resp = tiny_http::Response::from_data(reply.body).with_status_code(reply.status);
        for (name, value) in reply.headers {
            if let Ok(h) = tiny_http::Header::from_bytes(name, value) {
                resp.add_header(h);
            }
        }
        let _ = req.respond(resp);
    }

    fn reply(&self, method: &Method, url: &str, host: Option<&str>, now_ms: i64) -> Reply {
        if !matches!(method, Method::Get | Method::Head) {
            return Reply::text(405, "This page is read-only.\n").with("Allow", "GET, HEAD");
        }
        if host.is_some_and(|h| !local_host(h)) {
            return Reply::text(
                403,
                "This page answers to its IP address or LAN name only.\n",
            );
        }
        match url.split(['?', '#']).next().unwrap_or_default() {
            "/" | "/index.html" => Reply::new(
                200,
                "text/html; charset=utf-8",
                self.page.clone().into_bytes(),
            )
            .with("Cache-Control", "no-store")
            .with("Content-Security-Policy", CSP),
            "/state.json" => {
                let body = serde_json::to_vec(&self.page_state(now_ms)).unwrap_or_default();
                Reply::new(200, "application/json", body).with("Cache-Control", "no-store")
            }
            "/favicon.svg" => Reply::new(200, "image/svg+xml", FAVICON.as_bytes().to_vec())
                .with("Cache-Control", "max-age=86400"),
            _ => Reply::text(404, "Not found.\n"),
        }
    }

    fn page_state(&self, now_ms: i64) -> PageState<'_> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let now = now_ms.div_euclid(1000);
        PageState {
            build: &self.build,
            now_ms,
            utc_offset_secs: Local
                .timestamp_opt(now, 0)
                .single()
                .map(|t| t.offset().local_minus_utc())
                .unwrap_or(0),
            hostname: &self.hostname,
            name: &self.name,
            mascot_title: &self.mascot_title,
            mascot_font_px: view::mascot_font_px(self.config.ui.animal),
            screen: display::status_line(&state.snapshot.display, &self.schedule, now),
            dashboard: view::dashboard(&state, &self.config, now, Fit::All, view::loadavg()),
        }
    }
}

/// Whether a request's `Host` names this machine the way a LAN does: an IP address,
/// a single-label name such as `susel`, or a local suffix. Any other name means a
/// page on some site resolved its own name to this address (DNS rebinding) to read
/// the dashboard from a browser on the LAN.
fn local_host(host: &str) -> bool {
    if let Some(rest) = host.strip_prefix('[') {
        return rest
            .split(']')
            .next()
            .is_some_and(|ip| ip.parse::<Ipv6Addr>().is_ok());
    }
    let name = match host.rsplit_once(':') {
        Some((name, port)) if port.chars().all(|c| c.is_ascii_digit()) => name,
        _ => host,
    };
    if name.parse::<IpAddr>().is_ok() {
        return true;
    }
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    const LOCAL: &[&str] = &[".local", ".lan", ".home", ".home.arpa", ".internal"];
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
        && (!name.contains('.') || LOCAL.iter().any(|s| name.ends_with(s)))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// This binary's version and build time. An open page reloads itself when it changes,
/// so a tab left open across a deploy never runs old code against new JSON.
fn build_id() -> String {
    let built = std::env::current_exe()
        .and_then(|p| p.metadata())
        .and_then(|m| m.modified())
        .unwrap_or_else(|_| SystemTime::now());
    let ms = built
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("{}-{ms:x}", env!("CARGO_PKG_VERSION"))
}

/// SIGTERM/SIGINT/SIGHUP end the tick loop, so the demo's store is removed on exit.
fn watch_signals(stop: mpsc::Sender<()>) {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
    let mut signals = match signal_hook::iterator::Signals::new([SIGTERM, SIGINT, SIGHUP]) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("hermes-beaver: not watching signals: {e}");
            return;
        }
    };
    let spawned = std::thread::Builder::new()
        .name("signals".into())
        .spawn(move || {
            if signals.forever().next().is_some() {
                let _ = stop.send(());
            }
        });
    if let Err(e) = spawned {
        eprintln!("hermes-beaver: not watching signals: {e}");
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};

    use serde_json::{Value, json};

    use super::*;
    use crate::store::Store;
    use crate::tools::call_at;

    fn site(state: State) -> Site {
        let mut config = Config::default();
        config.ui.animal_name = Some("Suseł".into());
        Site::new(&config, &Mode::Live, "1.0-abc".into(), state)
    }

    fn get(site: &Site, path: &str, host: Option<&str>) -> Reply {
        site.reply(&Method::Get, path, host, 1_793_713_141_000)
    }

    fn header(r: &Reply, name: &str) -> Option<&'static str> {
        r.headers.iter().find(|(n, _)| *n == name).map(|(_, v)| *v)
    }

    #[test]
    fn page_carries_its_build() {
        assert!(PAGE.contains("\"__BUILD__\""));
        let r = get(&site(State::default()), "/", Some("192.168.87.75:8788"));
        assert_eq!(r.status, 200);
        let html = String::from_utf8(r.body.clone()).unwrap();
        assert!(html.contains("\"1.0-abc\"") && !html.contains("__BUILD__"));
        assert_eq!(header(&r, "Content-Security-Policy"), Some(CSP));
        assert_eq!(header(&r, "Cache-Control"), Some("no-store"));
    }

    #[test]
    fn state_shows_every_row_and_links_github_only() {
        let mut s = Store::open_in_memory().unwrap();
        for i in 0..7 {
            call_at(
                &mut s,
                "task_start",
                json!({"source": "github", "title": format!("t{i}"), "ref": format!("o/r#{i}"),
                       "url": format!("https://github.com/o/r/issues/{i}")}),
                1000,
            )
            .unwrap();
        }
        call_at(
            &mut s,
            "task_start",
            json!({"source": "direct", "title": "phish", "url": "javascript:alert(1)"}),
            1000,
        )
        .unwrap();
        let state = State {
            snapshot: s.snapshot(8).unwrap(),
            ..Default::default()
        };
        let r = get(&site(state), "/state.json?t=1", None);
        assert_eq!(r.status, 200);
        assert_eq!(header(&r, "Content-Type"), Some("application/json"));
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["build"], "1.0-abc");
        assert_eq!(v["name"], "Suseł");
        assert_eq!(v["mascot_title"], "SUSEŁ");
        assert_eq!(v["now_ms"], 1_793_713_141_000_i64);
        let tasks = v["tasks"].as_array().unwrap();
        assert_eq!(
            tasks.len(),
            8,
            "the web view doesn't cut lists to the screen"
        );
        assert_eq!(v["tasks_hint"], "");
        assert_eq!(tasks[0]["link"], "https://github.com/o/r/issues/0");
        assert_eq!(tasks[7]["link"], Value::Null);
        assert!(v["screen"].as_str().unwrap().starts_with("Screen on"));
        assert_eq!(v["needs_you"], 0);
        assert_eq!(v["metrics"].as_array().unwrap().len(), 4);
    }

    #[test]
    fn other_routes_methods_and_hosts() {
        let site = site(State::default());
        assert_eq!(get(&site, "/nope", None).status, 404);
        let fav = get(&site, "/favicon.svg", None);
        assert_eq!(fav.status, 200);
        assert_eq!(header(&fav, "Content-Type"), Some("image/svg+xml"));
        let post = site.reply(&Method::Post, "/", None, 0);
        assert_eq!(post.status, 405);
        assert_eq!(header(&post, "Allow"), Some("GET, HEAD"));
        assert_eq!(get(&site, "/", Some("evil.example.com")).status, 403);
        assert_eq!(get(&site, "/state.json", Some("susel:8788")).status, 200);
    }

    #[test]
    fn local_hosts() {
        for ok in [
            "192.168.87.75:8788",
            "192.168.87.75",
            "[::1]:8788",
            "[fe80::1]",
            "susel",
            "susel:8788",
            "localhost:8788",
            "susel.local",
            "susel.lan:8788",
            "susel.home.arpa",
            "SUSEL.LOCAL.",
        ] {
            assert!(local_host(ok), "{ok}");
        }
        for bad in [
            "",
            "evil.com",
            "evil.com:8788",
            "192.168.87.75.nip.io",
            "susel.idct.tech",
            "[nope]:80",
            "bad_name",
        ] {
            assert!(!local_host(bad), "{bad}");
        }
    }

    /// The real server, end to end over TCP.
    #[test]
    fn serves_over_http() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_ip().unwrap();
        let site = site(State::default());
        let handler = std::thread::spawn(move || {
            let req = server.recv().unwrap();
            site.handle(req);
        });
        let mut conn = std::net::TcpStream::connect(addr).unwrap();
        write!(
            conn,
            "GET /state.json HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut out = String::new();
        conn.read_to_string(&mut out).unwrap();
        handler.join().unwrap();
        assert!(out.starts_with("HTTP/1.1 200"), "{out}");
        assert!(out.contains("Referrer-Policy: no-referrer"), "{out}");
        let body = out.split("\r\n\r\n").nth(1).unwrap();
        let v: Value = serde_json::from_str(body).unwrap();
        assert_eq!(v["build"], "1.0-abc");
    }
}
