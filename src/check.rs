//! Module check periodically requests the configured URLs and tracks whether they are up.

use crate::config;
use crate::database;
use crate::email;
use crate::metrics;
use crate::outage;
use std::collections::BTreeMap;
use std::sync;
use std::sync::mpsc;
use std::thread;

/// Number of recent check results retained for each check.
const RESULTS_RETENTION: usize = 100;
/// Number of incidents retained for each check.
const INCIDENTS_RETENTION: usize = 20;
/// Number of days of uptime history retained for each check and project.
const DAYS_RETENTION: usize = 90;

const DB_KEY: &str = "check_manager/projects";
/// Before projects were added, checks were saved under this key by name.
/// It is only read if there is nothing under `DB_KEY`.
const LEGACY_DB_KEY: &str = "check_manager/checks";
/// The outage tracker's state changed format when projects were added, so it moved to a new
/// key. Any outage open under the old key is dropped.
const OUTAGE_DB_KEY: &str = "check_manager/project_outage";

type Time = chrono::DateTime<chrono::Utc>;

pub struct Manager<'a> {
    hostname: String,
    ui_url: String,
    projects: sync::Mutex<Vec<Project>>,
    outage: sync::Mutex<outage::Tracker>,
    db: &'a dyn database::DB,
    notifier: &'a dyn email::Notifier,
    poll_interval: chrono::Duration,
}

impl<'a> Manager<'a> {
    pub fn new(
        db: &'a dyn database::DB,
        notifier: &'a dyn email::Notifier,
        hostname: String,
        ui_url: String,
        projects: Vec<config::ProjectConfig>,
        poll_interval: chrono::Duration,
    ) -> Self {
        let saved: Option<BTreeMap<String, serde_json::Value>> =
            database::get_typed(db, DB_KEY).unwrap();
        let mut legacy: BTreeMap<String, serde_json::Value> = match saved {
            Some(_) => Default::default(),
            None => database::get_typed(db, LEGACY_DB_KEY)
                .unwrap()
                .unwrap_or_default(),
        };
        let mut saved = saved.unwrap_or_default();
        let mut projects: Vec<Project> = projects
            .into_iter()
            .map(|config| {
                let saved = saved.remove(&config.project);
                Project::restore(config, saved, &mut legacy)
            })
            .collect();
        projects.sort_by_key(|p| p.name.clone());
        let m = metrics::get();
        m.checks_configured
            .set(projects.iter().map(|p| p.checks.len()).sum::<usize>() as i64);
        for project in &projects {
            project.init_metrics();
        }
        let outage = database::get_typed(db, OUTAGE_DB_KEY)
            .unwrap()
            .unwrap_or_default();
        Self {
            hostname,
            ui_url,
            projects: sync::Mutex::new(projects),
            outage: sync::Mutex::new(outage),
            db,
            notifier,
            poll_interval,
        }
    }
    pub fn start<'scope>(&'a self, scope: &'scope thread::Scope<'scope, 'a>) -> Stopper {
        let (tx, rx) = mpsc::channel();
        scope.spawn(move || {
            self.run(rx);
        });
        Stopper { tx }
    }
    pub fn projects(&self) -> Vec<Project> {
        (*self.projects.lock().unwrap()).clone()
    }
    pub fn poll_interval(&self) -> chrono::Duration {
        self.poll_interval
    }
}

pub struct Stopper {
    tx: mpsc::Sender<()>,
}

impl Stopper {
    pub fn stop(self) {
        eprintln!("[check_manager] shutdown signal received");
        self.tx.send(()).unwrap();
        eprintln!("[check_manager] signalled to work thread; waiting to stop");
    }
}

impl<'a> Manager<'a> {
    fn run(&self, rx: mpsc::Receiver<()>) {
        eprintln!("[check_manager] work thread started");
        loop {
            let start = chrono::Utc::now();

            let mut projects = (*self.projects.lock().unwrap()).clone();
            // All checks run in parallel so that one slow URL doesn't delay the others.
            // Each check is bounded by its timeout.
            let results: Vec<Vec<(CheckResult, &str)>> = thread::scope(|s| {
                let handles: Vec<Vec<_>> = projects
                    .iter()
                    .map(|project| {
                        project
                            .checks
                            .iter()
                            .map(|check| s.spawn(|| probe(&check.config, &self.hostname)))
                            .collect()
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|handles| handles.into_iter().map(|h| h.join().unwrap()).collect())
                    .collect()
            });
            let now = chrono::Utc::now();
            let m = metrics::get();
            for (project, results) in projects.iter_mut().zip(results) {
                let project_name = project.name.clone();
                for (check, (result, result_label)) in project.checks.iter_mut().zip(results) {
                    let name = check.config.name.clone();
                    let labels = [project_name.as_str(), name.as_str()];
                    m.checks
                        .with_label_values(&[labels[0], labels[1], result_label])
                        .inc();
                    m.check_duration
                        .with_label_values(&labels)
                        .observe(result.latency_ms as f64 / 1000.0);
                    if !result.success {
                        eprintln!(
                            "[{project_name}/{name}] check failed: {}",
                            result.error.as_deref().unwrap_or("unknown error")
                        );
                    }
                    if check.record(result) == Some(Transition::Down) {
                        m.incidents.with_label_values(&labels).inc();
                    }
                    check.update_state_metrics(&project_name);
                }
                project.record(now);
                project.update_state_metrics();
            }
            // Emails are sent per outage rather than per project, so that a whole node going
            // down results in one email thread.
            let down = projects.iter().filter_map(Project::down).collect();
            let mut outage = self.outage.lock().unwrap();
            if let Some(email) = outage.update(&self.hostname, &self.ui_url, now, down) {
                self.notifier.notify(&email);
            }
            database::set_typed(self.db, OUTAGE_DB_KEY.into(), &*outage).unwrap();
            drop(outage);
            let saved: BTreeMap<&str, &Project> = projects
                .iter()
                .map(|project| (project.name.as_str(), project))
                .collect();
            database::set_typed(self.db, DB_KEY.into(), &saved).unwrap();
            *self.projects.lock().unwrap() = projects;

            let end = chrono::Utc::now();
            let loop_duration = end - start;
            m.poll_loop_duration
                .observe(loop_duration.num_milliseconds() as f64 / 1000.0);
            metrics::set_timestamp(&m.poll_loop_last_run, end);
            match self.poll_interval.checked_sub(&loop_duration) {
                Some(remaining) => {
                    if rx
                        .recv_timeout(remaining.to_std().unwrap_or_default())
                        .is_ok()
                    {
                        eprintln!("[check_manager] sleep interrupted because of shut down signal");
                        return;
                    }
                }
                None => {
                    eprintln!("[check_manager] time to run all checks ({loop_duration:?}) was longer than the poll interval ({:?}). Will check again immediately", self.poll_interval);
                    if rx.try_recv().is_ok() {
                        return;
                    }
                }
            }
        }
    }
}

/// Requests the URL in the check config and determines whether the check passed.
///
/// Also returns the result label for the `status_checks_total` metric.
fn probe(config: &config::CheckConfig, hostname: &str) -> (CheckResult, &'static str) {
    let time = chrono::Utc::now();
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(config.timeout_secs))
        .user_agent(&format!("status-agent ({hostname})"))
        .build();
    let start = std::time::Instant::now();
    let request = if config.skip_body {
        agent.head(&config.url)
    } else {
        agent.get(&config.url)
    };
    let response = match request.call() {
        Ok(response) => Ok(response),
        // Responses with 4xx and 5xx status codes are returned as errors by ureq.
        Err(ureq::Error::Status(_, response)) => Ok(response),
        Err(ureq::Error::Transport(err)) => {
            // The error message starts with the URL, which is already shown alongside it.
            let message = err.to_string();
            let message = match err.url() {
                Some(url) => message
                    .strip_prefix(&format!("{url}: "))
                    .unwrap_or(&message)
                    .to_string(),
                None => message,
            };
            Err((message, transport_error_result(&err)))
        }
    };
    let (status_code, error) = match response {
        Err(err) => (None, Some(err)),
        Ok(response) => {
            let status = response.status();
            let status_ok = match config.expected_status {
                None => (200..300).contains(&status),
                Some(expected) => status == expected,
            };
            let error = if !status_ok {
                let message = match config.expected_status {
                    None => format!("HTTP {status}"),
                    Some(expected) => format!("HTTP {status} (expected {expected})"),
                };
                Some((message, "http_status"))
            } else if let Some(contains) = &config.contains {
                match response.into_string() {
                    Err(err) => {
                        let result = if err.kind() == std::io::ErrorKind::TimedOut {
                            "timeout"
                        } else {
                            "other"
                        };
                        Some((format!("failed to read response body: {err}"), result))
                    }
                    Ok(body) if !body.contains(contains.as_str()) => Some((
                        format!("response body does not contain {contains:?}"),
                        "content",
                    )),
                    Ok(_) => None,
                }
            } else {
                None
            };
            (Some(status), error)
        }
    };
    let (error, result) = match error {
        None => (None, "success"),
        Some((message, result)) => (Some(message), result),
    };
    let check_result = CheckResult {
        time,
        success: error.is_none(),
        status_code,
        latency_ms: start.elapsed().as_millis() as u64,
        error,
    };
    (check_result, result)
}

/// Classifies a transport error for the `result` label of `status_checks_total`.
fn transport_error_result(err: &ureq::Transport) -> &'static str {
    use std::error::Error;
    // Timeouts surface as I/O errors somewhere in the chain of sources,
    // whether they happen while connecting or while reading.
    let mut source = err.source();
    while let Some(e) = source {
        if let Some(io) = e.downcast_ref::<std::io::Error>() {
            if matches!(
                io.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            ) {
                return "timeout";
            }
        }
        source = e.source();
    }
    match err.kind() {
        ureq::ErrorKind::Dns => "dns",
        ureq::ErrorKind::ConnectionFailed => "connection",
        _ => "other",
    }
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// The check hasn't passed or reached its failure threshold since it was added.
    Unknown,
    Up,
    Down,
}

/// A project and its checks.
///
/// The project is down if any of its checks are down, and up if none are down and at least
/// one is up.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct Project {
    name: String,
    state: State,
    /// When the current state started. For the down state this is the time of the first
    /// failed check of the check that went down first.
    since: Option<Time>,
    /// Most recent first. A poll of the project fails if any of its checks fail.
    days: Vec<Day>,
    checks: Vec<Check>,
}

impl Project {
    fn new(name: String) -> Self {
        Self {
            name,
            state: State::Unknown,
            since: None,
            days: vec![],
            checks: vec![],
        }
    }

    /// Restores the project from its saved state, keeping the history of any checks that
    /// are still configured. Checks are matched by name, first within the saved project and
    /// then in the legacy checks that were saved before projects were added.
    ///
    /// Saved state that can't be read is dropped, so changes to the config format don't
    /// lose history.
    fn restore(
        config: config::ProjectConfig,
        saved: Option<serde_json::Value>,
        legacy: &mut BTreeMap<String, serde_json::Value>,
    ) -> Self {
        let mut saved = saved.unwrap_or_default();
        let mut saved_checks: BTreeMap<String, serde_json::Value> =
            match saved.get_mut("checks").map(serde_json::Value::take) {
                Some(serde_json::Value::Array(checks)) => checks
                    .into_iter()
                    .filter_map(|check| {
                        let name = check.get("config")?.get("name")?.as_str()?.to_string();
                        Some((name, check))
                    })
                    .collect(),
                _ => Default::default(),
            };
        saved["name"] = config.project.clone().into();
        saved["checks"] = serde_json::Value::Array(vec![]);
        let mut project: Project =
            serde_json::from_value(saved).unwrap_or_else(|_| Project::new(config.project));
        project.checks = config
            .checks
            .into_iter()
            .map(|check_config| {
                let saved = saved_checks
                    .remove(&check_config.name)
                    .or_else(|| legacy.remove(&check_config.name));
                Check::restore(check_config, saved)
            })
            .collect();
        project
    }

    /// Creates this project's metrics and those of its checks.
    fn init_metrics(&self) {
        for check in &self.checks {
            check.init_metrics(&self.name);
        }
        self.update_state_metrics();
    }

    fn update_state_metrics(&self) {
        let m = metrics::get();
        let name = self.name.as_str();
        match self.state {
            State::Up => m.project_up.with_label_values(&[name]).set(1),
            State::Down => m.project_up.with_label_values(&[name]).set(0),
            State::Unknown => {
                let _ = m.project_up.remove_label_values(&[name]);
            }
        }
    }

    /// Updates the state and history of the project after its checks have been recorded.
    fn record(&mut self, now: Time) {
        let date = now.date_naive();
        if self.days.first().map(|day| day.date) != Some(date) {
            self.days.insert(0, Day::new(date));
            self.days.truncate(DAYS_RETENTION);
        }
        self.days[0].checks += 1;
        if self.checks.iter().any(|c| c.consecutive_failures > 0) {
            self.days[0].failures += 1;
        }
        let states: Vec<State> = self.checks.iter().map(|c| c.state).collect();
        let state = if states.contains(&State::Down) {
            State::Down
        } else if states.contains(&State::Up) {
            State::Up
        } else {
            State::Unknown
        };
        if state != self.state {
            self.state = state;
            self.since = match state {
                State::Down => self.down_checks().filter_map(|c| c.since).min(),
                State::Up | State::Unknown => Some(now),
            };
        }
        if self.state == State::Down {
            self.days[0].down = true;
        }
    }

    fn down_checks(&self) -> impl Iterator<Item = &Check> {
        self.checks.iter().filter(|c| c.state == State::Down)
    }

    /// Returns details of the project if it is down.
    fn down(&self) -> Option<outage::Down> {
        if self.state != State::Down {
            return None;
        }
        let down: Vec<&Check> = self.down_checks().collect();
        let n = self.checks.len();
        let summary = match down.as_slice() {
            [check] if n == 1 => check.error().to_string(),
            [check] => format!(
                "{}: {} (1 of {n} checks down)",
                check.config.name,
                check.error()
            ),
            _ => format!("{} of {n} checks down", down.len()),
        };
        Some(outage::Down {
            name: self.name.clone(),
            since: self.since?,
            summary,
        })
    }
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct Check {
    config: config::CheckConfig,
    state: State,
    /// When the current state started. For the down state this is the time of the first
    /// failed check, not the time the failure threshold was reached.
    since: Option<Time>,
    consecutive_failures: u32,
    /// Time of the first of the current consecutive failures.
    failing_since: Option<Time>,
    /// Most recent first.
    results: Vec<CheckResult>,
    /// Most recent first.
    incidents: Vec<Incident>,
    /// Most recent first.
    days: Vec<Day>,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct CheckResult {
    time: chrono::DateTime<chrono::Utc>,
    success: bool,
    status_code: Option<u16>,
    latency_ms: u64,
    error: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
struct Incident {
    start: chrono::DateTime<chrono::Utc>,
    /// None if the incident is ongoing.
    end: Option<chrono::DateTime<chrono::Utc>>,
    /// Error from the check that reached the failure threshold.
    error: String,
}

/// Summary of the checks on one day (UTC).
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
struct Day {
    date: chrono::NaiveDate,
    checks: u32,
    failures: u32,
    /// Whether the check was in the down state at any point during the day.
    down: bool,
}

impl Day {
    fn new(date: chrono::NaiveDate) -> Self {
        Self {
            date,
            checks: 0,
            failures: 0,
            down: false,
        }
    }
}

#[derive(Debug, PartialEq)]
enum Transition {
    Down,
    Recovered,
}

impl Check {
    fn new(config: config::CheckConfig) -> Self {
        Self {
            config,
            state: State::Unknown,
            since: None,
            consecutive_failures: 0,
            failing_since: None,
            results: vec![],
            incidents: vec![],
            days: vec![],
        }
    }

    /// Restores the check from its saved state, using the given config rather than the
    /// saved one. Saved state that can't be read is dropped.
    fn restore(config: config::CheckConfig, saved: Option<serde_json::Value>) -> Self {
        let Some(mut saved) = saved else {
            return Check::new(config);
        };
        saved["config"] = serde_json::to_value(&config).unwrap();
        match serde_json::from_value(saved) {
            Ok(check) => check,
            Err(err) => {
                eprintln!(
                    "[check_manager] dropping unreadable saved state for check {:?}: {err}",
                    config.name
                );
                Check::new(config)
            }
        }
    }

    /// Creates this check's metrics so that counters start at zero, and sets the gauges
    /// from state loaded from the database.
    fn init_metrics(&self, project: &str) {
        let m = metrics::get();
        let name = self.config.name.as_str();
        for result in metrics::CHECK_RESULTS {
            m.checks.with_label_values(&[project, name, result]);
        }
        m.incidents.with_label_values(&[project, name]);
        self.update_state_metrics(project);
    }

    fn update_state_metrics(&self, project: &str) {
        let m = metrics::get();
        let labels = [project, self.config.name.as_str()];
        match self.state {
            State::Up => m.check_up.with_label_values(&labels).set(1),
            State::Down => m.check_up.with_label_values(&labels).set(0),
            State::Unknown => {
                let _ = m.check_up.remove_label_values(&labels);
            }
        }
        m.check_consecutive_failures
            .with_label_values(&labels)
            .set(self.consecutive_failures as i64);
        if let Some(since) = self.since {
            metrics::set_timestamp(&m.check_state_since.with_label_values(&labels), since);
        }
    }

    /// Error from the check that reached the failure threshold in the latest incident.
    fn error(&self) -> &str {
        self.incidents
            .first()
            .map(|incident| incident.error.as_str())
            .unwrap_or_default()
    }

    /// Records the result of a check, returning the transition if the state changed
    /// from up to down or vice versa.
    fn record(&mut self, result: CheckResult) -> Option<Transition> {
        let now = result.time;
        let date = now.date_naive();
        if self.days.first().map(|day| day.date) != Some(date) {
            self.days.insert(0, Day::new(date));
            self.days.truncate(DAYS_RETENTION);
        }
        self.days[0].checks += 1;

        let transition = if result.success {
            self.consecutive_failures = 0;
            self.failing_since = None;
            let previous_state = self.state;
            if previous_state != State::Up {
                self.state = State::Up;
                self.since = Some(now);
            }
            match previous_state {
                State::Down => {
                    self.incidents[0].end = Some(now);
                    Some(Transition::Recovered)
                }
                State::Unknown | State::Up => None,
            }
        } else {
            self.days[0].failures += 1;
            self.consecutive_failures += 1;
            if self.consecutive_failures == 1 {
                self.failing_since = Some(now);
            }
            let first_failure = self.failing_since.unwrap_or(now);
            if self.state != State::Down
                && self.consecutive_failures >= self.config.failure_threshold
            {
                let error = result.error.clone().unwrap_or_default();
                self.state = State::Down;
                self.since = Some(first_failure);
                self.incidents.insert(
                    0,
                    Incident {
                        start: first_failure,
                        end: None,
                        error,
                    },
                );
                self.incidents.truncate(INCIDENTS_RETENTION);
                Some(Transition::Down)
            } else {
                None
            }
        };
        if self.state == State::Down {
            self.days[0].down = true;
        }
        self.results.insert(0, result);
        self.results.truncate(RESULTS_RETENTION);
        transition
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_check(failure_threshold: u32) -> Check {
        Check::new(config::CheckConfig {
            name: "test".into(),
            url: "https://example.com".into(),
            skip_body: false,
            expected_status: None,
            contains: None,
            timeout_secs: 10,
            failure_threshold,
        })
    }

    fn time(minute: i64) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-10-02T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc)
            + chrono::Duration::minutes(minute)
    }

    fn result(minute: i64, success: bool) -> CheckResult {
        CheckResult {
            time: time(minute),
            success,
            status_code: Some(if success { 200 } else { 502 }),
            latency_ms: 5,
            error: if success {
                None
            } else {
                Some("HTTP 502".into())
            },
        }
    }

    #[test]
    fn unknown_to_up_does_not_notify() {
        let mut check = new_check(2);
        assert_eq!(check.record(result(0, true)), None);
        assert_eq!(check.state, State::Up);
        assert_eq!(check.since, Some(time(0)));
        assert_eq!(check.record(result(1, true)), None);
        assert_eq!(check.since, Some(time(0)));
    }

    #[test]
    fn down_after_threshold_then_recovered() {
        let mut check = new_check(3);
        check.record(result(0, true));
        assert_eq!(check.record(result(1, false)), None);
        assert_eq!(check.record(result(2, false)), None);
        assert_eq!(check.state, State::Up);
        assert_eq!(check.record(result(3, false)), Some(Transition::Down));
        assert_eq!(check.state, State::Down);
        assert_eq!(check.since, Some(time(1)));
        assert_eq!(check.incidents.len(), 1);
        assert_eq!(check.incidents[0].start, time(1));
        assert_eq!(check.incidents[0].end, None);

        // Further failures don't send more emails.
        assert_eq!(check.record(result(4, false)), None);

        assert_eq!(check.record(result(5, true)), Some(Transition::Recovered));
        assert_eq!(check.state, State::Up);
        assert_eq!(check.since, Some(time(5)));
        assert_eq!(check.incidents[0].end, Some(time(5)));
        assert_eq!(check.consecutive_failures, 0);
    }

    #[test]
    fn failures_below_threshold_reset() {
        let mut check = new_check(2);
        check.record(result(0, true));
        check.record(result(1, false));
        check.record(result(2, true));
        assert_eq!(check.record(result(3, false)), None);
        assert_eq!(check.state, State::Up);
        assert!(check.incidents.is_empty());
    }

    #[test]
    fn unknown_to_down_notifies() {
        let mut check = new_check(1);
        assert_eq!(check.record(result(0, false)), Some(Transition::Down));
        assert_eq!(check.state, State::Down);
        assert_eq!(check.since, Some(time(0)));
    }

    #[test]
    fn day_buckets() {
        let mut check = new_check(2);
        check.record(result(0, true));
        check.record(result(1, false));
        check.record(result(60 * 24, false));
        check.record(result(60 * 24 + 1, false));

        assert_eq!(check.days.len(), 2);
        let today = &check.days[0];
        assert_eq!(today.date, time(60 * 24).date_naive());
        assert_eq!((today.checks, today.failures, today.down), (2, 2, true));
        let yesterday = &check.days[1];
        assert_eq!(
            (yesterday.checks, yesterday.failures, yesterday.down),
            (2, 1, false)
        );
        // The incident started with the first failure on the previous day.
        assert_eq!(check.incidents[0].start, time(1));
    }

    #[test]
    fn retention() {
        let mut check = new_check(2);
        for i in 0..200 {
            check.record(result(i * 60 * 24, true));
        }
        assert_eq!(check.days.len(), DAYS_RETENTION);
        assert_eq!(check.results.len(), RESULTS_RETENTION);
    }

    fn new_project(names: &[&str]) -> Project {
        let mut project = Project::new("project".into());
        project.checks = names
            .iter()
            .map(|name| {
                let mut check = new_check(1);
                check.config.name = name.to_string();
                check
            })
            .collect();
        project
    }

    /// Records a result for each check in the project, given whether each passed.
    fn record_project(project: &mut Project, minute: i64, success: &[bool]) {
        for (check, success) in project.checks.iter_mut().zip(success) {
            check.record(result(minute, *success));
        }
        project.record(time(minute));
    }

    #[test]
    fn project_state() {
        let mut project = new_project(&["a", "b", "c"]);
        project.record(time(0));
        assert_eq!(project.state, State::Unknown);
        assert!(project.down().is_none());

        // Checks that are still unknown don't stop the project being up.
        project.checks[0].record(result(1, true));
        project.record(time(1));
        assert_eq!(project.state, State::Up);
        assert_eq!(project.since, Some(time(1)));

        record_project(&mut project, 2, &[true, false, true]);
        assert_eq!(project.state, State::Down);
        assert_eq!(project.since, Some(time(2)));
        let down = project.down().unwrap();
        assert_eq!(down.since, time(2));
        assert_eq!(down.summary, "b: HTTP 502 (1 of 3 checks down)");

        // Another check going down doesn't change when the project went down.
        record_project(&mut project, 3, &[false, false, true]);
        assert_eq!(project.since, Some(time(2)));
        assert_eq!(project.down().unwrap().summary, "2 of 3 checks down");

        record_project(&mut project, 4, &[true, true, true]);
        assert_eq!(project.state, State::Up);
        assert_eq!(project.since, Some(time(4)));

        let today = &project.days[0];
        assert_eq!((today.checks, today.failures, today.down), (5, 2, true));
    }

    #[test]
    fn single_check_project_summary() {
        let mut project = new_project(&["a"]);
        record_project(&mut project, 0, &[false]);
        assert_eq!(project.down().unwrap().summary, "HTTP 502");
    }

    #[test]
    fn restore() {
        let mut project = new_project(&["a", "b"]);
        record_project(&mut project, 0, &[true, false]);
        let saved = serde_json::to_value(&project).unwrap();
        let mut legacy_check = new_check(1);
        legacy_check.record(result(0, false));
        let mut legacy = BTreeMap::from([(
            "legacy".to_string(),
            serde_json::to_value(&legacy_check).unwrap(),
        )]);
        let config = |name: &str| {
            let mut check = new_check(5).config;
            check.name = name.into();
            check
        };

        let restored = Project::restore(
            config::ProjectConfig {
                project: "project".into(),
                checks: vec![config("b"), config("legacy"), config("new")],
            },
            Some(saved),
            &mut legacy,
        );

        assert_eq!(restored.state, State::Down);
        assert_eq!(restored.days.len(), 1);
        let names: Vec<&str> = restored
            .checks
            .iter()
            .map(|c| c.config.name.as_str())
            .collect();
        assert_eq!(names, vec!["b", "legacy", "new"]);
        // The config comes from the new config file, not the saved state.
        assert_eq!(restored.checks[0].config.failure_threshold, 5);
        assert_eq!(restored.checks[0].state, State::Down);
        assert_eq!(restored.checks[1].state, State::Down);
        assert_eq!(restored.checks[2].state, State::Unknown);
        assert!(legacy.is_empty());
    }

    #[test]
    fn restore_unreadable() {
        let mut legacy = BTreeMap::new();
        let restored = Project::restore(
            config::ProjectConfig {
                project: "project".into(),
                checks: vec![new_check(1).config],
            },
            Some(
                serde_json::json!({"state": "sideways", "checks": [{"config": {"name": "test"}}]}),
            ),
            &mut legacy,
        );
        assert_eq!(restored.state, State::Unknown);
        assert_eq!(restored.checks[0].state, State::Unknown);
    }
}
