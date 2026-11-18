//! The window's only source: the `beaver://` protocol. It serves the app's own
//! pages under `/__app/` and passes every other request to the dashboard's web
//! view with the token, so the page runs unchanged and never sees the token.
//!
//! - `GET /__app/setup` shows the setup form; `POST` checks the address and token
//!   against the dashboard (`/state.json`), saves them and opens the dashboard.
//!   No redirects anywhere: WebKitGTK doesn't follow them from a custom protocol,
//!   so pages move on with `<meta http-equiv="refresh">`.
//! - `POST /__app/fullscreen?to=toggle|off`: F11 and Esc, from the window's key
//!   handler.
//! - Anything else (`GET`/`HEAD`) goes to the dashboard. A refused token shows the
//!   setup form again; an unreachable dashboard, a page that retries.

use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use tauri::http::{Method, Request, Response, StatusCode, header};

use crate::pages;
use crate::settings::{Settings, normalize_server};

/// What the window should do after a response, besides showing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Full screen on or off: `None` toggles.
    Fullscreen(Option<bool>),
    /// The dashboard refused the token while a page was open: back to the setup
    /// form, at this path.
    Navigate(&'static str),
}

/// Where the setup form says the saved token was refused.
pub const SETUP_REFUSED: &str = "/__app/setup?e=token";

/// An answer from the dashboard.
pub struct Fetched {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Fetches from the dashboard; a trait so that tests can stand in for it.
pub trait Upstream: Send + Sync {
    /// `GET url`, with `Authorization: Bearer token` unless `token` is empty.
    /// `Err` is a sentence for people: the dashboard couldn't be reached.
    fn get(&self, url: &str, token: &str) -> Result<Fetched, String>;
}

/// The real one: plain HTTP on the LAN, HTTPS when the address says so.
pub struct Http {
    agent: ureq::Agent,
}

impl Http {
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(4)))
            .timeout_global(Some(Duration::from_secs(10)))
            // The web view never redirects; don't take the token anywhere else.
            .max_redirects(0)
            .user_agent(concat!("hermes-beaver-desktop/", env!("CARGO_PKG_VERSION")))
            .build();
        Self {
            agent: config.into(),
        }
    }
}

impl Upstream for Http {
    fn get(&self, url: &str, token: &str) -> Result<Fetched, String> {
        let mut req = self.agent.get(url);
        if !token.is_empty() {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        let mut resp = req.call().map_err(|e| e.to_string())?;
        let headers = resp
            .headers()
            .iter()
            .filter_map(|(k, v)| Some((k.as_str().to_owned(), v.to_str().ok()?.to_owned())))
            .collect();
        let body = resp
            .body_mut()
            .with_config()
            .limit(16 << 20)
            .read_to_vec()
            .map_err(|e| e.to_string())?;
        Ok(Fetched {
            status: resp.status().as_u16(),
            headers,
            body,
        })
    }
}

/// The dashboard's response headers the window gets; the rest (hop-by-hop,
/// `WWW-Authenticate`, which would bring up the web view's own prompt) stay here.
const PASSED_HEADERS: &[&str] = &[
    "content-type",
    "cache-control",
    "content-security-policy",
    "x-content-type-options",
    "referrer-policy",
];

pub struct Gateway {
    path: PathBuf,
    settings: Mutex<Settings>,
    upstream: Box<dyn Upstream>,
}

impl Gateway {
    pub fn new(path: PathBuf, upstream: Box<dyn Upstream>) -> Self {
        let settings = Settings::load(&path);
        Self {
            path,
            settings: Mutex::new(settings),
            upstream,
        }
    }

    pub fn settings(&self) -> Settings {
        self.settings
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn update(&self, change: impl FnOnce(&mut Settings)) {
        let mut s = self.settings.lock().unwrap_or_else(PoisonError::into_inner);
        change(&mut s);
        if let Err(e) = s.save(&self.path) {
            eprintln!("hermes-beaver-desktop: saving {}: {e}", self.path.display());
        }
    }

    /// Remembers the window's state for the next start.
    pub fn remember_fullscreen(&self, on: bool) {
        if self.settings().fullscreen != on {
            self.update(|s| s.fullscreen = on);
        }
    }

    pub fn handle(&self, req: &Request<Vec<u8>>) -> (Response<Vec<u8>>, Vec<Effect>) {
        let path = req.uri().path();
        let query = req.uri().query().unwrap_or_default();
        match (req.method(), path) {
            (&Method::GET, "/__app/setup") => {
                let message = query
                    .split('&')
                    .any(|p| p == "e=token")
                    .then_some("The dashboard refused the saved token. Enter its current one.");
                (
                    html(
                        StatusCode::OK,
                        pages::setup(&self.settings().server, message),
                    ),
                    vec![],
                )
            }
            (&Method::POST, "/__app/setup") => (self.setup(req.body()), vec![]),
            (&Method::POST, "/__app/fullscreen") => {
                let to = if query.split('&').any(|p| p == "to=off") {
                    Some(false)
                } else {
                    None
                };
                (empty(StatusCode::NO_CONTENT), vec![Effect::Fullscreen(to)])
            }
            (_, p) if p.starts_with("/__app/") => {
                (text(StatusCode::NOT_FOUND, "Not found.\n"), vec![])
            }
            (&Method::GET | &Method::HEAD, _) => self.proxy(path, query),
            _ => (
                text(
                    StatusCode::METHOD_NOT_ALLOWED,
                    "The dashboard is read-only.\n",
                ),
                vec![],
            ),
        }
    }

    /// The setup form was sent: check, save, open the dashboard.
    fn setup(&self, body: &[u8]) -> Response<Vec<u8>> {
        let form = parse_form(body);
        let field = |name: &str| {
            form.iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.trim().to_owned())
                .unwrap_or_default()
        };
        let (typed, token) = (field("server"), field("token"));
        let again = |message: &str| html(StatusCode::OK, pages::setup(&typed, Some(message)));
        let server = match normalize_server(&typed) {
            Ok(s) => s,
            Err(message) => return again(&message),
        };
        let checked = match self.upstream.get(&format!("{server}/state.json"), &token) {
            Err(e) => Err(format!("Can't reach {server}: {e}")),
            Ok(r) => match r.status {
                200 if looks_like_the_feed(&r.body) => Ok(()),
                200 => Err(format!(
                    "{server} answers, but it isn't hermes-beaver's web view."
                )),
                401 if token.is_empty() => Err("The dashboard needs its token.".into()),
                401 => Err("The dashboard refused this token.".into()),
                403 => Err(format!(
                    "The dashboard answers to its IP address or LAN name only, not to the \
                     host name in {server}."
                )),
                s => Err(format!("{server} answered with HTTP {s}.")),
            },
        };
        match checked {
            Ok(()) => {
                self.update(|s| {
                    s.server = server;
                    s.token = token;
                });
                html(StatusCode::OK, pages::opening())
            }
            Err(message) => html(StatusCode::OK, pages::setup(&server, Some(&message))),
        }
    }

    fn proxy(&self, path: &str, query: &str) -> (Response<Vec<u8>>, Vec<Effect>) {
        let settings = self.settings();
        let document = matches!(path, "/" | "/index.html");
        if !settings.configured() {
            return if document {
                (html(StatusCode::OK, pages::setup("", None)), vec![])
            } else {
                (
                    text(StatusCode::SERVICE_UNAVAILABLE, "Not set up yet.\n"),
                    vec![],
                )
            };
        }
        let mut url = format!("{}{path}", settings.server);
        if !query.is_empty() {
            url.push('?');
            url.push_str(query);
        }
        let fetched = match self.upstream.get(&url, &settings.token) {
            Ok(f) => f,
            Err(e) if document => {
                let page = pages::unreachable(&settings.server, &format!("Can't reach it: {e}"));
                return (html(StatusCode::OK, page), vec![]);
            }
            Err(e) => return (text(StatusCode::BAD_GATEWAY, &format!("{e}\n")), vec![]),
        };
        match fetched.status {
            401 if document => {
                let page = pages::setup(
                    &settings.server,
                    Some("The dashboard refused the saved token. Enter its current one."),
                );
                (html(StatusCode::OK, page), vec![])
            }
            // The token changed while the page was open.
            401 => (
                text(StatusCode::UNAUTHORIZED, "Token refused.\n"),
                vec![Effect::Navigate(SETUP_REFUSED)],
            ),
            s if document && !(200..300).contains(&s) => {
                let said = String::from_utf8_lossy(&fetched.body);
                let said = said.lines().next().unwrap_or_default().trim();
                let message = format!("It answered with HTTP {s}. {said}");
                let page = pages::unreachable(&settings.server, message.trim());
                (html(StatusCode::OK, page), vec![])
            }
            _ => (passed(fetched), vec![]),
        }
    }
}

/// Whether a 200 from `/state.json` is the web view's feed and not some other site.
fn looks_like_the_feed(body: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(body)
        .is_ok_and(|v| v.get("build").is_some_and(|b| b.is_string()))
}

fn passed(f: Fetched) -> Response<Vec<u8>> {
    let mut resp = Response::builder().status(f.status);
    for (name, value) in &f.headers {
        if PASSED_HEADERS.contains(&name.to_ascii_lowercase().as_str()) {
            resp = resp.header(name.as_str(), value.as_str());
        }
    }
    resp.body(f.body)
        .unwrap_or_else(|_| text(StatusCode::BAD_GATEWAY, "Bad response.\n"))
}

fn html(status: StatusCode, body: String) -> Response<Vec<u8>> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::CONTENT_SECURITY_POLICY, pages::CSP)
        .body(body.into_bytes())
        .expect("static headers")
}

fn text(status: StatusCode, body: &str) -> Response<Vec<u8>> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .body(body.as_bytes().to_vec())
        .expect("static headers")
}

fn empty(status: StatusCode) -> Response<Vec<u8>> {
    Response::builder()
        .status(status)
        .body(Vec::new())
        .expect("static headers")
}

/// `application/x-www-form-urlencoded`, as the setup form sends it.
fn parse_form(body: &[u8]) -> Vec<(String, String)> {
    String::from_utf8_lossy(body)
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (form_decode(k), form_decode(v))
        })
        .collect()
}

fn form_decode(s: &str) -> String {
    let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => match (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                (Some(high), Some(low)) => {
                    out.push(high << 4 | low);
                    i += 2;
                }
                _ => out.push(b'%'),
            },
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    const TOKEN: &str = "0123456789abcdef0123";
    const FEED: &str = r#"{"build":"0.1.0-abc","tasks":[]}"#;

    /// A dashboard that wants `TOKEN` and records what it was asked.
    #[derive(Default)]
    struct Fake {
        down: bool,
        token: Mutex<String>,
        asked: Arc<Mutex<Vec<(String, String)>>>,
    }

    impl Fake {
        fn wanting(token: &str) -> Self {
            Self {
                token: Mutex::new(token.into()),
                ..Default::default()
            }
        }
    }

    impl Upstream for Arc<Fake> {
        fn get(&self, url: &str, token: &str) -> Result<Fetched, String> {
            self.asked
                .lock()
                .unwrap()
                .push((url.to_owned(), token.to_owned()));
            if self.down {
                return Err("connection refused".into());
            }
            let want = self.token.lock().unwrap().clone();
            if !want.is_empty() && token != want {
                return Ok(Fetched {
                    status: 401,
                    headers: vec![("WWW-Authenticate".into(), "Basic realm=\"x\"".into())],
                    body: b"This dashboard needs its token.\n".to_vec(),
                });
            }
            let (ctype, body) = match url.rsplit_once('/').map(|(_, p)| p) {
                Some(p) if p.starts_with("state.json") => ("application/json", FEED),
                Some("") => ("text/html; charset=utf-8", "<!doctype html><p>dashboard"),
                _ => {
                    return Ok(Fetched {
                        status: 404,
                        headers: vec![],
                        body: b"Not found.\n".to_vec(),
                    });
                }
            };
            Ok(Fetched {
                status: 200,
                headers: vec![
                    ("Content-Type".into(), ctype.into()),
                    (
                        "Content-Security-Policy".into(),
                        "default-src 'none'".into(),
                    ),
                    ("Connection".into(), "close".into()),
                    ("Set-Cookie".into(), "x=1".into()),
                ],
                body: body.as_bytes().to_vec(),
            })
        }
    }

    struct Setup {
        gateway: Gateway,
        fake: Arc<Fake>,
        dir: PathBuf,
    }

    impl Drop for Setup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn setup(name: &str, fake: Fake, saved: Option<Settings>) -> Setup {
        let dir = std::env::temp_dir().join(format!("hbd-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("settings.json");
        if let Some(s) = saved {
            s.save(&path).unwrap();
        }
        let fake = Arc::new(fake);
        Setup {
            gateway: Gateway::new(path, Box::new(fake.clone())),
            fake,
            dir,
        }
    }

    fn saved() -> Settings {
        Settings {
            server: "http://susel:8788".into(),
            token: TOKEN.into(),
            fullscreen: false,
        }
    }

    fn request(method: Method, uri: &str, body: &str) -> Request<Vec<u8>> {
        Request::builder()
            .method(method)
            .uri(format!("beaver://localhost{uri}"))
            .body(body.as_bytes().to_vec())
            .unwrap()
    }

    fn get(g: &Gateway, uri: &str) -> (Response<Vec<u8>>, Vec<Effect>) {
        g.handle(&request(Method::GET, uri, ""))
    }

    fn body(r: &Response<Vec<u8>>) -> String {
        String::from_utf8(r.body().clone()).unwrap()
    }

    fn hdr<'a>(r: &'a Response<Vec<u8>>, name: &str) -> Option<&'a str> {
        r.headers().get(name).and_then(|v| v.to_str().ok())
    }

    #[test]
    fn first_launch_asks_for_the_dashboard() {
        let t = setup("first", Fake::wanting(TOKEN), None);
        let (r, fx) = get(&t.gateway, "/");
        assert_eq!(r.status(), StatusCode::OK);
        assert!(
            body(&r).contains("action=\"/__app/setup\""),
            "the form, at once"
        );
        assert!(fx.is_empty());
        let (r, _) = get(&t.gateway, "/__app/setup");
        assert_eq!(r.status(), StatusCode::OK);
        assert!(body(&r).contains("name=\"token\""));
        assert_eq!(hdr(&r, "content-security-policy"), Some(pages::CSP));
        assert!(
            t.fake.asked.lock().unwrap().is_empty(),
            "nothing to ask yet"
        );
        let (r, _) = get(&t.gateway, "/state.json");
        assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn setup_checks_the_token_before_saving_it() {
        let t = setup("check", Fake::wanting(TOKEN), None);
        let post = |form: &str| {
            t.gateway
                .handle(&request(Method::POST, "/__app/setup", form))
                .0
        };

        let r = post("server=susel%3A8788&token=wrong");
        assert_eq!(r.status(), StatusCode::OK);
        assert!(body(&r).contains("refused this token"));
        assert!(body(&r).contains("value=\"http://susel:8788\""));
        assert!(!body(&r).contains("wrong"), "the token is never echoed");
        assert!(!t.gateway.settings().configured());

        let r = post("server=susel%3A8788&token=");
        assert!(body(&r).contains("needs its token"));

        let r = post("server=ftp%3A%2F%2Fsusel&token=x");
        assert!(body(&r).contains("http:// or https://"));

        let r = post(&format!("server=+susel%3A8788%2F+&token={TOKEN}"));
        assert_eq!(r.status(), StatusCode::OK);
        assert!(
            body(&r).contains("http-equiv=\"refresh\" content=\"0; url=/\""),
            "moves on to the dashboard (no redirect: WebKitGTK ignores those here)"
        );
        assert_eq!(t.gateway.settings(), saved());
        assert_eq!(Settings::load(&t.gateway.path), saved(), "saved to disk");
        let asked = t.fake.asked.lock().unwrap().clone();
        assert_eq!(
            asked.last().unwrap(),
            &("http://susel:8788/state.json".to_owned(), TOKEN.to_owned())
        );
    }

    #[test]
    fn setup_tells_what_went_wrong() {
        let t = setup(
            "down",
            Fake {
                down: true,
                ..Default::default()
            },
            None,
        );
        let r = t
            .gateway
            .handle(&request(
                Method::POST,
                "/__app/setup",
                "server=susel:8788&token=",
            ))
            .0;
        assert!(body(&r).contains("Can&#39;t reach http://susel:8788: connection refused"));
        assert!(!t.gateway.settings().configured());

        // Some other site on that address.
        struct Other;
        impl Upstream for Other {
            fn get(&self, _: &str, _: &str) -> Result<Fetched, String> {
                Ok(Fetched {
                    status: 200,
                    headers: vec![],
                    body: b"<html>router login</html>".to_vec(),
                })
            }
        }
        let g = Gateway::new(t.dir.join("other.json"), Box::new(Other));
        let r = g
            .handle(&request(
                Method::POST,
                "/__app/setup",
                "server=192.168.1.1&token=",
            ))
            .0;
        assert!(body(&r).contains("isn&#39;t hermes-beaver&#39;s web view"));
    }

    #[test]
    fn a_web_view_without_token_needs_none() {
        let t = setup("open", Fake::default(), None);
        let r = t
            .gateway
            .handle(&request(
                Method::POST,
                "/__app/setup",
                "server=susel:8788&token=",
            ))
            .0;
        assert!(body(&r).contains("Connected"));
        let (r, _) = get(&t.gateway, "/state.json");
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(t.fake.asked.lock().unwrap().last().unwrap().1, "");
    }

    #[test]
    fn proxies_the_dashboard_with_the_token() {
        let t = setup("proxy", Fake::wanting(TOKEN), Some(saved()));
        let (r, fx) = get(&t.gateway, "/state.json?t=1");
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(body(&r), FEED);
        assert!(fx.is_empty());
        assert_eq!(hdr(&r, "content-type"), Some("application/json"));
        assert_eq!(
            hdr(&r, "content-security-policy"),
            Some("default-src 'none'"),
            "the page keeps its own policy"
        );
        assert_eq!(hdr(&r, "connection"), None);
        assert_eq!(hdr(&r, "set-cookie"), None);
        let (r, _) = get(&t.gateway, "/");
        assert!(body(&r).contains("dashboard"));
        let (r, _) = get(&t.gateway, "/nope");
        assert_eq!(r.status(), StatusCode::NOT_FOUND, "passed on as it is");
        let asked = t.fake.asked.lock().unwrap().clone();
        assert_eq!(
            asked[0],
            (
                "http://susel:8788/state.json?t=1".to_owned(),
                TOKEN.to_owned()
            )
        );
        assert_eq!(asked[1].0, "http://susel:8788/");
    }

    #[test]
    fn a_refused_token_goes_back_to_setup() {
        let t = setup("refused", Fake::wanting(TOKEN), Some(saved()));
        *t.fake.token.lock().unwrap() = "a-new-token-0123456789".into();
        let (r, fx) = get(&t.gateway, "/state.json");
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            hdr(&r, "www-authenticate"),
            None,
            "no prompt from the web view"
        );
        assert_eq!(fx, vec![Effect::Navigate(SETUP_REFUSED)]);
        let (r, fx) = get(&t.gateway, "/");
        assert_eq!(r.status(), StatusCode::OK);
        assert!(body(&r).contains("refused the saved token"));
        assert!(fx.is_empty());
        let (r, _) = get(&t.gateway, SETUP_REFUSED);
        assert!(body(&r).contains("refused the saved token"));
        assert!(body(&r).contains("value=\"http://susel:8788\""));
    }

    #[test]
    fn an_unreachable_dashboard_shows_a_page_that_retries() {
        let t = setup(
            "unreachable",
            Fake {
                down: true,
                ..Default::default()
            },
            Some(saved()),
        );
        let (r, _) = get(&t.gateway, "/");
        assert_eq!(r.status(), StatusCode::OK);
        assert!(body(&r).contains("Can't open the dashboard"));
        assert!(body(&r).contains("Can&#39;t reach it: connection refused"));
        assert!(body(&r).contains("http-equiv=\"refresh\""));
        // The page's own poll gets an error and shows the data's age.
        let (r, _) = get(&t.gateway, "/state.json");
        assert_eq!(r.status(), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn keys_and_methods() {
        let t = setup("keys", Fake::wanting(TOKEN), Some(saved()));
        let post = |uri: &str| t.gateway.handle(&request(Method::POST, uri, ""));
        let (r, fx) = post("/__app/fullscreen?to=toggle");
        assert_eq!(r.status(), StatusCode::NO_CONTENT);
        assert_eq!(fx, vec![Effect::Fullscreen(None)]);
        assert_eq!(
            post("/__app/fullscreen?to=off").1,
            vec![Effect::Fullscreen(Some(false))]
        );
        // A link or an <img> can't toggle it: only a POST does.
        assert!(get(&t.gateway, "/__app/fullscreen").1.is_empty());
        assert_eq!(post("/__app/nope").0.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            post("/state.json").0.status(),
            StatusCode::METHOD_NOT_ALLOWED
        );
        assert!(
            t.fake.asked.lock().unwrap().is_empty(),
            "none of these reach the dashboard"
        );

        t.gateway.remember_fullscreen(true);
        assert!(Settings::load(&t.gateway.path).fullscreen);
        assert_eq!(
            Settings::load(&t.gateway.path).token,
            TOKEN,
            "the rest is kept"
        );
    }

    #[test]
    fn http_sends_the_token_and_follows_no_redirect() {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        // Answers each request with the next canned response; hands back what came in.
        let server = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for answer in [
                "HTTP/1.1 302 Found\r\nLocation: http://elsewhere.example/\r\n",
                "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"x\"\r\n",
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n",
            ] {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut head = String::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                        break;
                    }
                    head.push_str(&line);
                }
                seen.push(head);
                let mut stream = stream;
                write!(
                    stream,
                    "{answer}Content-Length: 2\r\nConnection: close\r\n\r\n{{}}"
                )
                .unwrap();
            }
            seen
        });
        let http = Http::new();
        let url = format!("http://{addr}/state.json");
        let r = http.get(&url, TOKEN).unwrap();
        assert_eq!(r.status, 302, "a redirect comes back as it is");
        let r = http.get(&url, "").unwrap();
        assert_eq!(r.status, 401, "an error status is an answer, not an Err");
        let r = http.get(&url, TOKEN).unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body, b"{}");
        assert!(
            r.headers
                .iter()
                .any(|(k, v)| k == "content-type" && v == "application/json")
        );
        let seen = server.join().unwrap();
        let auth = |head: &str| {
            head.lines()
                .find_map(|l| {
                    l.strip_prefix("authorization: ")
                        .or(l.strip_prefix("Authorization: "))
                })
                .map(str::to_owned)
        };
        assert_eq!(auth(&seen[0]), Some(format!("Bearer {TOKEN}")));
        assert_eq!(auth(&seen[1]), None, "no token, no header");
        assert!(
            seen.iter()
                .all(|h| h.starts_with("GET /state.json HTTP/1.1"))
        );
        assert!(
            Http::new().get("http://127.0.0.1:1/", "").is_err(),
            "nothing listening is an Err"
        );
    }

    #[test]
    fn forms_decode() {
        assert_eq!(
            parse_form(b"server=http%3A%2F%2Fsusel%3A8788&token=a+b%2Bc&x"),
            vec![
                ("server".into(), "http://susel:8788".into()),
                ("token".into(), "a b+c".into()),
                ("x".into(), String::new()),
            ]
        );
        assert_eq!(form_decode("100%"), "100%");
        assert_eq!(form_decode("%zz%4"), "%zz%4");
        assert_eq!(form_decode("%C5%82"), "ł");
    }
}
