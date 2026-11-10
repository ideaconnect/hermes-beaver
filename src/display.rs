//! Monitor power: standby at night, while the owner is out, or on vacation.
//!
//! Whether the monitor should be off comes from two inputs:
//! - the owner's last request (`display_off` / `display_on` / `display_vacation`),
//!   stored with the time it was made;
//! - an optional daily schedule from the config (`[display] off_at` / `on_at`).
//!
//! The most recent of the two wins: a scheduled switch overrides an older
//! request, and a request holds until the next scheduled switch. Two exceptions:
//! an `off` with `until` holds until that time even across a scheduled switch on,
//! and vacation holds until `display_on`.
//!
//! The monitor is switched through the Wayland compositor with `wlopm` (the
//! wlr-output-power-management protocol, which sway implements): `wlopm --off '*'`
//! puts every output into standby and `wlopm --on '*'` wakes them. The compositor
//! never blanks the dashboard on its own: the kiosk session runs no idle daemon.
//!
//! A switch counts as done only when `wlopm -j` shows it: at least one output (none
//! is fine for standby) and every output in the wanted mode. Until then every sync
//! (the 10 s tick) tries again; afterwards the outputs are checked every
//! [`RECHECK_SECS`], and an output in the wrong mode is switched back. The exit code
//! alone can't be trusted, for two reasons:
//! - `wlopm --on '*'` switches the outputs present and succeeds when there are none,
//!   for example while the monitor is briefly off the cable;
//! - sway keeps the requested power mode by output name, so after `--off` an output
//!   that reconnects under that name comes up in standby until an `on` reaches it.
//!
//! Together they can leave the monitor dark after the morning switch-on while the
//! dashboard believes it is on (issue #33).
//!
//! A new request from the owner applies its state again even when the wanted state
//! stays the same: `display_on` wakes a monitor that stayed dark.
//!
//! A key press or pointer move wakes a monitor in standby without changing the
//! requested state. After [`INPUT_WAKE_SECS`] without input it goes back into
//! standby (the X DPMS timeout the dashboard had before Wayland). The checks leave a
//! monitor woken this way on.

use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Days, Local, NaiveTime, TimeZone};
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    On,
    Off,
    /// Off until `display_on`; the schedule doesn't turn it on.
    Vacation,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::On => "on",
            Mode::Off => "off",
            Mode::Vacation => "vacation",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "on" => Some(Mode::On),
            "off" => Some(Mode::Off),
            "vacation" => Some(Mode::Vacation),
            _ => None,
        }
    }
}

/// The owner's last request. The default (never asked) is "on since forever",
/// which leaves the schedule in charge.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Request {
    pub mode: Mode,
    /// When it was made (Unix seconds).
    pub since: i64,
    /// For `Off`: when it ends by itself.
    pub until: Option<i64>,
}

/// Daily switch times in the host's local time zone (see `timezone` in the config).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Schedule {
    pub off_at: Option<NaiveTime>,
    pub on_at: Option<NaiveTime>,
}

impl Schedule {
    /// Scheduled switches `(time, off)` from yesterday to the day after tomorrow, sorted.
    fn switches_around(&self, now: i64) -> Vec<(i64, bool)> {
        let Some(today) = Local.timestamp_opt(now, 0).single().map(|t| t.date_naive()) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for day in [
            today.checked_sub_days(Days::new(1)),
            Some(today),
            today.checked_add_days(Days::new(1)),
            today.checked_add_days(Days::new(2)),
        ]
        .into_iter()
        .flatten()
        {
            for (at, off) in [(self.off_at, true), (self.on_at, false)] {
                // A time skipped by a DST change has no switch that day.
                if let Some(t) =
                    at.and_then(|at| day.and_time(at).and_local_timezone(Local).earliest())
                {
                    out.push((t.timestamp(), off));
                }
            }
        }
        out.sort_unstable();
        out
    }

    /// The latest scheduled switch at or before `now`.
    fn last(&self, now: i64) -> Option<(i64, bool)> {
        self.switches_around(now)
            .into_iter()
            .take_while(|&(t, _)| t <= now)
            .last()
    }
}

/// Whether the monitor should be in standby at `now`.
pub fn wants_off(req: &Request, schedule: &Schedule, now: i64) -> bool {
    let last = schedule.last(now);
    // The schedule's word, if it switched after `t`.
    let switched_after = |t: i64| last.filter(|&(at, _)| at > t).map(|(_, off)| off);
    match req.mode {
        Mode::Vacation => true,
        Mode::Off => match req.until {
            Some(until) if now < until => true,
            // After `until`, back to what the schedule says (on without one).
            Some(_) => last.is_some_and(|(_, off)| off),
            None => switched_after(req.since).unwrap_or(true),
        },
        Mode::On => switched_after(req.since).unwrap_or(false),
    }
}

/// When `wants_off` next flips: within two days of now, or of an `until`. `None`
/// means it stays as it is until the owner asks otherwise.
pub fn next_change(req: &Request, schedule: &Schedule, now: i64) -> Option<i64> {
    let current = wants_off(req, schedule, now);
    // An `until` at night hands over to the schedule, which switches on later.
    let around_until = req
        .until
        .map(|u| schedule.switches_around(u))
        .unwrap_or_default();
    let mut points: Vec<i64> = schedule
        .switches_around(now)
        .into_iter()
        .chain(around_until)
        .map(|(t, _)| t)
        .chain(req.until)
        .filter(|&t| t > now)
        .collect();
    points.sort_unstable();
    // `wants_off` only changes at these points, so checking each one is enough.
    points
        .into_iter()
        .find(|&t| wants_off(req, schedule, t) != current)
}

/// One line for people from a display tool's result, e.g. "Screen off until 09:00."
pub fn describe(reply: &Value, now: DateTime<Local>) -> String {
    if reply["vacation"] == true {
        return "Vacation mode: the screen stays off until you turn it on.".into();
    }
    let state = reply["display"].as_str().unwrap_or("?");
    let until = reply["until"]
        .as_str()
        .and_then(|u| DateTime::parse_from_rfc3339(u).ok())
        .map(|t| t.with_timezone(&Local));
    let Some(t) = until else {
        return format!("Screen {state}.");
    };
    let today = now.date_naive();
    let when = if t.date_naive() == today {
        t.format("%H:%M").to_string()
    } else if today.succ_opt() == Some(t.date_naive()) {
        t.format("tomorrow %H:%M").to_string()
    } else {
        t.format("%a %-d %b %H:%M").to_string()
    };
    format!("Screen {state} until {when}.")
}

/// One output as the compositor reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub name: String,
    pub on: bool,
}

pub trait Power {
    /// Switches the output called `output`, or every output present with `"*"`.
    fn set_on(&mut self, output: &str, on: bool) -> Result<()>;
    /// The outputs the compositor has now, and whether each is on.
    fn outputs(&mut self) -> Result<Vec<Output>>;
}

/// Output power through the compositor, with `wlopm` (package wlopm).
pub struct OutputPower;

impl Power for OutputPower {
    fn set_on(&mut self, output: &str, on: bool) -> Result<()> {
        let flag = if on { "--on" } else { "--off" };
        let out = wlopm(&[flag, output])?;
        let stderr = String::from_utf8_lossy(&out.stderr);
        // A refused switch or an unknown output name is an ERROR line with exit code 0.
        if !out.status.success() || stderr.lines().any(|l| l.starts_with("ERROR")) {
            bail!("wlopm {flag} {output} failed: {}", stderr.trim());
        }
        Ok(())
    }

    fn outputs(&mut self) -> Result<Vec<Output>> {
        let out = wlopm(&["-j"])?;
        if !out.status.success() {
            bail!(
                "wlopm -j failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        parse_outputs(&out.stdout)
    }
}

fn wlopm(args: &[&str]) -> Result<std::process::Output> {
    Command::new("wlopm")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .context("running wlopm (is it installed?)")
}

/// Reads `wlopm -j`: `[{"output": "HDMI-A-1", "power-mode": "on"}, ...]`.
fn parse_outputs(json: &[u8]) -> Result<Vec<Output>> {
    #[derive(Deserialize)]
    struct Entry {
        output: String,
        #[serde(rename = "power-mode")]
        mode: String,
    }
    let entries: Vec<Entry> = serde_json::from_slice(json).context("reading wlopm -j")?;
    Ok(entries
        .into_iter()
        .map(|e| Output {
            on: e.mode == "on",
            name: e.output,
        })
        .collect())
}

/// How long a monitor woken by input stays on without further input while it
/// should be in standby.
pub const INPUT_WAKE_SECS: i64 = 600;

/// How often the outputs are checked while they are in the wanted mode. Until they
/// are, every sync checks and switches again.
pub const RECHECK_SECS: i64 = 60;

/// What a sync did to the monitor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    None,
    TurnedOff,
    TurnedOn,
}

/// Applies the wanted state and keeps the outputs in it. Dropping it turns the
/// monitor back on, so stopping the dashboard never leaves a dark screen.
pub struct Screen {
    power: Box<dyn Power>,
    log: Box<dyn FnMut(String)>,
    /// The wanted state, `Some(true)` = off. `None` before the first sync.
    off: Option<bool>,
    /// When the owner made the request applied last (`Request::since`).
    asked: Option<i64>,
    /// Woken by input while it should be off: the time of the last input.
    woken: Option<i64>,
    /// The outputs were in the wanted mode at the last check.
    settled: bool,
    /// When the outputs were last checked.
    checked: Option<i64>,
    /// The output names the last successful check read; `None` before one.
    seen: Option<Vec<String>>,
    /// The problem logged last, so one that lasts is logged once.
    problem: Option<String>,
}

impl Screen {
    /// Logs to stderr, that is the journal.
    pub fn new(power: Box<dyn Power>) -> Self {
        Self::with_log(power, Box::new(|line| eprintln!("hermes-beaver: {line}")))
    }

    pub fn with_log(power: Box<dyn Power>, log: Box<dyn FnMut(String)>) -> Self {
        Self {
            power,
            log,
            off: None,
            asked: None,
            woken: None,
            settled: false,
            checked: None,
            seen: None,
            problem: None,
        }
    }

    /// In standby now: switched off and not woken by input since.
    pub fn is_off(&self) -> bool {
        self.off == Some(true) && self.woken.is_none()
    }

    /// A key press or pointer move at `now`. Wakes a monitor in standby; the requested
    /// state doesn't change. Returns whether this input woke it.
    pub fn input(&mut self, now: i64) -> bool {
        if self.off != Some(true) {
            return false;
        }
        let woke = self.woken.is_none();
        self.woken = Some(now);
        if woke {
            self.switch_all(now, Some("monitor on (input)"));
        }
        woke
    }

    /// Applies `want_off` when it changes, when the owner asked again (`asked`, the
    /// time of their latest request, moved) and at the first call: after a restart the
    /// monitor may still be in standby from the previous run. In between it checks the
    /// outputs every [`RECHECK_SECS`], or at every call until they follow, and switches
    /// those in the wrong mode. A monitor woken by input goes back into standby after
    /// [`INPUT_WAKE_SECS`] without input.
    pub fn sync(&mut self, want_off: bool, asked: i64, now: i64) -> Change {
        let first = self.off.is_none();
        let again = self.asked.is_some_and(|a| a != asked);
        self.asked = Some(asked);
        if first || again || self.off != Some(want_off) {
            self.off = Some(want_off);
            self.woken = None;
            return match (want_off, first) {
                (true, _) => {
                    self.switch_all(now, Some("monitor off (standby)"));
                    Change::TurnedOff
                }
                // Startup: on, as it was; only a problem is worth a line.
                (false, true) => {
                    self.switch_all(now, None);
                    Change::None
                }
                (false, false) => {
                    self.switch_all(now, Some("monitor on"));
                    Change::TurnedOn
                }
            };
        }
        if self.woken.is_some_and(|last| now - last >= INPUT_WAKE_SECS) {
            self.woken = None;
            self.switch_all(now, Some("monitor off again (no input)"));
            return Change::TurnedOff;
        }
        if self.settled && self.checked.is_some_and(|t| now - t < RECHECK_SECS) {
            return Change::None;
        }
        self.reconcile(now)
    }

    /// Switches every output to the wanted mode and checks that they follow.
    fn switch_all(&mut self, now: i64, done: Option<&str>) {
        let on = !self.is_off();
        let result = self
            .power
            .set_on("*", on)
            .and_then(|()| self.power.outputs());
        self.settle(result, now, done);
    }

    /// Checks the outputs and switches those in the wrong mode. Reports a change when
    /// they follow again after a switch or a problem.
    fn reconcile(&mut self, now: i64) -> Change {
        let on = !self.is_off();
        let outputs = match self.power.outputs() {
            Ok(outputs) => outputs,
            Err(e) => {
                self.settle(Err(e), now, None);
                return Change::None;
            }
        };
        let wrong: Vec<String> = outputs
            .iter()
            .filter(|o| o.on != on)
            .map(|o| o.name.clone())
            .collect();
        if wrong.is_empty() && self.problem.is_none() {
            self.settle(Ok(outputs), now, None);
            return Change::None;
        }
        // For the log line: outputs missing at the last check came back since.
        let notes: Vec<String> = outputs
            .iter()
            .filter_map(|o| {
                let back = self
                    .seen
                    .as_ref()
                    .is_some_and(|seen| !seen.contains(&o.name));
                Some(match (on, o.on == on, back) {
                    (true, _, true) => format!("{} came back", o.name),
                    (true, false, false) => format!("{} was off", o.name),
                    (false, false, true) => format!("{} came back on", o.name),
                    (false, false, false) => format!("{} was on", o.name),
                    _ => return None,
                })
            })
            .collect();
        let detail = match (notes.is_empty(), outputs.is_empty()) {
            (false, _) => notes.join(", "),
            (true, false) => outputs
                .iter()
                .map(|o| o.name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            (true, true) => "no outputs".to_owned(),
        };
        let done = format!("monitor {} ({detail})", if on { "on" } else { "off" });
        let result = if wrong.is_empty() {
            Ok(outputs)
        } else {
            wrong
                .iter()
                .try_for_each(|name| self.power.set_on(name, on))
                .and_then(|()| self.power.outputs())
        };
        match (self.settle(result, now, Some(&done)), on) {
            (false, _) => Change::None,
            (true, true) => Change::TurnedOn,
            (true, false) => Change::TurnedOff,
        }
    }

    /// Records a check. It settles when there is an output (none is fine for standby)
    /// and every output is in the wanted mode; then `done` is logged. A problem is
    /// logged only when it differs from the one logged last. Returns whether it settled.
    fn settle(&mut self, result: Result<Vec<Output>>, now: i64, done: Option<&str>) -> bool {
        let on = !self.is_off();
        let state = if on { "on" } else { "off" };
        self.checked = Some(now);
        let problem = match result {
            Err(e) => Some(format!("monitor {state}: {e:#}, retrying")),
            Ok(outputs) => {
                let wrong: Vec<&str> = outputs
                    .iter()
                    .filter(|o| o.on != on)
                    .map(|o| o.name.as_str())
                    .collect();
                let problem = if !wrong.is_empty() {
                    let other = if on { "off" } else { "on" };
                    Some(format!(
                        "monitor {state}: {} still {other}, retrying",
                        wrong.join(", ")
                    ))
                } else if on && outputs.is_empty() {
                    Some("monitor on: no outputs yet, retrying".to_owned())
                } else {
                    None
                };
                self.seen = Some(outputs.into_iter().map(|o| o.name).collect());
                problem
            }
        };
        self.settled = problem.is_none();
        match problem {
            Some(p) if self.problem.as_ref() != Some(&p) => {
                (self.log)(p.clone());
                self.problem = Some(p);
            }
            Some(_) => {}
            None => {
                self.problem = None;
                if let Some(line) = done {
                    (self.log)(line.to_owned());
                }
            }
        }
        self.settled
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        if self.is_off()
            && let Err(e) = self.power.set_on("*", true)
        {
            (self.log)(format!("turning the monitor back on failed: {e:#}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    const HDMI: &str = "HDMI-A-1";

    /// A compositor whose outputs can come and go.
    #[derive(Default)]
    struct Sim {
        /// The outputs present, and whether each is on.
        outputs: Vec<(&'static str, bool)>,
        /// Every switch asked for, e.g. "off *" or "on HDMI-A-1".
        switches: Vec<String>,
        reads: usize,
        /// Switching fails with this error.
        broken: Option<&'static str>,
        /// Reading the outputs fails.
        blind: bool,
    }

    struct Fake(Arc<Mutex<Sim>>);

    impl Power for Fake {
        fn set_on(&mut self, output: &str, on: bool) -> Result<()> {
            let mut s = self.0.lock().unwrap();
            s.switches
                .push(format!("{} {output}", if on { "on" } else { "off" }));
            if let Some(e) = s.broken {
                bail!(e);
            }
            for o in s.outputs.iter_mut() {
                if output == "*" || o.0 == output {
                    o.1 = on;
                }
            }
            Ok(())
        }

        fn outputs(&mut self) -> Result<Vec<Output>> {
            let mut s = self.0.lock().unwrap();
            if s.blind {
                bail!("no Wayland display");
            }
            s.reads += 1;
            Ok(s.outputs
                .iter()
                .map(|&(name, on)| Output {
                    name: name.into(),
                    on,
                })
                .collect())
        }
    }

    /// The fake compositor and the log lines, seen from the test.
    struct Rig {
        sim: Arc<Mutex<Sim>>,
        log: Arc<Mutex<Vec<String>>>,
    }

    impl Rig {
        fn set(&self, outputs: &[(&'static str, bool)]) {
            self.sim.lock().unwrap().outputs = outputs.to_vec();
        }

        fn outputs(&self) -> Vec<(&'static str, bool)> {
            self.sim.lock().unwrap().outputs.clone()
        }

        fn switches(&self) -> Vec<String> {
            self.sim.lock().unwrap().switches.clone()
        }

        fn reads(&self) -> usize {
            self.sim.lock().unwrap().reads
        }

        fn break_with(&self, error: Option<&'static str>) {
            self.sim.lock().unwrap().broken = error;
        }

        fn blind(&self, blind: bool) {
            self.sim.lock().unwrap().blind = blind;
        }

        /// The lines logged since the last call.
        fn take_log(&self) -> Vec<String> {
            std::mem::take(&mut *self.log.lock().unwrap())
        }
    }

    /// A screen with one output, on.
    fn screen() -> (Screen, Rig) {
        let sim = Arc::new(Mutex::new(Sim {
            outputs: vec![(HDMI, true)],
            ..Sim::default()
        }));
        let log = Arc::new(Mutex::new(Vec::new()));
        let sink = log.clone();
        let s = Screen::with_log(
            Box::new(Fake(sim.clone())),
            Box::new(move |line| sink.lock().unwrap().push(line)),
        );
        (s, Rig { sim, log })
    }

    /// 2030-01-`day` `hh`:`mm` in the local time zone (January: no DST change nearby).
    fn at(day: u32, hh: u32, mm: u32) -> i64 {
        chrono::NaiveDate::from_ymd_opt(2030, 1, day)
            .unwrap()
            .and_hms_opt(hh, mm, 0)
            .unwrap()
            .and_local_timezone(Local)
            .earliest()
            .unwrap()
            .timestamp()
    }

    fn nightly() -> Schedule {
        Schedule {
            off_at: NaiveTime::from_hms_opt(23, 0, 0),
            on_at: NaiveTime::from_hms_opt(9, 0, 0),
        }
    }

    fn req(mode: Mode, since: i64, until: Option<i64>) -> Request {
        Request { mode, since, until }
    }

    #[test]
    fn schedule_alone_switches_at_the_set_times() {
        let (r, s) = (Request::default(), nightly());
        assert!(!wants_off(&r, &s, at(10, 22, 59)));
        assert!(wants_off(&r, &s, at(10, 23, 0)));
        assert!(wants_off(&r, &s, at(11, 3, 0)));
        assert!(wants_off(&r, &s, at(11, 8, 59)));
        assert!(!wants_off(&r, &s, at(11, 9, 0)));
        assert_eq!(next_change(&r, &s, at(10, 12, 0)), Some(at(10, 23, 0)));
        assert_eq!(next_change(&r, &s, at(10, 23, 30)), Some(at(11, 9, 0)));
    }

    #[test]
    fn a_request_holds_until_the_next_scheduled_switch() {
        let s = nightly();
        // Turned on at night: stays on through the morning, off again the next night.
        let on = req(Mode::On, at(10, 23, 30), None);
        assert!(!wants_off(&on, &s, at(11, 2, 0)));
        assert!(!wants_off(&on, &s, at(11, 22, 0)));
        assert!(wants_off(&on, &s, at(11, 23, 0)));
        assert_eq!(next_change(&on, &s, at(11, 2, 0)), Some(at(11, 23, 0)));
        // Turned off in the day: back on at 09:00 whatever happens.
        let off = req(Mode::Off, at(10, 10, 0), None);
        assert!(wants_off(&off, &s, at(10, 15, 0)));
        assert!(wants_off(&off, &s, at(11, 8, 0)));
        assert!(!wants_off(&off, &s, at(11, 9, 0)));
        assert_eq!(next_change(&off, &s, at(10, 15, 0)), Some(at(11, 9, 0)));
    }

    #[test]
    fn until_overrides_the_schedule_then_hands_back_to_it() {
        let s = nightly();
        // "Back on at 10": the 09:00 switch doesn't wake it.
        let late = req(Mode::Off, at(10, 23, 30), Some(at(11, 10, 0)));
        assert!(wants_off(&late, &s, at(11, 9, 30)));
        assert!(!wants_off(&late, &s, at(11, 10, 0)));
        assert_eq!(next_change(&late, &s, at(11, 9, 30)), Some(at(11, 10, 0)));
        // A nap ending after 23:00 doesn't turn the screen on at night.
        let nap = req(Mode::Off, at(10, 22, 0), Some(at(10, 23, 30)));
        assert!(wants_off(&nap, &s, at(10, 23, 45)));
        assert!(!wants_off(&nap, &s, at(11, 9, 0)));
        assert_eq!(next_change(&nap, &s, at(10, 22, 30)), Some(at(11, 9, 0)));
        // An `until` days ahead that falls at night: on at the next 09:00 after it.
        let trip = req(Mode::Off, at(10, 12, 0), Some(at(20, 7, 0)));
        assert!(wants_off(&trip, &s, at(20, 8, 0)));
        assert_eq!(next_change(&trip, &s, at(10, 12, 0)), Some(at(20, 9, 0)));
    }

    #[test]
    fn vacation_ignores_the_schedule() {
        let v = req(Mode::Vacation, at(10, 12, 0), None);
        let s = nightly();
        assert!(wants_off(&v, &s, at(11, 9, 0)));
        assert!(wants_off(&v, &s, at(25, 12, 0)));
        assert_eq!(next_change(&v, &s, at(10, 12, 0)), None);
    }

    #[test]
    fn without_a_schedule_requests_last_until_changed() {
        let none = Schedule::default();
        assert!(!wants_off(&Request::default(), &none, at(10, 23, 30)));
        let off = req(Mode::Off, at(10, 12, 0), None);
        assert!(wants_off(&off, &none, at(20, 12, 0)));
        assert_eq!(next_change(&off, &none, at(10, 13, 0)), None);
        let until = req(Mode::Off, at(10, 12, 0), Some(at(10, 14, 0)));
        assert!(wants_off(&until, &none, at(10, 13, 0)));
        assert!(!wants_off(&until, &none, at(10, 14, 0)));
        assert_eq!(
            next_change(&until, &none, at(10, 13, 0)),
            Some(at(10, 14, 0))
        );
    }

    #[test]
    fn off_time_alone_keeps_it_off_until_asked() {
        let s = Schedule {
            off_at: NaiveTime::from_hms_opt(23, 0, 0),
            on_at: None,
        };
        assert!(wants_off(&Request::default(), &s, at(11, 12, 0)));
        let on = req(Mode::On, at(11, 8, 0), None);
        assert!(!wants_off(&on, &s, at(11, 12, 0)));
        assert!(wants_off(&on, &s, at(11, 23, 0)));
    }

    #[test]
    fn describes_results_for_people() {
        let now = Local.timestamp_opt(at(10, 23, 30), 0).unwrap();
        let reply = |display: &str, until: Option<i64>, vacation: bool| {
            let until = until.map(|t| Local.timestamp_opt(t, 0).unwrap().to_rfc3339());
            serde_json::json!({"display": display, "until": until, "vacation": vacation})
        };
        assert_eq!(
            describe(&reply("off", Some(at(11, 9, 0)), false), now),
            "Screen off until tomorrow 09:00."
        );
        assert_eq!(
            describe(&reply("on", Some(at(10, 23, 45)), false), now),
            "Screen on until 23:45."
        );
        assert_eq!(
            describe(&reply("off", Some(at(14, 9, 0)), false), now),
            "Screen off until Mon 14 Jan 09:00."
        );
        assert_eq!(describe(&reply("on", None, false), now), "Screen on.");
        assert!(describe(&reply("off", None, true), now).starts_with("Vacation mode"));
    }

    #[test]
    fn modes_round_trip() {
        for m in [Mode::On, Mode::Off, Mode::Vacation] {
            assert_eq!(Mode::parse(m.as_str()), Some(m));
        }
        assert_eq!(Mode::parse("sleep"), None);
    }

    #[test]
    fn applies_only_changes_and_restores_on_drop() {
        let (mut s, rig) = screen();
        assert_eq!(
            s.sync(false, 0, 0),
            Change::None,
            "startup: on, nothing to report"
        );
        assert_eq!(s.sync(false, 0, 0), Change::None);
        assert_eq!(s.sync(true, 0, 0), Change::TurnedOff);
        assert!(s.is_off());
        assert_eq!(s.sync(true, 0, 0), Change::None);
        assert_eq!(s.sync(false, 0, 0), Change::TurnedOn);
        assert_eq!(s.sync(true, 0, 0), Change::TurnedOff);
        drop(s);
        assert_eq!(rig.switches(), ["on *", "off *", "on *", "off *", "on *"]);
        assert_eq!(
            rig.take_log(),
            [
                "monitor off (standby)",
                "monitor on",
                "monitor off (standby)"
            ]
        );
    }

    #[test]
    fn starting_while_asleep_turns_it_off() {
        let (mut s, rig) = screen();
        assert_eq!(s.sync(true, 0, 0), Change::TurnedOff);
        assert_eq!(rig.switches(), ["off *"]);
    }

    #[test]
    fn on_with_no_outputs_is_retried_until_one_appears() {
        let (mut s, rig) = screen();
        s.sync(true, 0, 0);
        // The monitor drops off HDMI in standby: nothing lit, nothing to do.
        rig.set(&[]);
        assert_eq!(s.sync(true, 0, RECHECK_SECS), Change::None);
        rig.take_log();
        // The morning switch: `wlopm --on '*'` succeeds and switches nothing.
        assert_eq!(s.sync(false, 0, 100), Change::TurnedOn);
        assert_eq!(rig.take_log(), ["monitor on: no outputs yet, retrying"]);
        assert_eq!(s.sync(false, 0, 110), Change::None);
        assert!(rig.take_log().is_empty(), "the same problem is logged once");
        // It reconnects in standby (sway keeps `power off` for its name).
        rig.set(&[(HDMI, false)]);
        assert_eq!(s.sync(false, 0, 120), Change::TurnedOn);
        assert_eq!(rig.take_log(), ["monitor on (HDMI-A-1 came back)"]);
        assert_eq!(rig.outputs(), [(HDMI, true)]);
        assert_eq!(rig.switches(), ["off *", "on *", "on HDMI-A-1"]);
        // Settled: checked again only after RECHECK_SECS.
        let reads = rig.reads();
        assert_eq!(s.sync(false, 0, 120 + RECHECK_SECS - 1), Change::None);
        assert_eq!(rig.reads(), reads);
        assert_eq!(s.sync(false, 0, 120 + RECHECK_SECS), Change::None);
        assert_eq!(rig.reads(), reads + 1);
        assert!(rig.take_log().is_empty());
    }

    #[test]
    fn outputs_in_the_wrong_mode_are_switched_back() {
        let (mut s, rig) = screen();
        s.sync(false, 0, 0);
        // Reconnected in standby between two checks.
        rig.set(&[(HDMI, false)]);
        assert_eq!(s.sync(false, 0, RECHECK_SECS), Change::TurnedOn);
        assert_eq!(rig.take_log(), ["monitor on (HDMI-A-1 was off)"]);
        // At night, one that comes back on goes back into standby.
        s.sync(true, 0, 1000);
        rig.set(&[(HDMI, true)]);
        assert_eq!(s.sync(true, 0, 1000 + RECHECK_SECS), Change::TurnedOff);
        assert_eq!(
            rig.take_log(),
            ["monitor off (standby)", "monitor off (HDMI-A-1 was on)"]
        );
        assert_eq!(rig.outputs(), [(HDMI, false)]);
        // Also when it was missing at the last check.
        rig.set(&[]);
        assert_eq!(s.sync(true, 0, 1000 + 2 * RECHECK_SECS), Change::None);
        rig.set(&[(HDMI, true)]);
        assert_eq!(s.sync(true, 0, 1000 + 3 * RECHECK_SECS), Change::TurnedOff);
        assert_eq!(rig.take_log(), ["monitor off (HDMI-A-1 came back on)"]);
        assert_eq!(
            rig.switches(),
            [
                "on *",
                "on HDMI-A-1",
                "off *",
                "off HDMI-A-1",
                "off HDMI-A-1"
            ]
        );
    }

    #[test]
    fn an_input_wake_is_not_undone_by_the_checks() {
        let (mut s, rig) = screen();
        s.sync(true, 0, 0);
        assert!(s.input(100));
        // Switched off behind its back while awake: woken on purpose, so back on.
        rig.set(&[(HDMI, false)]);
        assert_eq!(s.sync(true, 0, 100 + RECHECK_SECS), Change::TurnedOn);
        let reads = rig.reads();
        for t in (170..100 + INPUT_WAKE_SECS).step_by(10) {
            assert_eq!(s.sync(true, 0, t), Change::None, "at {t}");
        }
        assert!(rig.reads() > reads, "checked while awake");
        assert_eq!(rig.outputs(), [(HDMI, true)]);
        assert_eq!(s.sync(true, 0, 100 + INPUT_WAKE_SECS), Change::TurnedOff);
        assert!(s.is_off());
        assert_eq!(rig.switches(), ["off *", "on *", "on HDMI-A-1", "off *"]);
    }

    #[test]
    fn a_repeated_request_applies_its_state_again() {
        let (mut s, rig) = screen();
        s.sync(false, 0, 0);
        // Dark while the dashboard believes it's on.
        rig.set(&[(HDMI, false)]);
        // `display_on` while on: a newer request, the same wanted state.
        assert_eq!(s.sync(false, 5, 10), Change::TurnedOn);
        assert_eq!(rig.outputs(), [(HDMI, true)]);
        assert_eq!(s.sync(false, 5, 20), Change::None, "the same request");
        // `display_off` during an input wake ends the wake.
        s.sync(true, 5, 30);
        assert!(s.input(40));
        assert_eq!(s.sync(true, 50, 50), Change::TurnedOff);
        assert!(s.is_off());
        assert_eq!(rig.switches(), ["on *", "on *", "off *", "on *", "off *"]);
    }

    #[test]
    fn a_failing_switch_is_retried_and_logged_when_its_result_changes() {
        let (mut s, rig) = screen();
        rig.break_with(Some("no compositor"));
        assert_eq!(s.sync(true, 0, 0), Change::TurnedOff);
        assert_eq!(s.sync(true, 0, 10), Change::None);
        assert_eq!(s.sync(true, 0, 20), Change::None);
        assert_eq!(rig.switches(), ["off *", "off HDMI-A-1", "off HDMI-A-1"]);
        assert_eq!(rig.take_log(), ["monitor off: no compositor, retrying"]);
        rig.break_with(Some("output busy"));
        s.sync(true, 0, 30);
        s.sync(true, 0, 40);
        assert_eq!(rig.take_log(), ["monitor off: output busy, retrying"]);
        // Fixed: the next sync settles it and says so, then it stops switching.
        rig.break_with(None);
        assert_eq!(s.sync(true, 0, 50), Change::TurnedOff);
        assert_eq!(rig.take_log(), ["monitor off (HDMI-A-1 was on)"]);
        assert_eq!(s.sync(true, 0, 60), Change::None);
        assert_eq!(rig.switches().len(), 6);
        assert_eq!(rig.outputs(), [(HDMI, false)]);
    }

    #[test]
    fn a_failing_check_is_logged_once_and_its_end_too() {
        let (mut s, rig) = screen();
        s.sync(false, 0, 0);
        rig.blind(true);
        assert_eq!(s.sync(false, 0, RECHECK_SECS), Change::None);
        assert_eq!(s.sync(false, 0, RECHECK_SECS + 10), Change::None);
        assert_eq!(rig.take_log(), ["monitor on: no Wayland display, retrying"]);
        rig.blind(false);
        assert_eq!(s.sync(false, 0, RECHECK_SECS + 20), Change::TurnedOn);
        assert_eq!(rig.take_log(), ["monitor on (HDMI-A-1)"]);
        assert_eq!(rig.switches(), ["on *"], "nothing needed switching");
    }

    #[test]
    fn input_wakes_standby_until_a_quiet_spell() {
        let (mut s, rig) = screen();
        assert!(!s.input(0), "already on: nothing to wake");
        assert_eq!(s.sync(true, 0, 0), Change::TurnedOff);
        assert!(s.input(100), "woke the monitor");
        assert!(!s.is_off());
        assert!(!s.input(200), "already awake");
        // Each input restarts the quiet spell.
        assert_eq!(s.sync(true, 0, 200 + INPUT_WAKE_SECS - 1), Change::None);
        assert_eq!(s.sync(true, 0, 200 + INPUT_WAKE_SECS), Change::TurnedOff);
        assert!(s.is_off());
        assert_eq!(s.sync(true, 0, 200 + 2 * INPUT_WAKE_SECS), Change::None);
        assert_eq!(rig.switches(), ["off *", "on *", "off *"]);
        assert_eq!(
            rig.take_log(),
            [
                "monitor off (standby)",
                "monitor on (input)",
                "monitor off again (no input)"
            ]
        );
    }

    #[test]
    fn a_requested_change_ends_an_input_wake() {
        let (mut s, rig) = screen();
        s.sync(true, 0, 0);
        s.input(10);
        assert_eq!(s.sync(false, 0, 20), Change::TurnedOn);
        // Off again later: input wakes it again, the quiet spell starts afresh.
        assert_eq!(s.sync(true, 0, 30), Change::TurnedOff);
        assert_eq!(s.sync(true, 0, 30 + INPUT_WAKE_SECS), Change::None);
        assert!(s.input(40));
        drop(s);
        // Dropping an input-woken screen doesn't switch it again: it's on.
        assert_eq!(rig.switches(), ["off *", "on *", "on *", "off *", "on *"]);
    }

    #[test]
    fn reads_wlopm_json() {
        // As wlopm 1.0.0 prints it on the dashboard host.
        let one = b"[\n  {\n    \"output\": \"HDMI-A-1\",\n    \"power-mode\": \"on\"\n  }\n]\n";
        let out = |name: &str, on| Output {
            name: name.into(),
            on,
        };
        assert_eq!(parse_outputs(one).unwrap(), [out("HDMI-A-1", true)]);
        // No outputs: an empty list.
        assert_eq!(parse_outputs(b"[\n]\n").unwrap(), Vec::<Output>::new());
        let two = br#"[
  {"output": "DP-1", "power-mode": "off"},
  {"output": "HDMI-A-1", "power-mode": "on"}
]"#;
        assert_eq!(
            parse_outputs(two).unwrap(),
            [out("DP-1", false), out("HDMI-A-1", true)]
        );
        assert!(parse_outputs(b"").is_err());
        assert!(parse_outputs(b"HDMI-A-1 on\n").is_err());
    }
}
