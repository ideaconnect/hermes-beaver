//! Screen brightness: dim when idle, brighten for a while when something happens.
//!
//! The monitor backlight is driven over DDC/CI with `ddcutil` (what Windows and
//! KDE do for external monitors). Without DDC the dashboard dims itself with a
//! black overlay, which on the kiosk is the whole picture. Commands run on a
//! background thread, because one DDC write takes 60-400 ms.

use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::config::{BrightnessConfig, BrightnessMethod};

pub trait Backend: Send {
    fn name(&self) -> String;
    fn set(&mut self, level: u8) -> Result<()>;
    /// Called once on exit.
    fn exit(&mut self, exit_level: u8) -> Result<()> {
        self.set(exit_level)
    }
}

/// Monitor backlight over DDC/CI.
pub struct Ddc {
    bus: u32,
}

impl Ddc {
    /// Finds the first display with DDC/CI and checks that VCP 0x10 reads.
    pub fn probe() -> Result<Self> {
        let out = Command::new("ddcutil")
            .args(["detect", "--brief"])
            .stderr(Stdio::null())
            .output()
            .context("running ddcutil (is it installed?)")?;
        let Some(bus) = parse_detect_bus(&String::from_utf8_lossy(&out.stdout)) else {
            bail!(
                "ddcutil found no display with DDC/CI (is DDC/CI enabled in the monitor's menu?)"
            );
        };
        let ok = Command::new("ddcutil")
            .args(["--bus", &bus.to_string(), "getvcp", "10"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .context("running ddcutil getvcp")?
            .success();
        if !ok {
            bail!("display on i2c bus {bus} doesn't answer brightness (VCP 0x10) reads");
        }
        Ok(Self { bus })
    }
}

impl Backend for Ddc {
    fn name(&self) -> String {
        format!("DDC/CI on i2c-{}", self.bus)
    }

    fn set(&mut self, level: u8) -> Result<()> {
        // --skip-ddc-checks: the startup probe already verified DDC, and it cuts a
        // write from ~410 ms to ~60 ms. ddcutil prints traces on stdout: ignore it.
        let out = Command::new("ddcutil")
            .args([
                "--bus",
                &self.bus.to_string(),
                "--noverify",
                "--skip-ddc-checks",
            ])
            .args(["setvcp", "10", &level.min(100).to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()?;
        if !out.status.success() {
            bail!(
                "ddcutil setvcp failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }
}

/// Dims the picture with a black overlay drawn by the dashboard itself, for
/// monitors without DDC/CI. Needs nothing from the compositor.
pub struct Dim {
    set_overlay: Box<dyn Fn(f32) + Send>,
}

impl Dim {
    /// `set_overlay` gets the overlay's opacity (0 = clear) and is called on the
    /// brightness thread.
    pub fn new(set_overlay: impl Fn(f32) + Send + 'static) -> Self {
        Self {
            set_overlay: Box::new(set_overlay),
        }
    }
}

impl Backend for Dim {
    fn name(&self) -> String {
        "an overlay (no DDC/CI)".into()
    }

    fn set(&mut self, level: u8) -> Result<()> {
        (self.set_overlay)(overlay_opacity(level));
        Ok(())
    }

    /// The window is closing: nothing to restore.
    fn exit(&mut self, _exit_level: u8) -> Result<()> {
        Ok(())
    }
}

/// The overlay for a brightness level: clear at 100, never darker than level 5.
pub fn overlay_opacity(level: u8) -> f32 {
    1.0 - f32::from(level.clamp(5, 100)) / 100.0
}

/// Parses the first `I2C bus:  /dev/i2c-N` from `ddcutil detect --brief`.
pub fn parse_detect_bus(text: &str) -> Option<u32> {
    text.lines()
        .filter(|l| !l.trim_start().starts_with("Invalid display"))
        .find_map(|l| l.trim().strip_prefix("I2C bus:"))
        .and_then(|rest| rest.trim().strip_prefix("/dev/i2c-"))
        .and_then(|n| n.trim().parse().ok())
}

/// Picks a backend for the configured method, logging why others were skipped.
pub fn select(method: BrightnessMethod, dim: Dim) -> Option<Box<dyn Backend>> {
    let ddc = || match Ddc::probe() {
        Ok(d) => Some(Box::new(d) as Box<dyn Backend>),
        Err(e) => {
            eprintln!("hermes-beaver: DDC/CI unavailable: {e:#}");
            None
        }
    };
    match method {
        BrightnessMethod::Off => None,
        BrightnessMethod::Ddc => ddc(),
        BrightnessMethod::Dim => Some(Box::new(dim)),
        BrightnessMethod::Auto => ddc().or_else(|| Some(Box::new(dim))),
    }
}

enum Msg {
    Wake,
    Exit,
}

#[derive(Debug, Clone, Copy)]
pub struct Levels {
    pub idle: u8,
    pub active: u8,
    pub exit: u8,
    pub active_for: Duration,
    pub step_pause: Duration,
}

impl From<&BrightnessConfig> for Levels {
    fn from(c: &BrightnessConfig) -> Self {
        Self {
            idle: c.idle,
            active: c.active,
            exit: c.exit_level,
            active_for: Duration::from_secs(c.active_secs),
            step_pause: Duration::from_millis(300),
        }
    }
}

/// Runs the backend on its own thread. Dropping it restores the exit level.
pub struct Controller {
    tx: Sender<Msg>,
    handle: Option<JoinHandle<()>>,
}

impl Controller {
    /// `backend` is created on the worker thread, so slow probing never blocks the UI.
    pub fn start(
        levels: Levels,
        backend: impl FnOnce() -> Option<Box<dyn Backend>> + Send + 'static,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        let handle = std::thread::Builder::new()
            .name("brightness".into())
            .spawn(move || {
                let Some(mut backend) = backend() else {
                    // Drain messages until exit so senders never fail loudly.
                    while let Ok(Msg::Wake) = rx.recv() {}
                    return;
                };
                eprintln!("hermes-beaver: brightness via {}", backend.name());
                let mut level = levels.idle;
                apply(&mut *backend, level);
                let mut active_until: Option<Instant> = None;
                loop {
                    let msg = match active_until {
                        Some(t) => rx.recv_timeout(t.saturating_duration_since(Instant::now())),
                        None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
                    };
                    match msg {
                        Ok(Msg::Wake) => {
                            if level != levels.active {
                                fade(&mut *backend, level, levels.active, levels.step_pause);
                                level = levels.active;
                            }
                            active_until = Some(Instant::now() + levels.active_for);
                        }
                        Err(RecvTimeoutError::Timeout) => {
                            fade(&mut *backend, level, levels.idle, levels.step_pause);
                            level = levels.idle;
                            active_until = None;
                        }
                        Ok(Msg::Exit) | Err(RecvTimeoutError::Disconnected) => {
                            if let Err(e) = backend.exit(levels.exit) {
                                eprintln!("hermes-beaver: restoring brightness failed: {e:#}");
                            }
                            return;
                        }
                    }
                }
            })
            .expect("spawning brightness thread");
        Self {
            tx,
            handle: Some(handle),
        }
    }

    pub fn wake(&self) {
        let _ = self.tx.send(Msg::Wake);
    }
}

impl Drop for Controller {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Exit);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn apply(backend: &mut dyn Backend, level: u8) {
    if let Err(e) = backend.set(level) {
        eprintln!("hermes-beaver: setting brightness {level} failed: {e:#}");
    }
}

/// Three steps from `from` to `to`.
fn fade(backend: &mut dyn Backend, from: u8, to: u8, pause: Duration) {
    for k in 1..=3i32 {
        let v = from as i32 + (to as i32 - from as i32) * k / 3;
        apply(backend, v as u8);
        if k < 3 {
            std::thread::sleep(pause);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    struct Fake(Arc<Mutex<Vec<u8>>>);

    impl Backend for Fake {
        fn name(&self) -> String {
            "fake".into()
        }
        fn set(&mut self, level: u8) -> Result<()> {
            self.0.lock().unwrap().push(level);
            Ok(())
        }
    }

    #[test]
    fn parses_ddcutil_detect() {
        let text = "Display 1\n   I2C bus:  /dev/i2c-6\n   DRM connector:  card0-HDMI-A-1\n   Monitor:  DEL:DELL G3223Q:XYZ\n";
        assert_eq!(parse_detect_bus(text), Some(6));
        assert_eq!(parse_detect_bus("No displays found.\n"), None);
    }

    #[test]
    fn overlay_darkens_as_the_level_drops() {
        assert_eq!(overlay_opacity(100), 0.0);
        assert!((overlay_opacity(20) - 0.8).abs() < 1e-6);
        assert_eq!(overlay_opacity(0), overlay_opacity(5));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s2 = seen.clone();
        let mut dim = Dim::new(move |o| s2.lock().unwrap().push(o));
        dim.set(100).unwrap();
        dim.exit(80).unwrap();
        assert_eq!(
            *seen.lock().unwrap(),
            vec![0.0],
            "exit leaves the closing window alone"
        );
    }

    #[test]
    fn wake_fades_up_then_back_down_and_exit_restores() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let levels = Levels {
            idle: 20,
            active: 80,
            exit: 90,
            active_for: Duration::from_millis(150),
            step_pause: Duration::from_millis(1),
        };
        let l2 = log.clone();
        let c = Controller::start(levels, move || Some(Box::new(Fake(l2)) as Box<dyn Backend>));
        std::thread::sleep(Duration::from_millis(30));
        c.wake();
        std::thread::sleep(Duration::from_millis(60));
        c.wake(); // extends the active period without another fade
        std::thread::sleep(Duration::from_millis(400));
        drop(c);
        assert_eq!(*log.lock().unwrap(), vec![20, 40, 60, 80, 60, 40, 20, 90]);
    }

    #[test]
    fn no_backend_is_harmless() {
        let c = Controller::start(
            Levels {
                idle: 20,
                active: 80,
                exit: 80,
                active_for: Duration::from_secs(1),
                step_pause: Duration::ZERO,
            },
            || None,
        );
        c.wake();
        drop(c);
    }
}
