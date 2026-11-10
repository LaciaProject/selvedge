//! Process metrics in the Prometheus text format, served on
//! `--metrics-listen` (`GET /metrics`).
//!
//! A deliberately small registry: counters, gauges and histograms with fixed
//! label names. No metric carries an account id, so cardinality stays bounded
//! however many tenants there are.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Mutex;
use std::time::Duration;

/// Latency buckets, in seconds.
const BUCKETS: &[f64] = &[0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0];

#[derive(Default)]
struct Hist {
    counts: Vec<u64>,
    sum: f64,
    count: u64,
}

enum Values {
    Counter(BTreeMap<Vec<String>, u64>),
    Gauge(BTreeMap<Vec<String>, i64>),
    Histogram(BTreeMap<Vec<String>, Hist>),
}

/// One metric and its series, one per combination of label values.
pub struct Family {
    name: &'static str,
    help: &'static str,
    labels: &'static [&'static str],
    values: Mutex<Values>,
}

impl Family {
    const fn new(name: &'static str, help: &'static str, labels: &'static [&'static str], v: Values) -> Self {
        Self { name, help, labels, values: Mutex::new(v) }
    }

    const fn counter(name: &'static str, help: &'static str, labels: &'static [&'static str]) -> Self {
        Self::new(name, help, labels, Values::Counter(BTreeMap::new()))
    }

    const fn gauge(name: &'static str, help: &'static str, labels: &'static [&'static str]) -> Self {
        Self::new(name, help, labels, Values::Gauge(BTreeMap::new()))
    }

    const fn histogram(name: &'static str, help: &'static str, labels: &'static [&'static str]) -> Self {
        Self::new(name, help, labels, Values::Histogram(BTreeMap::new()))
    }

    fn key(&self, labels: &[&str]) -> Vec<String> {
        debug_assert_eq!(labels.len(), self.labels.len(), "{}", self.name);
        labels.iter().map(|l| (*l).to_owned()).collect()
    }

    /// Adds to a counter.
    pub fn inc_by(&self, labels: &[&str], by: u64) {
        let key = self.key(labels);
        if let Values::Counter(m) = &mut *self.values.lock().expect("poisoned") {
            *m.entry(key).or_default() += by;
        }
    }

    pub fn inc(&self, labels: &[&str]) {
        self.inc_by(labels, 1);
    }

    /// Sets a gauge.
    pub fn set(&self, labels: &[&str], value: i64) {
        let key = self.key(labels);
        if let Values::Gauge(m) = &mut *self.values.lock().expect("poisoned") {
            m.insert(key, value);
        }
    }

    /// Adds to a gauge (negative to subtract).
    pub fn add(&self, labels: &[&str], by: i64) {
        let key = self.key(labels);
        if let Values::Gauge(m) = &mut *self.values.lock().expect("poisoned") {
            *m.entry(key).or_default() += by;
        }
    }

    /// Records a duration in a histogram.
    pub fn observe(&self, labels: &[&str], took: Duration) {
        let key = self.key(labels);
        let secs = took.as_secs_f64();
        if let Values::Histogram(m) = &mut *self.values.lock().expect("poisoned") {
            let h =
                m.entry(key).or_insert_with(|| Hist { counts: vec![0; BUCKETS.len()], ..Hist::default() });
            for (i, le) in BUCKETS.iter().enumerate() {
                if secs <= *le {
                    h.counts[i] += 1;
                }
            }
            h.sum += secs;
            h.count += 1;
        }
    }

    fn render(&self, out: &mut String) {
        let values = self.values.lock().expect("poisoned");
        let kind = match &*values {
            Values::Counter(_) => "counter",
            Values::Gauge(_) => "gauge",
            Values::Histogram(_) => "histogram",
        };
        let _ = writeln!(out, "# HELP {} {}", self.name, self.help);
        let _ = writeln!(out, "# TYPE {} {kind}", self.name);
        let labels = |key: &[String], extra: Option<(&str, String)>| -> String {
            let mut parts: Vec<String> =
                self.labels.iter().zip(key).map(|(n, v)| format!("{n}=\"{}\"", escape(v))).collect();
            if let Some((n, v)) = extra {
                parts.push(format!("{n}=\"{v}\""));
            }
            if parts.is_empty() { String::new() } else { format!("{{{}}}", parts.join(",")) }
        };
        match &*values {
            Values::Counter(m) => {
                for (k, v) in m {
                    let _ = writeln!(out, "{}{} {v}", self.name, labels(k, None));
                }
            }
            Values::Gauge(m) => {
                for (k, v) in m {
                    let _ = writeln!(out, "{}{} {v}", self.name, labels(k, None));
                }
            }
            Values::Histogram(m) => {
                for (k, h) in m {
                    for (le, c) in BUCKETS.iter().zip(&h.counts) {
                        let _ = writeln!(
                            out,
                            "{}_bucket{} {c}",
                            self.name,
                            labels(k, Some(("le", le.to_string())))
                        );
                    }
                    let _ = writeln!(
                        out,
                        "{}_bucket{} {}",
                        self.name,
                        labels(k, Some(("le", "+Inf".into()))),
                        h.count
                    );
                    let _ = writeln!(out, "{}_sum{} {}", self.name, labels(k, None), h.sum);
                    let _ = writeln!(out, "{}_count{} {}", self.name, labels(k, None), h.count);
                }
            }
        }
    }
}

fn escape(v: &str) -> String {
    v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

/// Requests a component served, by status class (`2xx`, ...).
pub static HTTP_REQUESTS: Family = Family::counter(
    "selvedge_http_requests_total",
    "Requests served, by component and status class.",
    &["component", "code"],
);
/// Time to the response's headers.
pub static HTTP_DURATION: Family = Family::histogram(
    "selvedge_http_request_duration_seconds",
    "Time from a request's arrival to its response headers, by component.",
    &["component"],
);
pub static GATEWAY_RUNNER_ERRORS: Family = Family::counter(
    "selvedge_gateway_runner_errors_total",
    "Requests a runner could not take: unreachable (tried the next runner) or failed after sending.",
    &["kind"],
);
pub static GATEWAY_HEALTHY_RUNNERS: Family =
    Family::gauge("selvedge_gateway_healthy_runners", "Runners passing the gateway's health checks.", &[]);
pub static CONNECTIONS_OPENED: Family = Family::counter(
    "selvedge_connections_opened_total",
    "Connections opened to other components, by the server's role. Kept connections are reused.",
    &["role"],
);
pub static AGENT_PROCESSES: Family =
    Family::gauge("selvedge_agent_account_processes", "Account workerd processes running.", &[]);
pub static AGENT_PROCESS_STARTS: Family = Family::counter(
    "selvedge_agent_process_starts_total",
    "Account workerd processes started (first use, deploy, or after a stop).",
    &[],
);
pub static AGENT_PROCESS_START_DURATION: Family = Family::histogram(
    "selvedge_agent_process_start_duration_seconds",
    "Time to start an account process, from fetching its bundle to its sockets accepting.",
    &[],
);
pub static AGENT_PROCESS_STOPS: Family = Family::counter(
    "selvedge_agent_process_stops_total",
    "Account processes stopped, by reason: replaced (deploy), idle, memory, hung, removed, shutdown.",
    &["reason"],
);
pub static AGENT_PROCESS_EXITS: Family = Family::counter(
    "selvedge_agent_process_exits_total",
    "Account processes found exited on their own (a crash or a kernel OOM kill).",
    &[],
);
pub static AGENT_CPU_THROTTLED: Family = Family::counter(
    "selvedge_agent_cpu_throttles_total",
    "Times an account process over its CPU share was given a lower priority.",
    &[],
);
pub static AGENT_BINDING_REQUESTS: Family = Family::counter(
    "selvedge_agent_binding_requests_total",
    "Binding calls from workerd sent to the data service, by binding and status class.",
    &["binding", "code"],
);
pub static CONTAINER_INSTANCES: Family =
    Family::gauge("selvedge_container_instances", "Container instances, by state.", &["state"]);
pub static CONTAINER_STARTS: Family = Family::counter(
    "selvedge_container_starts_total",
    "Container instance start requests, by result (created, limit, error).",
    &["result"],
);
pub static CONTAINER_STOPS: Family =
    Family::counter("selvedge_container_stops_total", "Container instances removed, by reason.", &["reason"]);
pub static AGENT_CONTAINER_START_DURATION: Family = Family::histogram(
    "selvedge_agent_container_start_duration_seconds",
    "From workerd asking for a container to its application running.",
    &["result"],
);
pub static AGENT_CONTAINER_UNSUPPORTED: Family = Family::counter(
    "selvedge_agent_container_unsupported_total",
    "Docker API calls from workerd that Selvedge does not implement, by operation.",
    &["operation"],
);
pub static AGENT_CONTAINER_EGRESS_REFUSED: Family = Family::counter(
    "selvedge_agent_container_egress_refused_total",
    "Connections to the egress relay from addresses that are not this runner's containers.",
    &[],
);

static ALL: &[&Family] = &[
    &HTTP_REQUESTS,
    &HTTP_DURATION,
    &GATEWAY_RUNNER_ERRORS,
    &GATEWAY_HEALTHY_RUNNERS,
    &CONNECTIONS_OPENED,
    &AGENT_PROCESSES,
    &AGENT_PROCESS_STARTS,
    &AGENT_PROCESS_START_DURATION,
    &AGENT_PROCESS_STOPS,
    &AGENT_PROCESS_EXITS,
    &AGENT_CPU_THROTTLED,
    &AGENT_BINDING_REQUESTS,
    &CONTAINER_INSTANCES,
    &CONTAINER_STARTS,
    &CONTAINER_STOPS,
    &AGENT_CONTAINER_START_DURATION,
    &AGENT_CONTAINER_UNSUPPORTED,
    &AGENT_CONTAINER_EGRESS_REFUSED,
];

/// A status's class label: `2xx`, `4xx`, ...
pub fn code_class(status: http::StatusCode) -> &'static str {
    match status.as_u16() {
        100..=199 => "1xx",
        200..=299 => "2xx",
        300..=399 => "3xx",
        400..=499 => "4xx",
        _ => "5xx",
    }
}

/// Records a served request.
pub fn request(component: &str, status: http::StatusCode, took: Duration) {
    HTTP_REQUESTS.inc(&[component, code_class(status)]);
    HTTP_DURATION.observe(&[component], took);
}

/// Every metric, in the Prometheus text format.
pub fn render() -> String {
    let mut out = String::new();
    for f in ALL {
        f.render(&mut out);
    }
    out
}

/// An axum layer recording each request of `component`.
pub async fn layer(
    component: &'static str,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    // Health probes would drown out the traffic.
    if req.uri().path().ends_with("/healthz") {
        return next.run(req).await;
    }
    let start = std::time::Instant::now();
    let resp = next.run(req).await;
    request(component, resp.status(), start.elapsed());
    resp
}

/// Serves `GET /metrics` on `listen` until the process ends.
pub async fn serve(listen: std::net::SocketAddr) -> anyhow::Result<()> {
    let app = axum::Router::new().route(
        "/metrics",
        axum::routing::get(|| async {
            ([(http::header::CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")], render())
        }),
    );
    let listener = tokio::net::TcpListener::bind(listen).await?;
    tracing::info!(addr = %listen, "metrics listening");
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_the_text_format() {
        static C: Family = Family::counter("t_total", "A test counter.", &["code"]);
        static H: Family = Family::histogram("t_seconds", "A test histogram.", &[]);
        static G: Family = Family::gauge("t_gauge", "A test gauge.", &[]);
        C.inc(&["2xx"]);
        C.inc_by(&["2\"x"], 2);
        H.observe(&[], Duration::from_millis(3));
        G.add(&[], 3);
        G.add(&[], -1);
        let mut out = String::new();
        for f in [&C, &H, &G] {
            f.render(&mut out);
        }
        assert!(out.contains("# TYPE t_total counter\n"), "{out}");
        assert!(out.contains("t_total{code=\"2xx\"} 1\n"), "{out}");
        assert!(out.contains("t_total{code=\"2\\\"x\"} 2\n"), "{out}");
        assert!(out.contains("t_seconds_bucket{le=\"0.0025\"} 0\n"), "{out}");
        assert!(out.contains("t_seconds_bucket{le=\"0.005\"} 1\n"), "{out}");
        assert!(out.contains("t_seconds_bucket{le=\"+Inf\"} 1\n"), "{out}");
        assert!(out.contains("t_seconds_count 1\n"), "{out}");
        assert!(out.contains("t_gauge 2\n"), "{out}");
    }
}
