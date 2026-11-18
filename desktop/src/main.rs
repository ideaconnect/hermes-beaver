//! hermes-beaver-desktop: the dashboard's web view in a window of its own, on the
//! system's web view (WebKitGTK, WebView2, WKWebView) through Tauri.
//!
//! The window loads `beaver://localhost/`, which [`gateway`] answers: the app's
//! setup page, or the dashboard fetched with the token. No Tauri commands and no
//! capabilities are defined, so the page gets no access to the app beyond what
//! the gateway serves.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod gateway;
mod pages;
mod settings;

use tauri::webview::NewWindowResponse;
use tauri::{AppHandle, Manager, Url, WebviewUrl, WebviewWindowBuilder};

use gateway::{Effect, Gateway, Http};

const SCHEME: &str = "beaver";
const WINDOW: &str = "main";
const KEYS: &str = include_str!("keys.js");

const HELP: &str = "\
hermes-beaver-desktop: hermes-beaver's web view in a window of its own

Usage: hermes-beaver-desktop [--setup] [--fullscreen | --windowed]

  --setup       ask for the dashboard's address and token again
  --fullscreen  start full screen (default: as the window was last time)
  --windowed    start in a window
  -h, --help    this help
  -V, --version the version

In the window: F11 full screen, Esc leaves it, Ctrl+, the settings.
";

struct Args {
    setup: bool,
    fullscreen: Option<bool>,
}

fn parse_args(args: impl Iterator<Item = String>) -> Result<Option<Args>, String> {
    let mut parsed = Args {
        setup: false,
        fullscreen: None,
    };
    for arg in args {
        match arg.as_str() {
            "--setup" => parsed.setup = true,
            "--fullscreen" => parsed.fullscreen = Some(true),
            "--windowed" => parsed.fullscreen = Some(false),
            "-h" | "--help" => {
                print!("{HELP}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("hermes-beaver-desktop {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok(Some(parsed))
}

/// The address of a path on the `beaver` protocol, as this platform's web view
/// names it.
fn app_url(path: &str) -> Url {
    let origin = if cfg!(any(windows, target_os = "android")) {
        "http://beaver.localhost"
    } else {
        "beaver://localhost"
    };
    Url::parse(&format!("{origin}{path}")).expect("a valid path")
}

/// Whether the window may show `url`: only the app's own protocol.
fn is_ours(url: &Url) -> bool {
    let base = app_url("/");
    url.scheme() == base.scheme() && url.host_str() == base.host_str()
}

/// The dashboard links tasks to GitHub, and only there: those open in the
/// system's browser. Anything else is ignored.
fn open_outside(url: &Url) {
    if url.scheme() == "https" && url.host_str() == Some("github.com") {
        if let Err(e) = open::that_detached(url.as_str()) {
            eprintln!("hermes-beaver-desktop: opening {url}: {e}");
        }
    } else {
        eprintln!("hermes-beaver-desktop: not opening {url}");
    }
}

fn apply(app: &AppHandle, effects: Vec<Effect>) {
    let Some(window) = app.get_webview_window(WINDOW) else {
        return;
    };
    for effect in effects {
        match effect {
            Effect::Fullscreen(to) => {
                let now = window.is_fullscreen().unwrap_or(false);
                let want = to.unwrap_or(!now);
                if want != now {
                    let _ = window.set_fullscreen(want);
                }
                app.state::<Gateway>().remember_fullscreen(want);
            }
            Effect::Navigate(path) => {
                let _ = window.navigate(app_url(path));
            }
        }
    }
}

fn main() {
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(Some(args)) => args,
        Ok(None) => return,
        Err(e) => {
            eprintln!("hermes-beaver-desktop: {e}\n\n{HELP}");
            std::process::exit(2);
        }
    };

    tauri::Builder::default()
        .register_asynchronous_uri_scheme_protocol(SCHEME, |ctx, request, responder| {
            let app = ctx.app_handle().clone();
            // The dashboard may take a while to answer: never on the UI thread.
            std::thread::spawn(move || {
                let (response, effects) = app.state::<Gateway>().handle(&request);
                responder.respond(response);
                apply(&app, effects);
            });
        })
        .setup(move |app| {
            let path = app.path().app_config_dir()?.join("settings.json");
            let gateway = Gateway::new(path, Box::new(Http::new()));
            let settings = gateway.settings();
            app.manage(gateway);
            let start = if args.setup || !settings.configured() {
                "/__app/setup"
            } else {
                "/"
            };
            WebviewWindowBuilder::new(app, WINDOW, WebviewUrl::CustomProtocol(app_url(start)))
                .title("hermes-beaver")
                .inner_size(1440.0, 900.0)
                .min_inner_size(480.0, 360.0)
                .fullscreen(args.fullscreen.unwrap_or(settings.fullscreen))
                .initialization_script(KEYS)
                .on_navigation(|url| {
                    let ours = is_ours(url);
                    if !ours {
                        open_outside(url);
                    }
                    ours
                })
                .on_new_window(|url, _features| {
                    open_outside(&url);
                    NewWindowResponse::Deny
                })
                // "(2) Beaver · susel": the page's title, in the task bar too.
                .on_document_title_changed(|window, title| {
                    let _ = window.set_title(&title);
                })
                .build()?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("hermes-beaver-desktop failed to start");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_our_own_pages_load_in_the_window() {
        assert!(is_ours(&app_url("/")));
        assert!(is_ours(&app_url("/__app/setup?e=token")));
        for other in [
            "https://github.com/o/r/issues/1",
            "http://susel:8788/",
            "beaver://elsewhere/",
            "http://localhost/",
        ] {
            assert!(!is_ours(&Url::parse(other).unwrap()), "{other}");
        }
    }

    #[test]
    fn arguments() {
        let parse = |a: &[&str]| parse_args(a.iter().map(|s| s.to_string()));
        let a = parse(&["--setup", "--fullscreen"]).unwrap().unwrap();
        assert!(a.setup);
        assert_eq!(a.fullscreen, Some(true));
        let a = parse(&["--windowed"]).unwrap().unwrap();
        assert_eq!((a.setup, a.fullscreen), (false, Some(false)));
        assert_eq!(parse(&[]).unwrap().unwrap().fullscreen, None);
        assert!(parse(&["--bogus"]).is_err());
    }
}
