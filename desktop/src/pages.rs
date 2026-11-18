//! The app's own pages: the setup form and the "can't reach the dashboard" page.
//! Same dark look as the web view; no script, so they work whatever happens.

const ICON: &str = include_str!("../../web/favicon.svg");

/// No scripts or outside resources of their own; `connect-src` lets the window's
/// key handler (F11, Esc) reach the app.
pub const CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; connect-src 'self'; \
                       base-uri 'none'; frame-ancestors 'none'";

const STYLE: &str = r#"
:root { color-scheme: dark; }
* { box-sizing: border-box; }
body { margin: 0; min-height: 100vh; display: grid; place-items: center;
  background: #0e1014; color: #e6e8ee; font: 16px/1.45 system-ui, sans-serif; }
main { width: min(34rem, calc(100vw - 2rem)); background: #161920; border: 1px solid #262a33;
  border-radius: 14px; padding: 2rem; }
header { display: flex; align-items: center; gap: 0.9rem; margin-bottom: 1.4rem; }
header svg { width: 3rem; height: 3rem; flex: none; }
h1 { font-size: 1.35rem; margin: 0; }
h1 + p { margin: 0.1rem 0 0; color: #9aa1ae; }
label { display: block; margin: 1rem 0 0.35rem; font-weight: 600; }
input { width: 100%; padding: 0.6rem 0.75rem; border-radius: 8px; border: 1px solid #343944;
  background: #0e1014; color: inherit; font: inherit; }
input:focus { outline: 2px solid #e8a052; outline-offset: 1px; }
.hint, footer { color: #9aa1ae; font-size: 0.875rem; margin: 0.35rem 0 0; }
.error { margin: 0 0 0.5rem; padding: 0.7rem 0.9rem; border-radius: 8px; background: #3a1d1f;
  color: #ffb4ab; border: 1px solid #5c2b2e; }
.actions { display: flex; gap: 0.75rem; margin-top: 1.5rem; align-items: center; }
button, .button { padding: 0.6rem 1.2rem; border-radius: 8px; border: 0; background: #e8a052;
  color: #161920; font: inherit; font-weight: 700; cursor: pointer; text-decoration: none; }
.link { color: #e8a052; }
code { font-size: 0.85em; background: #0e1014; padding: 0.05rem 0.3rem; border-radius: 4px; }
footer { margin-top: 1.5rem; border-top: 1px solid #262a33; padding-top: 0.9rem; }
kbd { font: inherit; font-size: 0.8rem; border: 1px solid #343944; border-radius: 4px;
  padding: 0 0.3rem; }
"#;

const KEYS: &str = "<footer><kbd>F11</kbd> full screen · <kbd>Esc</kbd> leave full screen · \
                    <kbd>Ctrl</kbd>+<kbd>,</kbd> these settings</footer>";

/// Escapes text for HTML content and attribute values.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

fn page(title: &str, head_extra: &str, body: &str) -> String {
    format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         {head_extra}<title>{title}</title><style>{STYLE}</style></head>\
         <body><main>{body}</main></body></html>\n",
        title = escape(title),
    )
}

fn error_box(message: Option<&str>) -> String {
    message
        .map(|m| format!("<p class=\"error\" role=\"alert\">{}</p>", escape(m)))
        .unwrap_or_default()
}

/// The form that asks for the dashboard's address and token. `server` prefills the
/// address; the token is never sent back to the page.
pub fn setup(server: &str, message: Option<&str>) -> String {
    let (server_focus, token_focus) = if server.is_empty() {
        (" autofocus", "")
    } else {
        ("", " autofocus")
    };
    let body = format!(
        "<header>{ICON}<div><h1>Connect to the dashboard</h1>\
         <p>hermes-beaver's web view, on your LAN.</p></div></header>\
         {error}\
         <form method=\"post\" action=\"/__app/setup\">\
         <label for=\"server\">Address</label>\
         <input id=\"server\" name=\"server\" value=\"{server}\" placeholder=\"susel:8788\" \
          required spellcheck=\"false\" autocapitalize=\"off\"{server_focus}>\
         <p class=\"hint\">Where <code>hermes-beaver web</code> listens: <code>[web] listen</code> \
          in its config.</p>\
         <label for=\"token\">Token</label>\
         <input id=\"token\" name=\"token\" type=\"password\" autocomplete=\"off\"{token_focus}>\
         <p class=\"hint\">The contents of the file in <code>[web] token_file</code> on that \
          machine, e.g. <code>sudo cat /etc/secrets/hermes-beaver-web.token</code>. Leave it \
          empty if the web view has no token. It's kept in this computer's app settings.</p>\
         <div class=\"actions\"><button type=\"submit\">Connect</button></div>\
         </form>{KEYS}",
        error = error_box(message),
        server = escape(server),
    );
    page("hermes-beaver: connect", "", &body)
}

/// The dashboard didn't answer (or answered with an error): retries by itself
/// every 10 s.
pub fn unreachable(server: &str, message: &str) -> String {
    let body = format!(
        "<header>{ICON}<div><h1>Can't open the dashboard</h1>\
         <p>{server}</p></div></header>\
         {error}\
         <p class=\"hint\">Trying again every 10 seconds.</p>\
         <div class=\"actions\"><a class=\"button\" href=\"/\">Try now</a>\
         <a class=\"link\" href=\"/__app/setup\">Change the address or token</a></div>{KEYS}",
        server = escape(server),
        error = error_box(Some(message)),
    );
    page(
        "hermes-beaver: not reachable",
        "<meta http-equiv=\"refresh\" content=\"10; url=/\">",
        &body,
    )
}

/// After a successful setup: WebKitGTK doesn't follow a redirect from a custom
/// protocol, so this page moves on to `/` by itself.
pub fn opening() -> String {
    let body = format!(
        "<header>{ICON}<div><h1>Connected</h1><p>Opening the dashboard…</p></div></header>\
         <div class=\"actions\"><a class=\"button\" href=\"/\">Open it</a></div>"
    );
    page(
        "hermes-beaver: connected",
        "<meta http-equiv=\"refresh\" content=\"0; url=/\">",
        &body,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_markup() {
        assert_eq!(
            escape(r#"<a href="x">'&'</a>"#),
            "&lt;a href=&quot;x&quot;&gt;&#39;&amp;&#39;&lt;/a&gt;"
        );
    }

    #[test]
    fn setup_prefills_the_address_only() {
        let html = setup("http://\"susel\":8788", Some("Refused <b>"));
        assert!(html.contains("value=\"http://&quot;susel&quot;:8788\""));
        assert!(html.contains("Refused &lt;b&gt;"));
        assert!(html.contains("name=\"token\" type=\"password\" autocomplete=\"off\" autofocus"));
        assert!(!html.contains("<script"));
        let empty = setup("", None);
        assert!(empty.contains("required spellcheck=\"false\" autocapitalize=\"off\" autofocus"));
        assert!(!empty.contains("class=\"error\""));
    }

    #[test]
    fn unreachable_retries_and_links_to_setup() {
        let html = unreachable("http://susel:8788", "connection refused");
        assert!(html.contains("http-equiv=\"refresh\" content=\"10; url=/\""));
        assert!(html.contains("href=\"/__app/setup\""));
        assert!(html.contains("connection refused"));
    }
}
