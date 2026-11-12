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
//! The monitor is switched through sway's IPC socket: `output * power off` puts
//! every output into standby and `output * power on` wakes them, as `swaymsg` would.
//! Not `wlopm` (wlr-output-power-management): that protocol addresses a
//! `wl_output`, and sway 1.12 can withdraw the `wl_output` of an output it powered
//! off, after which nothing can wake it that way. Sway's own command addresses the
//! output by name in its config, so it works in any state, including an output that
//! was unplugged and came back while off. Because a switch can still fail to stick,
//! the actual state is read back every [`VERIFY_SECS`] and applied again if it
//! drifted. The compositor never blanks the dashboard on its own: the kiosk session
//! runs no idle daemon.
//!
//! A key press or pointer move wakes a monitor in standby without changing the
//! requested state. After [`INPUT_WAKE_SECS`] without input it goes back into
//! standby (the X DPMS timeout the dashboard had before Wayland). Keys reach the
//! window, which keeps the focus while the outputs are off; the pointer doesn't
//! (the window is on no output then), so the compositor's idle notifications
//! report it (see `idle.rs`).

use anyhow::Result;
use chrono::{DateTime, Days, Local, NaiveTime, TimeZone};
use serde_json::Value;

use crate::sway::Sway;

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

pub trait Power {
    fn set_on(&mut self, on: bool) -> Result<()>;
    /// Whether each output is powered on now. Empty when there's none (unplugged).
    fn powered(&mut self) -> Result<Vec<bool>>;
}

/// Output power through sway's IPC.
pub struct SwayPower(Sway);

impl SwayPower {
    pub fn from_env() -> Result<Self> {
        Sway::from_env().map(Self)
    }
}

impl Power for SwayPower {
    fn set_on(&mut self, on: bool) -> Result<()> {
        self.0.run(if on {
            "output * power on"
        } else {
            "output * power off"
        })
    }

    fn powered(&mut self) -> Result<Vec<bool>> {
        Ok(self
            .0
            .output_power()?
            .into_iter()
            .map(|(_, on)| on)
            .collect())
    }
}

/// How long a monitor woken by input stays on without further input while it
/// should be in standby.
pub const INPUT_WAKE_SECS: i64 = 600;

/// How often the outputs' actual power is compared with the state applied last.
pub const VERIFY_SECS: i64 = 60;

/// Inputs this soon after the one that woke the monitor belong to the same
/// gesture: a key that arrives just after the compositor reported the activity
/// still does nothing else (Esc would quit).
const WAKE_GESTURE_SECS: i64 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    None,
    TurnedOff,
    TurnedOn,
}

/// Applies the requested state whenever it changes. Dropping it turns the
/// monitor back on, so stopping the dashboard never leaves a dark screen.
pub struct Screen {
    power: Box<dyn Power>,
    /// Last applied state, `Some(true)` = off. `None` before the first sync.
    off: Option<bool>,
    /// Woken by input while it should be off: the time of the last input.
    woken: Option<i64>,
    /// When input last woke it.
    woke_at: Option<i64>,
    /// When the outputs' actual power was last read back (or a switch was made).
    checked: Option<i64>,
    /// A drift was reported and hasn't cleared yet: retry without logging again.
    drifting: bool,
}

impl Screen {
    pub fn new(power: Box<dyn Power>) -> Self {
        Self {
            power,
            off: None,
            woken: None,
            woke_at: None,
            checked: None,
            drifting: false,
        }
    }

    /// In standby now: switched off and not woken by input since.
    pub fn is_off(&self) -> bool {
        self.off == Some(true) && self.woken.is_none()
    }

    /// A key press or pointer move at `now`. Wakes a monitor in standby; the requested
    /// state doesn't change. Returns whether this input woke it, or came within
    /// [`WAKE_GESTURE_SECS`] of the input that did.
    pub fn input(&mut self, now: i64) -> bool {
        if self.off != Some(true) {
            return false;
        }
        let woke = self.woken.is_none();
        self.woken = Some(now);
        if woke {
            self.woke_at = Some(now);
            match self.power.set_on(true) {
                Ok(()) => eprintln!("hermes-beaver: monitor on (input)"),
                Err(e) => eprintln!("hermes-beaver: waking the monitor failed: {e:#}"),
            }
        }
        woke || self.woke_at.is_some_and(|t| now - t <= WAKE_GESTURE_SECS)
    }

    /// The first call applies the state either way: after a restart the monitor
    /// may still be in standby from the previous run. A monitor woken by input goes
    /// back into standby after [`INPUT_WAKE_SECS`] without input.
    pub fn sync(&mut self, want_off: bool, now: i64) -> Change {
        if self.off == Some(want_off) {
            if !self.woken.is_some_and(|last| now - last >= INPUT_WAKE_SECS) {
                self.verify(now);
                return Change::None;
            }
            self.woken = None;
            self.checked = Some(now);
            self.drifting = false;
            match self.power.set_on(false) {
                Ok(()) => eprintln!("hermes-beaver: monitor off again (no input)"),
                Err(e) => eprintln!("hermes-beaver: switching the monitor failed: {e:#}"),
            }
            return Change::TurnedOff;
        }
        self.woken = None;
        let first = self.off.is_none();
        match self.power.set_on(!want_off) {
            Ok(()) if want_off => eprintln!("hermes-beaver: monitor off (standby)"),
            Ok(()) if !first => eprintln!("hermes-beaver: monitor on"),
            Ok(()) => {}
            Err(e) => eprintln!("hermes-beaver: switching the monitor failed: {e:#}"),
        }
        // Marked as applied even after a failure: retrying every tick would
        // only repeat the error. `verify` tries again in a minute.
        self.off = Some(want_off);
        // Sway applies a switch after the command returns: read it back later.
        self.checked = Some(now);
        // A new state: an earlier drift is no longer the question.
        self.drifting = false;
        match (want_off, first) {
            (true, _) => Change::TurnedOff,
            (false, true) => Change::None,
            (false, false) => Change::TurnedOn,
        }
    }

    /// Every [`VERIFY_SECS`]: if an output isn't in the state applied last (an input
    /// wake counts as on), applies it again. Logs when a drift starts and ends, not
    /// at every retry.
    fn verify(&mut self, now: i64) {
        if self.checked.is_some_and(|t| now - t < VERIFY_SECS) {
            return;
        }
        self.checked = Some(now);
        let Some(off) = self.off else { return };
        let want_on = !off || self.woken.is_some();
        let word = |on: bool| if on { "on" } else { "off" };
        let powered = match self.power.powered() {
            Ok(p) => p,
            Err(e) => {
                if !self.drifting {
                    eprintln!("hermes-beaver: reading the monitor's state failed: {e:#}");
                    self.drifting = true;
                }
                return;
            }
        };
        if powered.iter().all(|&on| on == want_on) {
            if self.drifting {
                eprintln!("hermes-beaver: monitor {} as it should be", word(want_on));
                self.drifting = false;
            }
            return;
        }
        let fresh = !self.drifting;
        if fresh {
            eprintln!(
                "hermes-beaver: monitor should be {} but is {}: switching it again",
                word(want_on),
                word(!want_on)
            );
            self.drifting = true;
        }
        if let Err(e) = self.power.set_on(want_on)
            && fresh
        {
            eprintln!("hermes-beaver: switching the monitor failed: {e:#}");
        }
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        if self.is_off()
            && let Err(e) = self.power.set_on(true)
        {
            eprintln!("hermes-beaver: turning the monitor back on failed: {e:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    /// What the fake compositor saw and shows.
    #[derive(Default)]
    struct Outputs {
        /// Every `set_on`, in order.
        calls: Vec<bool>,
        /// Each output's power; none by default.
        power: Vec<bool>,
        /// Switches don't stick (the sway 1.12 `wl_output` case).
        stuck: bool,
    }

    struct Fake(Arc<Mutex<Outputs>>);

    impl Power for Fake {
        fn set_on(&mut self, on: bool) -> Result<()> {
            let mut o = self.0.lock().unwrap();
            o.calls.push(on);
            if !o.stuck {
                o.power.iter_mut().for_each(|p| *p = on);
            }
            Ok(())
        }

        fn powered(&mut self) -> Result<Vec<bool>> {
            Ok(self.0.lock().unwrap().power.clone())
        }
    }

    fn screen() -> (Screen, Arc<Mutex<Outputs>>) {
        let outputs = Arc::new(Mutex::new(Outputs::default()));
        (Screen::new(Box::new(Fake(outputs.clone()))), outputs)
    }

    fn calls(o: &Arc<Mutex<Outputs>>) -> Vec<bool> {
        o.lock().unwrap().calls.clone()
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
        let (mut s, log) = screen();
        assert_eq!(
            s.sync(false, 0),
            Change::None,
            "startup: on, nothing to report"
        );
        assert_eq!(s.sync(false, 0), Change::None);
        assert_eq!(s.sync(true, 0), Change::TurnedOff);
        assert!(s.is_off());
        assert_eq!(s.sync(true, 0), Change::None);
        assert_eq!(s.sync(false, 0), Change::TurnedOn);
        assert_eq!(s.sync(true, 0), Change::TurnedOff);
        drop(s);
        assert_eq!(calls(&log), vec![true, false, true, false, true]);
    }

    #[test]
    fn starting_while_asleep_turns_it_off() {
        let (mut s, log) = screen();
        assert_eq!(s.sync(true, 0), Change::TurnedOff);
        assert_eq!(calls(&log), vec![false]);
    }

    #[test]
    fn failures_are_not_retried_every_tick() {
        struct Broken(u32);
        impl Power for Broken {
            fn set_on(&mut self, _: bool) -> Result<()> {
                self.0 += 1;
                anyhow::bail!("no compositor")
            }
            fn powered(&mut self) -> Result<Vec<bool>> {
                anyhow::bail!("no compositor")
            }
        }
        let mut s = Screen::new(Box::new(Broken(0)));
        assert_eq!(s.sync(true, 0), Change::TurnedOff);
        assert_eq!(s.sync(true, 0), Change::None);
    }

    #[test]
    fn input_wakes_standby_until_a_quiet_spell() {
        let (mut s, log) = screen();
        assert!(!s.input(0), "already on: nothing to wake");
        assert_eq!(s.sync(true, 0), Change::TurnedOff);
        assert!(s.input(100), "woke the monitor");
        assert!(!s.is_off());
        assert!(!s.input(200), "already awake");
        // Each input restarts the quiet spell.
        assert_eq!(s.sync(true, 200 + INPUT_WAKE_SECS - 1), Change::None);
        assert_eq!(s.sync(true, 200 + INPUT_WAKE_SECS), Change::TurnedOff);
        assert!(s.is_off());
        assert_eq!(s.sync(true, 200 + 2 * INPUT_WAKE_SECS), Change::None);
        assert_eq!(calls(&log), vec![false, true, false]);
    }

    #[test]
    fn a_requested_change_ends_an_input_wake() {
        let (mut s, log) = screen();
        s.sync(true, 0);
        s.input(10);
        assert_eq!(s.sync(false, 20), Change::TurnedOn);
        // Off again later: input wakes it again, the quiet spell starts afresh.
        assert_eq!(s.sync(true, 30), Change::TurnedOff);
        assert_eq!(s.sync(true, 30 + INPUT_WAKE_SECS), Change::None);
        assert!(s.input(40));
        drop(s);
        // Dropping an input-woken screen doesn't switch it again: it's on.
        assert_eq!(calls(&log), vec![false, true, true, false, true]);
    }

    #[test]
    fn a_switch_that_didnt_stick_is_made_again_every_minute() {
        let (mut s, log) = screen();
        log.lock().unwrap().power = vec![true];
        s.sync(false, 0);
        assert_eq!(s.sync(true, 10), Change::TurnedOff);
        // The morning switch doesn't reach the monitor.
        log.lock().unwrap().stuck = true;
        assert_eq!(s.sync(false, 100), Change::TurnedOn);
        assert_eq!(s.sync(false, 100 + VERIFY_SECS - 1), Change::None);
        assert_eq!(calls(&log), vec![true, false, true], "too early to check");
        s.sync(false, 100 + VERIFY_SECS);
        assert_eq!(
            calls(&log),
            vec![true, false, true, true],
            "read back off: on again"
        );
        s.sync(false, 100 + 2 * VERIFY_SECS);
        assert_eq!(
            calls(&log).len(),
            5,
            "still off: once a minute, not every tick"
        );
        log.lock().unwrap().stuck = false;
        s.sync(false, 100 + 3 * VERIFY_SECS);
        assert_eq!(log.lock().unwrap().power, vec![true]);
        s.sync(false, 100 + 4 * VERIFY_SECS);
        assert_eq!(
            calls(&log).len(),
            6,
            "on as it should be: nothing more to do"
        );
        assert!(!s.drifting);
    }

    #[test]
    fn a_requested_change_starts_without_an_old_drift() {
        let (mut s, log) = screen();
        log.lock().unwrap().power = vec![true];
        s.sync(false, 0);
        log.lock().unwrap().power = vec![false];
        s.sync(false, VERIFY_SECS);
        assert!(s.drifting, "reported");
        assert_eq!(s.sync(true, VERIFY_SECS + 1), Change::TurnedOff);
        assert!(!s.drifting, "the next drift is reported afresh");
    }

    #[test]
    fn a_monitor_that_came_back_on_by_itself_goes_off_again() {
        let (mut s, log) = screen();
        log.lock().unwrap().power = vec![true];
        s.sync(true, 0);
        log.lock().unwrap().power = vec![true];
        s.sync(true, VERIFY_SECS);
        assert_eq!(calls(&log), vec![false, false]);
        assert_eq!(log.lock().unwrap().power, vec![false]);
    }

    #[test]
    fn checking_leaves_an_input_wake_alone() {
        let (mut s, log) = screen();
        log.lock().unwrap().power = vec![true];
        s.sync(true, 0);
        assert!(s.input(10));
        assert_eq!(s.sync(true, 10 + VERIFY_SECS), Change::None);
        assert_eq!(
            calls(&log),
            vec![false, true],
            "woken and on: nothing to fix"
        );
    }

    #[test]
    fn no_outputs_means_nothing_to_fix() {
        let (mut s, log) = screen();
        s.sync(false, 0);
        s.sync(true, 1);
        s.sync(true, 1 + 5 * VERIFY_SECS);
        assert_eq!(calls(&log), vec![true, false]);
    }

    #[test]
    fn input_right_after_the_wake_belongs_to_it() {
        let (mut s, log) = screen();
        s.sync(true, 0);
        assert!(s.input(100), "woke it");
        assert!(s.input(101), "the key right after: swallowed too");
        assert!(!s.input(103), "later input is ordinary input");
        assert_eq!(calls(&log), vec![false, true], "switched on once");
    }
}
