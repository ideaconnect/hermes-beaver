//! A small client for sway's IPC socket (`$SWAYSOCK`), the protocol `swaymsg`
//! speaks: a 14-byte header ("i3-ipc", then the payload length and the message
//! type as u32 in native byte order) followed by a JSON payload. The dashboard
//! only needs two messages: run a command, and list the outputs.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::Value;

const MAGIC: &[u8; 6] = b"i3-ipc";
const RUN_COMMAND: u32 = 0;
const GET_OUTPUTS: u32 = 3;
const TIMEOUT: Duration = Duration::from_secs(5);

pub struct Sway {
    socket: PathBuf,
}

impl Sway {
    /// The session's socket. The kiosk's sway config exports `SWAYSOCK` to the
    /// user manager before it starts the dashboard.
    pub fn from_env() -> Result<Self> {
        let socket = std::env::var_os("SWAYSOCK")
            .context("SWAYSOCK is not set (the dashboard isn't running under sway)")?;
        Ok(Self {
            socket: socket.into(),
        })
    }

    /// Runs a sway command, as `swaymsg <command>` would.
    pub fn run(&self, command: &str) -> Result<()> {
        let reply = self.request(RUN_COMMAND, command)?;
        let results = reply.as_array().context("unexpected reply to a command")?;
        for r in results {
            if r["success"] != true {
                let why = r["error"].as_str().unwrap_or("no reason given");
                bail!("sway refused `{command}`: {why}");
            }
        }
        Ok(())
    }

    /// Each output sway knows of (connected, on or off), and whether it's powered.
    pub fn output_power(&self) -> Result<Vec<(String, bool)>> {
        let reply = self.request(GET_OUTPUTS, "")?;
        let outputs = reply
            .as_array()
            .context("unexpected reply to GET_OUTPUTS")?;
        Ok(outputs
            .iter()
            .map(|o| {
                let name = o["name"].as_str().unwrap_or("?").to_owned();
                (name, o["power"] == true)
            })
            .collect())
    }

    /// One connection per request: they're a minute apart, and a restarted sway
    /// needs nothing re-established.
    fn request(&self, kind: u32, payload: &str) -> Result<Value> {
        let mut s = UnixStream::connect(&self.socket)
            .with_context(|| format!("connecting to sway at {}", self.socket.display()))?;
        s.set_read_timeout(Some(TIMEOUT))?;
        s.set_write_timeout(Some(TIMEOUT))?;
        let len = u32::try_from(payload.len()).context("sway command too long")?;
        let mut msg = Vec::with_capacity(14 + payload.len());
        msg.extend_from_slice(MAGIC);
        msg.extend_from_slice(&len.to_ne_bytes());
        msg.extend_from_slice(&kind.to_ne_bytes());
        msg.extend_from_slice(payload.as_bytes());
        s.write_all(&msg).context("writing to sway")?;

        let mut header = [0u8; 14];
        s.read_exact(&mut header).context("reading sway's reply")?;
        let field = |at: usize| {
            u32::from_ne_bytes([header[at], header[at + 1], header[at + 2], header[at + 3]])
        };
        if &header[..6] != MAGIC {
            bail!("not a sway IPC reply");
        }
        if field(10) != kind {
            bail!("sway answered message {} with {}", kind, field(10));
        }
        let mut body = vec![0; field(6) as usize];
        s.read_exact(&mut body).context("reading sway's reply")?;
        serde_json::from_slice(&body).context("parsing sway's reply")
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixListener;
    use std::thread;

    use super::*;

    /// A fake sway that answers each `(type, payload)` it expects with a reply.
    fn fake_sway(
        name: &str,
        exchanges: Vec<(u32, &'static str, &'static str)>,
    ) -> (Sway, thread::JoinHandle<()>) {
        let socket =
            std::env::temp_dir().join(format!("hb-sway-{}-{name}.sock", std::process::id()));
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            for (kind, want, reply) in exchanges {
                let (mut c, _) = listener.accept().unwrap();
                let mut header = [0u8; 14];
                c.read_exact(&mut header).unwrap();
                assert_eq!(&header[..6], MAGIC);
                let len = u32::from_ne_bytes(header[6..10].try_into().unwrap());
                assert_eq!(u32::from_ne_bytes(header[10..14].try_into().unwrap()), kind);
                let mut payload = vec![0; len as usize];
                c.read_exact(&mut payload).unwrap();
                assert_eq!(String::from_utf8(payload).unwrap(), want);
                let mut msg = MAGIC.to_vec();
                msg.extend_from_slice(&(reply.len() as u32).to_ne_bytes());
                msg.extend_from_slice(&kind.to_ne_bytes());
                msg.extend_from_slice(reply.as_bytes());
                c.write_all(&msg).unwrap();
            }
        });
        (Sway { socket }, server)
    }

    #[test]
    fn runs_commands_and_reports_refusals() {
        let (sway, server) = fake_sway(
            "run",
            vec![
                (RUN_COMMAND, "output * power off", r#"[{"success": true}]"#),
                (
                    RUN_COMMAND,
                    "output * power sideways",
                    r#"[{"success": false, "parse_error": true, "error": "Invalid power state"}]"#,
                ),
            ],
        );
        sway.run("output * power off").unwrap();
        let err = sway.run("output * power sideways").unwrap_err().to_string();
        assert!(err.contains("Invalid power state"), "{err}");
        server.join().unwrap();
        let _ = std::fs::remove_file(&sway.socket);
    }

    #[test]
    fn lists_outputs_with_their_power() {
        let (sway, server) = fake_sway(
            "outputs",
            vec![(
                GET_OUTPUTS,
                "",
                r#"[{"name": "HDMI-A-1", "active": true, "power": false, "modes": []},
                    {"name": "DP-1", "power": true}]"#,
            )],
        );
        assert_eq!(
            sway.output_power().unwrap(),
            vec![("HDMI-A-1".to_owned(), false), ("DP-1".to_owned(), true)]
        );
        server.join().unwrap();
        let _ = std::fs::remove_file(&sway.socket);
    }

    #[test]
    fn a_missing_socket_is_an_error_not_a_hang() {
        let sway = Sway {
            socket: std::env::temp_dir().join("hb-sway-nonexistent.sock"),
        };
        assert!(sway.run("output * power on").is_err());
    }
}
