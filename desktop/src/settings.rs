//! The app's settings: which dashboard to open, its token, and whether the window
//! was full screen. One JSON file in the app's config directory, readable by the
//! user only (it holds the token).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tauri::Url;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// The web view's base address, normalised by [`normalize_server`], e.g.
    /// `http://susel:8788`.
    pub server: String,
    /// `[web] token_file`'s token on the dashboard machine; empty when it has none.
    pub token: String,
    /// The window's state when it last changed, restored at the next start.
    pub fullscreen: bool,
}

impl Settings {
    /// Whether there is a dashboard to open.
    pub fn configured(&self) -> bool {
        !self.server.is_empty()
    }

    /// Reads `path`; a missing or unreadable file gives empty settings, which shows
    /// the setup page.
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                eprintln!("hermes-beaver-desktop: ignoring {}: {e}", path.display());
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    /// Writes `path` atomically, mode 0600 on Unix.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = sibling(path, ".tmp");
        let json = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        write_private(&tmp, &json)?;
        std::fs::rename(&tmp, path)
    }
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

#[cfg(unix)]
fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(data)?;
    f.sync_all()
}

#[cfg(not(unix))]
fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    // The user's profile directory is private on Windows and macOS already.
    std::fs::write(path, data)
}

/// The web view's base address from what the user typed: `susel:8788`,
/// `192.168.1.10:8788`, `http://susel:8788/`. HTTP unless it says HTTPS; no
/// trailing slash. A path stays, for a reverse proxy that serves it under one.
pub fn normalize_server(input: &str) -> Result<String, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err("Enter the dashboard's address.".into());
    }
    let with_scheme = if input.contains("://") {
        input.to_owned()
    } else {
        format!("http://{input}")
    };
    let url =
        Url::parse(&with_scheme).map_err(|e| format!("\"{input}\" is not an address: {e}."))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!(
            "\"{input}\" must be an http:// or https:// address."
        ));
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err(format!("\"{input}\" has no host name."));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("Put the token in its own field, not in the address.".into());
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(format!(
            "\"{input}\" must be just the address, without ? or #."
        ));
    }
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_are_normalised() {
        for (typed, want) in [
            ("susel:8788", "http://susel:8788"),
            ("  http://susel:8788/ ", "http://susel:8788"),
            ("192.168.87.75:8788", "http://192.168.87.75:8788"),
            ("https://beaver.example.com", "https://beaver.example.com"),
            ("http://proxy.lan/beaver/", "http://proxy.lan/beaver"),
            ("HTTP://Susel.LAN:8788", "http://susel.lan:8788"),
            ("[::1]:8788", "http://[::1]:8788"),
        ] {
            assert_eq!(normalize_server(typed).as_deref(), Ok(want), "{typed}");
        }
        for bad in [
            "",
            "   ",
            "ftp://susel",
            "http://",
            "http://me:secret@susel:8788",
            "susel:8788/?x=1",
            "susel:8788/#top",
            "susel:notaport",
        ] {
            assert!(normalize_server(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn saves_privately_and_loads_back() {
        let dir = std::env::temp_dir().join(format!("hbd-settings-{}", std::process::id()));
        let path = dir.join("nested").join("settings.json");
        assert_eq!(Settings::load(&path), Settings::default());
        assert!(!Settings::load(&path).configured());
        let s = Settings {
            server: "http://susel:8788".into(),
            token: "0123456789abcdef".into(),
            fullscreen: true,
        };
        s.save(&path).unwrap();
        assert_eq!(Settings::load(&path), s);
        assert!(!sibling(&path, ".tmp").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "only the user may read the token");
        }
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(Settings::load(&path), Settings::default());
        std::fs::write(&path, r#"{"server":"http://x:1"}"#).unwrap();
        assert_eq!(
            Settings::load(&path).server,
            "http://x:1",
            "missing keys default"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
