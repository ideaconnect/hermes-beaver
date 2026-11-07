//! Configuration from `~/.config/hermes-beaver/config.toml`.
//!
//! Every key is optional. A missing file means all defaults. Unknown keys are
//! rejected so that typos don't silently fall back to defaults.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use chrono::NaiveTime;
use serde::Deserialize;

use crate::display::Schedule;

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// IANA time zone for the clock, the monitor schedule and `until` times, e.g.
    /// "Europe/Warsaw". Unset: the host's zone.
    pub timezone: Option<String>,
    pub hermes: HermesConfig,
    pub tasks: TasksConfig,
    pub brightness: BrightnessConfig,
    pub metrics: MetricsConfig,
    pub display: DisplayConfig,
    pub ui: UiConfig,
}

/// Daily monitor standby. Times are "HH:MM"; either may be left out.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DisplayConfig {
    pub off_at: Option<String>,
    pub on_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Renderer {
    /// CPU rasteriser: ~40 MB RSS on the target (16 MB of it the frame buffers shared with
    /// the compositor), same CPU as OpenGL at this refresh rate.
    Software,
    /// OpenGL (femtovg): ~110 MB RSS on the target.
    Opengl,
}

/// The ASCII animal in the bottom-right panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Animal {
    Beaver,
    /// A ground squirrel at a laptop, from the art on idct.tech.
    Squirrel,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UiConfig {
    /// `SLINT_BACKEND` in the environment overrides this.
    pub renderer: Renderer,
    pub animal: Animal,
    /// Title of the animal's panel. Unset: the animal's own name.
    pub animal_name: Option<String>,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            renderer: Renderer::Software,
            animal: Animal::Beaver,
            animal_name: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HermesConfig {
    /// Hermes home directory; a leading `~/` is expanded.
    pub home: String,
    /// Cron job ids whose executions feed the health rules. Host-specific.
    pub agent_jobs: Vec<String>,
    pub heartbeat_max_age_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TasksConfig {
    pub stale_after_mins: u64,
    pub finished_rows: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BrightnessMethod {
    /// DDC/CI, else `dim`.
    Auto,
    Ddc,
    /// The dashboard dims itself with an overlay. `gamma` is the name from the X11
    /// days, when this was the RandR gamma ramp.
    #[serde(alias = "gamma")]
    Dim,
    Off,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BrightnessConfig {
    pub method: BrightnessMethod,
    pub idle: u8,
    pub active: u8,
    pub active_secs: u64,
    pub exit_level: u8,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MetricsConfig {
    /// Block device to watch, or "auto" for the device behind `/`.
    pub disk: String,
}

impl Default for HermesConfig {
    fn default() -> Self {
        Self {
            home: "~/.hermes".into(),
            agent_jobs: Vec::new(),
            heartbeat_max_age_secs: 180,
        }
    }
}

impl Default for TasksConfig {
    fn default() -> Self {
        Self {
            stale_after_mins: 30,
            finished_rows: 8,
        }
    }
}

impl Default for BrightnessConfig {
    fn default() -> Self {
        Self {
            method: BrightnessMethod::Auto,
            idle: 20,
            active: 80,
            active_secs: 60,
            exit_level: 80,
        }
    }
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            disk: "auto".into(),
        }
    }
}

impl Config {
    /// Loads `path`, or the default location when `None`. A missing file at the
    /// default location yields defaults; a missing explicit path is an error.
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let (path, explicit) = match path {
            Some(p) => (p.to_path_buf(), true),
            None => match default_path() {
                Some(p) => (p, false),
                None => return Ok(Self::default()),
            },
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !explicit => {
                return Ok(Self::default());
            }
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Self::parse(&text).with_context(|| format!("in {}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Self> {
        let config: Self = toml::from_str(text)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        let b = &self.brightness;
        for (name, value) in [
            ("idle", b.idle),
            ("active", b.active),
            ("exit_level", b.exit_level),
        ] {
            ensure!(
                value <= 100,
                "brightness.{name} must be 0..=100, got {value}"
            );
        }
        ensure!(
            self.tasks.finished_rows > 0,
            "tasks.finished_rows must be > 0"
        );
        let schedule = self.try_schedule()?;
        ensure!(
            schedule.off_at.is_none() || schedule.off_at != schedule.on_at,
            "display.off_at and display.on_at must differ"
        );
        if let Some(tz) = &self.timezone {
            ensure!(
                !tz.is_empty() && !tz.starts_with('/') && !tz.contains(".."),
                "timezone must be an IANA name like \"Europe/Warsaw\", got {tz:?}"
            );
            ensure!(
                Path::new(ZONEINFO).join(tz).is_file(),
                "timezone {tz:?} not found in {ZONEINFO}"
            );
        }
        Ok(())
    }

    /// The monitor's daily schedule (validated at load).
    pub fn schedule(&self) -> Schedule {
        self.try_schedule().unwrap_or_default()
    }

    fn try_schedule(&self) -> Result<Schedule> {
        let parse = |key: &str, v: &Option<String>| -> Result<Option<NaiveTime>> {
            match v.as_deref().map(str::trim) {
                None | Some("") => Ok(None),
                Some(t) => match NaiveTime::parse_from_str(t, "%H:%M") {
                    Ok(t) => Ok(Some(t)),
                    Err(_) => bail!("display.{key} must be a time like \"23:00\", got {t:?}"),
                },
            }
        };
        Ok(Schedule {
            off_at: parse("off_at", &self.display.off_at)?,
            on_at: parse("on_at", &self.display.on_at)?,
        })
    }

    /// Makes `timezone` the process's local zone (chrono reads `TZ`). Call it first
    /// thing in `main`, while the process is still single-threaded.
    pub fn apply_timezone(&self) {
        if let Some(tz) = &self.timezone {
            // SAFETY: called before any thread is spawned, so nothing reads the
            // environment concurrently.
            unsafe { std::env::set_var("TZ", tz) };
        }
    }

    /// The Hermes home directory with `~/` expanded.
    #[allow(dead_code)] // used by the Hermes readers (issue #24)
    pub fn hermes_home(&self) -> PathBuf {
        expand_tilde(&self.hermes.home, dirs::home_dir())
    }
}

const ZONEINFO: &str = "/usr/share/zoneinfo";

fn default_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("hermes-beaver").join("config.toml"))
}

fn expand_tilde(path: &str, home: Option<PathBuf>) -> PathBuf {
    match (path.strip_prefix("~/"), home) {
        (Some(rest), Some(home)) => home.join(rest),
        _ if path == "~" => dirs::home_dir().unwrap_or_else(|| PathBuf::from(path)),
        _ => PathBuf::from(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_gives_defaults() {
        assert_eq!(Config::parse("").unwrap(), Config::default());
    }

    #[test]
    fn missing_explicit_file_is_an_error() {
        let missing = Path::new("/nonexistent/hermes-beaver/config.toml");
        assert!(Config::load(Some(missing)).is_err());
    }

    #[test]
    fn partial_file_is_merged_with_defaults() {
        let c = Config::parse(
            r#"
            [hermes]
            agent_jobs = ["a1", "b2"]

            [brightness]
            idle = 10
            "#,
        )
        .unwrap();
        assert_eq!(c.hermes.agent_jobs, vec!["a1", "b2"]);
        assert_eq!(c.hermes.home, "~/.hermes");
        assert_eq!(c.hermes.heartbeat_max_age_secs, 180);
        assert_eq!(c.brightness.idle, 10);
        assert_eq!(c.brightness.active, 80);
        assert_eq!(c.brightness.method, BrightnessMethod::Auto);
        assert_eq!(c.tasks, TasksConfig::default());
        assert_eq!(c.metrics.disk, "auto");
    }

    #[test]
    fn brightness_method_parses() {
        let c = Config::parse("[brightness]\nmethod = \"dim\"").unwrap();
        assert_eq!(c.brightness.method, BrightnessMethod::Dim);
        let c = Config::parse("[brightness]\nmethod = \"gamma\"").unwrap();
        assert_eq!(
            c.brightness.method,
            BrightnessMethod::Dim,
            "the old name still works"
        );
        assert!(Config::parse("[brightness]\nmethod = \"laser\"").is_err());
    }

    #[test]
    fn renderer_defaults_to_software() {
        assert_eq!(Config::default().ui.renderer, Renderer::Software);
        let c = Config::parse("[ui]\nrenderer = \"opengl\"").unwrap();
        assert_eq!(c.ui.renderer, Renderer::Opengl);
    }

    #[test]
    fn animal_defaults_to_the_beaver() {
        assert_eq!(Config::default().ui.animal, Animal::Beaver);
        assert_eq!(Config::default().ui.animal_name, None);
        let c = Config::parse("[ui]\nanimal = \"squirrel\"\nanimal_name = \"Suseł\"").unwrap();
        assert_eq!(c.ui.animal, Animal::Squirrel);
        assert_eq!(c.ui.animal_name.as_deref(), Some("Suseł"));
        assert!(Config::parse("[ui]\nanimal = \"unicorn\"").is_err());
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let err = Config::parse("[tasks]\nstale_after_minutes = 5").unwrap_err();
        assert!(format!("{err:#}").contains("stale_after_minutes"));
    }

    #[test]
    fn out_of_range_brightness_is_rejected() {
        assert!(Config::parse("[brightness]\nactive = 150").is_err());
    }

    #[test]
    fn display_schedule_parses_and_validates() {
        assert_eq!(Config::default().schedule(), Schedule::default());
        let c = Config::parse("[display]\noff_at = \"23:00\"\non_at = \"09:00\"").unwrap();
        assert_eq!(
            c.schedule(),
            Schedule {
                off_at: NaiveTime::from_hms_opt(23, 0, 0),
                on_at: NaiveTime::from_hms_opt(9, 0, 0),
            }
        );
        let only_off = Config::parse("[display]\noff_at = \"22:30\"").unwrap();
        assert_eq!(only_off.schedule().on_at, None);
        let err = |text: &str| format!("{:#}", Config::parse(text).unwrap_err());
        assert!(err("[display]\noff_at = \"11pm\"").contains("like \"23:00\""));
        assert!(err("[display]\noff_at = \"09:00\"\non_at = \"09:00\"").contains("must differ"));
    }

    #[test]
    fn timezone_must_exist() {
        if !Path::new(ZONEINFO).join("UTC").is_file() {
            return; // no tz database on this machine
        }
        assert!(Config::parse("timezone = \"UTC\"").is_ok());
        let err = |text: &str| format!("{:#}", Config::parse(text).unwrap_err());
        assert!(err("timezone = \"Mars/Olympus\"").contains("not found"));
        assert!(err("timezone = \"../etc/passwd\"").contains("IANA name"));
    }

    #[test]
    fn tilde_is_expanded() {
        let home = Some(PathBuf::from("/home/u"));
        assert_eq!(
            expand_tilde("~/.hermes", home.clone()),
            PathBuf::from("/home/u/.hermes")
        );
        assert_eq!(expand_tilde("/abs", home), PathBuf::from("/abs"));
    }
}
