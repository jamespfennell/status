//! Prometheus metrics for the agent.
//!
//! Metrics are registered in a process-wide registry and served at /metrics by the HTTP service.

use std::sync::LazyLock;

use prometheus::{
    Encoder, Gauge, Histogram, HistogramOpts, HistogramVec, IntCounterVec, IntGauge, IntGaugeVec,
    Opts, Registry,
};

/// Values of the `result` label of `status_checks_total`.
pub const CHECK_RESULTS: [&str; 7] = [
    "success",
    "http_status",
    "content",
    "timeout",
    "dns",
    "connection",
    "other",
];

pub struct Metrics {
    registry: Registry,

    pub check_up: IntGaugeVec,
    pub checks: IntCounterVec,
    pub check_duration: HistogramVec,
    pub check_consecutive_failures: IntGaugeVec,
    pub incidents: IntCounterVec,
    pub check_state_since: prometheus::GaugeVec,

    pub poll_loop_duration: Histogram,
    pub poll_loop_last_run: Gauge,
    pub notifications: IntCounterVec,
    pub build_info: IntGaugeVec,
    pub checks_configured: IntGauge,
}

static METRICS: LazyLock<Metrics> = LazyLock::new(Metrics::new);

/// Returns the process-wide metrics.
pub fn get() -> &'static Metrics {
    &METRICS
}

impl Metrics {
    fn new() -> Self {
        let registry = Registry::new();
        let m = Self {
            check_up: IntGaugeVec::new(
                Opts::new(
                    "status_check_up",
                    "1 if the check is up, 0 if it is down. Absent while the state is unknown.",
                ),
                &["check"],
            )
            .unwrap(),
            checks: IntCounterVec::new(
                Opts::new(
                    "status_checks_total",
                    "Checks run, by result (success, http_status, content, timeout, dns, connection, other).",
                ),
                &["check", "result"],
            )
            .unwrap(),
            check_duration: HistogramVec::new(
                HistogramOpts::new(
                    "status_check_duration_seconds",
                    "Time taken to request the URL, including failed requests.",
                )
                .buckets(vec![
                    0.05, 0.1, 0.2, 0.3, 0.5, 0.75, 1.0, 2.0, 5.0, 10.0,
                ]),
                &["check"],
            )
            .unwrap(),
            check_consecutive_failures: IntGaugeVec::new(
                Opts::new(
                    "status_check_consecutive_failures",
                    "Number of consecutive failed checks; 0 if the last check passed.",
                ),
                &["check"],
            )
            .unwrap(),
            incidents: IntCounterVec::new(
                Opts::new(
                    "status_incidents_total",
                    "Times the check went down (reached its failure threshold).",
                ),
                &["check"],
            )
            .unwrap(),
            check_state_since: prometheus::GaugeVec::new(
                Opts::new(
                    "status_check_state_since_timestamp_seconds",
                    "Unix time at which the current up or down state started.",
                ),
                &["check"],
            )
            .unwrap(),

            poll_loop_duration: Histogram::with_opts(
                HistogramOpts::new(
                    "status_poll_loop_duration_seconds",
                    "Time taken to run all of the checks once, including sending emails.",
                )
                .buckets(vec![0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0]),
            )
            .unwrap(),
            poll_loop_last_run: Gauge::new(
                "status_poll_loop_last_run_timestamp_seconds",
                "Unix time at which the last run of all of the checks finished.",
            )
            .unwrap(),
            notifications: IntCounterVec::new(
                Opts::new(
                    "status_notifications_total",
                    "Email notifications sent, by result (success, failure).",
                ),
                &["result"],
            )
            .unwrap(),
            build_info: IntGaugeVec::new(
                Opts::new(
                    "status_build_info",
                    "Always 1; labels describe the running build.",
                ),
                &["version"],
            )
            .unwrap(),
            checks_configured: IntGauge::new(
                "status_checks_configured",
                "Number of checks in the config.",
            )
            .unwrap(),

            registry,
        };
        let collectors: Vec<Box<dyn prometheus::core::Collector>> = vec![
            Box::new(m.check_up.clone()),
            Box::new(m.checks.clone()),
            Box::new(m.check_duration.clone()),
            Box::new(m.check_consecutive_failures.clone()),
            Box::new(m.incidents.clone()),
            Box::new(m.check_state_since.clone()),
            Box::new(m.poll_loop_duration.clone()),
            Box::new(m.poll_loop_last_run.clone()),
            Box::new(m.notifications.clone()),
            Box::new(m.build_info.clone()),
            Box::new(m.checks_configured.clone()),
        ];
        for collector in collectors {
            m.registry.register(collector).unwrap();
        }
        m.build_info
            .with_label_values(&[env!("CARGO_PKG_VERSION")])
            .set(1);
        for result in ["success", "failure"] {
            m.notifications.with_label_values(&[result]);
        }
        m
    }

    /// Renders all metrics in the Prometheus text exposition format.
    pub fn render(&self) -> String {
        let mut buffer = vec![];
        prometheus::TextEncoder::new()
            .encode(&self.registry.gather(), &mut buffer)
            .unwrap();
        String::from_utf8(buffer).unwrap()
    }
}

/// Sets a gauge to the given time as a Unix timestamp in seconds.
pub fn set_timestamp(gauge: &Gauge, time: chrono::DateTime<chrono::Utc>) {
    gauge.set(time.timestamp_millis() as f64 / 1000.0);
}
