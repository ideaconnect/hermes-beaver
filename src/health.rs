//! Health rules: decide whether the red banner (and the broken beaver) is on.
//!
//! These read Hermes directly because a broken agent can't report its own
//! breakage over MCP: a run that fails before the model starts never calls a tool.

use std::time::Duration;

use crate::hermes::HermesState;
use crate::mask::mask;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Health {
    pub problems: Vec<String>,
}

impl Health {
    pub fn ok(&self) -> bool {
        self.problems.is_empty()
    }

    /// One line for the header banner.
    pub fn banner(&self) -> String {
        self.problems.join("  ·  ")
    }
}

pub fn evaluate(st: &HermesState, heartbeat_max_age: Duration, check_jobs: bool) -> Health {
    let mut problems = Vec::new();
    if !st.home_exists {
        problems.push("Hermes home not found".to_owned());
        return Health { problems };
    }
    match st.heartbeat_age {
        None => problems.push("Hermes gateway not running (no heartbeat)".to_owned()),
        Some(age) if age > heartbeat_max_age => problems.push(format!(
            "Hermes gateway not responding (heartbeat {} old)",
            human(age)
        )),
        _ => {}
    }
    if check_jobs {
        let finished: Vec<_> = st
            .executions
            .iter()
            .filter(|e| e.finished_at.is_some())
            .take(2)
            .collect();
        if finished.len() == 2 && finished.iter().all(|e| e.status == "failed") {
            let reason = finished[0]
                .error
                .as_deref()
                .and_then(|e| e.lines().map(str::trim).find(|l| !l.is_empty()))
                .map(|l| truncate(&mask(l), 90))
                .unwrap_or_else(|| "no error message".to_owned());
            problems.push(format!("Agent runs failing: {reason}"));
        }
    }
    if let Some(n) = st.bober_failures.filter(|n| *n > 0) {
        problems.push(format!(
            "Bober: {n} consecutive failure{}",
            if n == 1 { "" } else { "s" }
        ));
    }
    problems.extend(st.read_errors.iter().cloned());
    Health { problems }
}

fn human(d: Duration) -> String {
    let s = d.as_secs();
    if s < 120 {
        format!("{s}s")
    } else if s < 7200 {
        format!("{}m", s / 60)
    } else {
        format!("{}h", s / 3600)
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        s.chars().take(max - 1).chain(['…']).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hermes::Execution;

    fn exec(status: &str, finished: bool, error: Option<&str>) -> Execution {
        Execution {
            job_id: "j".into(),
            status: status.into(),
            started_at: Some(1),
            finished_at: finished.then_some(2),
            error: error.map(Into::into),
        }
    }

    fn healthy() -> HermesState {
        HermesState {
            home_exists: true,
            heartbeat_age: Some(Duration::from_secs(10)),
            ..Default::default()
        }
    }

    const MAX: Duration = Duration::from_secs(180);

    #[test]
    fn healthy_state_has_no_problems() {
        let mut st = healthy();
        st.executions = vec![
            exec("completed", true, None),
            exec("failed", true, Some("x")),
        ];
        assert!(evaluate(&st, MAX, true).ok());
    }

    #[test]
    fn stale_or_missing_heartbeat() {
        let mut st = healthy();
        st.heartbeat_age = Some(Duration::from_secs(400));
        assert_eq!(
            evaluate(&st, MAX, false).problems,
            ["Hermes gateway not responding (heartbeat 6m old)"]
        );
        st.heartbeat_age = None;
        assert!(evaluate(&st, MAX, false).banner().contains("not running"));
    }

    #[test]
    fn two_failed_runs_raise_the_masked_first_error_line() {
        let mut st = healthy();
        st.executions = vec![
            exec("running", false, None),
            exec(
                "failed",
                true,
                Some(
                    "\n[blocked_config] provider credential missing: key sk-ant-abcdefghijkl1234\ntrace…",
                ),
            ),
            exec("failed", true, Some("older")),
            exec("completed", true, None),
        ];
        let h = evaluate(&st, MAX, true);
        assert_eq!(
            h.problems,
            ["Agent runs failing: [blocked_config] provider credential missing: key ••••"]
        );
        // Without configured jobs the rule is skipped.
        assert!(evaluate(&st, MAX, false).ok());
    }

    #[test]
    fn bober_failures_and_read_errors() {
        let mut st = healthy();
        st.bober_failures = Some(1);
        st.read_errors = vec!["cannot read cron ledger: locked".into()];
        assert_eq!(
            evaluate(&st, MAX, true).problems,
            [
                "Bober: 1 consecutive failure",
                "cannot read cron ledger: locked"
            ]
        );
    }

    #[test]
    fn missing_home() {
        let st = HermesState::default();
        assert_eq!(evaluate(&st, MAX, true).problems, ["Hermes home not found"]);
    }
}
