//! Module check periodically requests the configured URLs and tracks whether they are up.

use crate::config;
use crate::database;
use crate::email;
use crate::metrics;
use std::sync;
use std::sync::mpsc;
use std::thread;

/// Number of recent check results retained for each check.
const RESULTS_RETENTION: usize = 100;
/// Number of incidents retained for each check.
const INCIDENTS_RETENTION: usize = 20;
/// Number of days of uptime history retained for each check.
const DAYS_RETENTION: usize = 90;

const DB_KEY: &str = "check_manager/checks";

pub struct Manager<'a> {
    hostname: String,
    checks: sync::Mutex<Vec<Check>>,
    db: &'a dyn database::DB,
    notifier: &'a dyn email::Notifier,
    poll_interval: chrono::Duration,
}

impl<'a> Manager<'a> {
    pub fn new(
        db: &'a dyn database::DB,
        notifier: &'a dyn email::Notifier,
        hostname: String,
        checks: Vec<config::CheckConfig>,
        poll_interval: chrono::Duration,
    ) -> Self {
        let mut saved: std::collections::BTreeMap<String, Check> =
            database::get_typed(db, DB_KEY).unwrap().unwrap_or_default();
        let mut checks: Vec<Check> = checks
            .into_iter()
            .map(|check_config| match saved.remove(&check_config.name) {
                None => Check::new(check_config),
                Some(mut check) => {
                    check.config = check_config;
                    check
                }
            })
            .collect();
        checks.sort_by_key(|c| c.config.name.clone());
        let m = metrics::get();
        m.checks_configured.set(checks.len() as i64);
        for check in &checks {
            check.init_metrics();
        }
        Self {
            hostname,
            checks: sync::Mutex::new(checks),
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
    pub fn checks(&self) -> Vec<Check> {
        (*self.checks.lock().unwrap()).clone()
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

            let mut checks = (*self.checks.lock().unwrap()).clone();
            // All checks run in parallel so that one slow URL doesn't delay the others.
            // Each check is bounded by its timeout.
            let results: Vec<(CheckResult, &str)> = thread::scope(|s| {
                let handles: Vec<_> = checks
                    .iter()
                    .map(|check| s.spawn(|| probe(&check.config, &self.hostname)))
                    .collect();
                handles.into_iter().map(|h| h.join().unwrap()).collect()
            });
            let m = metrics::get();
            for (check, (result, result_label)) in checks.iter_mut().zip(results) {
                let name = check.config.name.clone();
                let name = name.as_str();
                m.checks.with_label_values(&[name, result_label]).inc();
                m.check_duration
                    .with_label_values(&[name])
                    .observe(result.latency_ms as f64 / 1000.0);
                if !result.success {
                    eprintln!(
                        "[{}] check failed: {}",
                        check.config.name,
                        result.error.as_deref().unwrap_or("unknown error")
                    );
                }
                if let Some(notification) = check.record(result) {
                    if let Notification::Down { .. } = notification {
                        m.incidents.with_label_values(&[name]).inc();
                    }
                    let (title, body) = self.email(check, &notification);
                    self.notifier.notify(&title, &body);
                }
                check.update_state_metrics();
            }
            let saved: std::collections::BTreeMap<&str, &Check> = checks
                .iter()
                .map(|check| (check.config.name.as_str(), check))
                .collect();
            database::set_typed(self.db, DB_KEY.into(), &saved).unwrap();
            *self.checks.lock().unwrap() = checks;

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

    fn email(&self, check: &Check, notification: &Notification) -> (String, String) {
        let name = &check.config.name;
        let url = &check.config.url;
        let link = format!(
            "https://{}/#/checks/{}/{}",
            self.hostname,
            percent_encode(&self.hostname),
            percent_encode(name)
        );
        match notification {
            Notification::Down { error } => (
                format!("{name} is down"),
                format!(
                    "{url} failed {} consecutive checks.\n\nError: {error}\n\nDetails: {link}",
                    check.consecutive_failures
                ),
            ),
            Notification::Recovered { down_for } => (
                format!("{name} is up again"),
                format!(
                    "{url} is responding again after being down for {}.\n\nDetails: {link}",
                    format_duration(*down_for)
                ),
            ),
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
    let response = match agent.get(&config.url).call() {
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

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct Check {
    config: config::CheckConfig,
    state: State,
    /// When the current state started. For the down state this is the time of the first
    /// failed check, not the time the failure threshold was reached.
    since: Option<chrono::DateTime<chrono::Utc>>,
    consecutive_failures: u32,
    /// Time of the first of the current consecutive failures.
    failing_since: Option<chrono::DateTime<chrono::Utc>>,
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

#[derive(Debug, PartialEq)]
enum Notification {
    Down { error: String },
    Recovered { down_for: chrono::Duration },
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

    /// Creates this check's metrics so that counters start at zero, and sets the gauges
    /// from state loaded from the database.
    fn init_metrics(&self) {
        let m = metrics::get();
        let name = self.config.name.as_str();
        for result in metrics::CHECK_RESULTS {
            m.checks.with_label_values(&[name, result]);
        }
        m.incidents.with_label_values(&[name]);
        self.update_state_metrics();
    }

    fn update_state_metrics(&self) {
        let m = metrics::get();
        let name = self.config.name.as_str();
        match self.state {
            State::Up => m.check_up.with_label_values(&[name]).set(1),
            State::Down => m.check_up.with_label_values(&[name]).set(0),
            State::Unknown => {
                let _ = m.check_up.remove_label_values(&[name]);
            }
        }
        m.check_consecutive_failures
            .with_label_values(&[name])
            .set(self.consecutive_failures as i64);
        if let Some(since) = self.since {
            metrics::set_timestamp(&m.check_state_since.with_label_values(&[name]), since);
        }
    }

    /// Records the result of a check, returning a notification to send if the state changed
    /// from up to down or vice versa.
    fn record(&mut self, result: CheckResult) -> Option<Notification> {
        let now = result.time;
        let date = now.date_naive();
        if self.days.first().map(|day| day.date) != Some(date) {
            self.days.insert(
                0,
                Day {
                    date,
                    checks: 0,
                    failures: 0,
                    down: false,
                },
            );
            self.days.truncate(DAYS_RETENTION);
        }
        self.days[0].checks += 1;

        let notification = if result.success {
            self.consecutive_failures = 0;
            self.failing_since = None;
            let previous_state = self.state;
            if previous_state != State::Up {
                self.state = State::Up;
                self.since = Some(now);
            }
            match previous_state {
                State::Down => {
                    let incident = &mut self.incidents[0];
                    incident.end = Some(now);
                    Some(Notification::Recovered {
                        down_for: now - incident.start,
                    })
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
                        error: error.clone(),
                    },
                );
                self.incidents.truncate(INCIDENTS_RETENTION);
                Some(Notification::Down { error })
            } else {
                None
            }
        };
        if self.state == State::Down {
            self.days[0].down = true;
        }
        self.results.insert(0, result);
        self.results.truncate(RESULTS_RETENTION);
        notification
    }
}

fn format_duration(d: chrono::Duration) -> String {
    let minutes = d.num_minutes();
    if minutes < 1 {
        format!("{}s", d.num_seconds())
    } else if minutes < 60 {
        format!("{minutes}m")
    } else if minutes < 60 * 24 {
        format!("{}h {}m", minutes / 60, minutes % 60)
    } else {
        format!("{}d {}h", minutes / (60 * 24), (minutes / 60) % 24)
    }
}

/// Percent-encodes everything except unreserved characters, matching JavaScript's
/// encodeURIComponent closely enough for links to the status page.
fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_check(failure_threshold: u32) -> Check {
        Check::new(config::CheckConfig {
            name: "test".into(),
            url: "https://example.com".into(),
            view: config::View::Status,
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
        assert_eq!(
            check.record(result(3, false)),
            Some(Notification::Down {
                error: "HTTP 502".into()
            })
        );
        assert_eq!(check.state, State::Down);
        assert_eq!(check.since, Some(time(1)));
        assert_eq!(check.incidents.len(), 1);
        assert_eq!(check.incidents[0].start, time(1));
        assert_eq!(check.incidents[0].end, None);

        // Further failures don't send more emails.
        assert_eq!(check.record(result(4, false)), None);

        assert_eq!(
            check.record(result(5, true)),
            Some(Notification::Recovered {
                down_for: chrono::Duration::minutes(4)
            })
        );
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
        assert_eq!(
            check.record(result(0, false)),
            Some(Notification::Down {
                error: "HTTP 502".into()
            })
        );
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

    #[test]
    fn encode() {
        assert_eq!(percent_encode("a.b-c (d)/é"), "a.b-c%20%28d%29%2F%C3%A9");
    }
}
