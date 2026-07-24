//! In-process metrics: atomic counters and latency histograms for HTTP
//! requests, payment settlement, webhook delivery, and auth decisions.
//!
//! All types are cheaply clonable (backed by `Arc`-wrapped atomics) so they
//! can be stored on `AppState` and shared across handlers and background tasks
//! without additional synchronisation.
//!
//! ## Exposition
//! `GET /metrics` returns a plain-text Prometheus-compatible snapshot so any
//! standard scraper can ingest the data with zero configuration.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Histogram buckets for webhook delivery latency (milliseconds).
/// Covers the range from sub-10 ms fast paths up to the 10 s default timeout.
const LATENCY_BUCKETS_MS: &[u64] = &[10, 50, 100, 250, 500, 1_000, 2_500, 5_000, 10_000];

/// Histogram buckets for HTTP request latency (milliseconds). API handlers
/// are in-process DB work, not outbound calls, so this is scaled well below
/// the webhook bucket set above.
const REQUEST_LATENCY_BUCKETS_MS: &[u64] = &[5, 10, 25, 50, 100, 250, 500, 1_000, 2_500];

/// Histogram buckets for settlement latency (seconds) — the time from a
/// payment intent's `created_at` to it reaching a terminal state. Spans from
/// near-instant stream settlement up to the default 1h payment TTL.
const SETTLEMENT_LATENCY_BUCKETS_SECS: &[u64] = &[1, 5, 10, 30, 60, 300, 900, 1_800, 3_600];

/// Seconds elapsed between `created_at` (an RFC 3339 timestamp, as stored in
/// `payments.created_at`) and now. Returns `None` if the timestamp can't be
/// parsed — never expected in practice, but settlement metrics are
/// best-effort observability and must never be the reason a settlement fails.
pub fn seconds_since_rfc3339(created_at: &str) -> Option<u64> {
    let then = time::OffsetDateTime::parse(created_at, &time::format_description::well_known::Rfc3339)
        .ok()?;
    let elapsed = time::OffsetDateTime::now_utc() - then;
    Some(elapsed.whole_seconds().max(0) as u64)
}

#[derive(Clone)]
pub struct WebhookMetrics {
    inner: Arc<WebhookMetricsInner>,
}

struct WebhookMetricsInner {
    /// Deliveries that reached the endpoint and received a 2xx response.
    delivered: AtomicU64,
    /// Deliveries that exhausted all retry attempts without a success.
    failed: AtomicU64,
    /// Individual retry attempts (i.e. attempts after the first try).
    retried: AtomicU64,
    /// Sum of all delivery latencies in milliseconds (for computing mean).
    latency_sum_ms: AtomicU64,
    /// Total completed delivery attempts (for mean denominator).
    latency_count: AtomicU64,
    /// Per-bucket counts. Index `i` corresponds to `LATENCY_BUCKETS_MS[i]`;
    /// the last slot is the `+Inf` bucket.
    latency_buckets: [AtomicU64; 10],
}

impl Default for WebhookMetricsInner {
    fn default() -> Self {
        Self {
            delivered: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            retried: AtomicU64::new(0),
            latency_sum_ms: AtomicU64::new(0),
            latency_count: AtomicU64::new(0),
            // 9 explicit buckets + 1 +Inf = 10 slots
            latency_buckets: [
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
            ],
        }
    }
}

impl WebhookMetrics {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(WebhookMetricsInner::default()),
        }
    }

    /// Record a successful delivery (2xx response received).
    pub fn record_delivered(&self) {
        self.inner.delivered.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a final delivery failure (all retries exhausted without success).
    pub fn record_failed(&self) {
        self.inner.failed.fetch_add(1, Ordering::Relaxed);
    }

    /// Record one retry attempt (every attempt after the first try).
    pub fn record_retry(&self) {
        self.inner.retried.fetch_add(1, Ordering::Relaxed);
    }

    /// Record the end-to-end latency for one delivery, in milliseconds.
    ///
    /// Histogram buckets are cumulative: a 75 ms observation increments every
    /// bucket whose `le` bound is ≥ 75 (i.e. `le="100"`, `le="250"`, …
    /// `le="+Inf"`), matching the Prometheus exposition format.
    pub fn record_latency_ms(&self, ms: u64) {
        self.inner.latency_sum_ms.fetch_add(ms, Ordering::Relaxed);
        self.inner.latency_count.fetch_add(1, Ordering::Relaxed);
        for (i, &bound) in LATENCY_BUCKETS_MS.iter().enumerate() {
            if ms <= bound {
                self.inner.latency_buckets[i].fetch_add(1, Ordering::Relaxed);
            }
        }
        // +Inf bucket is always incremented.
        self.inner.latency_buckets[LATENCY_BUCKETS_MS.len()].fetch_add(1, Ordering::Relaxed);
    }

    // ── Snapshot accessors ────────────────────────────────────────────────

    pub fn delivered(&self) -> u64 {
        self.inner.delivered.load(Ordering::Relaxed)
    }
    pub fn failed(&self) -> u64 {
        self.inner.failed.load(Ordering::Relaxed)
    }
    pub fn retried(&self) -> u64 {
        self.inner.retried.load(Ordering::Relaxed)
    }
    pub fn latency_sum_ms(&self) -> u64 {
        self.inner.latency_sum_ms.load(Ordering::Relaxed)
    }
    pub fn latency_count(&self) -> u64 {
        self.inner.latency_count.load(Ordering::Relaxed)
    }
    pub fn latency_bucket(&self, i: usize) -> u64 {
        self.inner.latency_buckets[i].load(Ordering::Relaxed)
    }
}

impl Default for WebhookMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Outcome counters for `auth_middleware`, so credential-stuffing or
/// misconfigured-client traffic is visible in the `/metrics` scrape rather
/// than only in logs.
#[derive(Clone)]
pub struct AuthMetrics {
    inner: Arc<AuthMetricsInner>,
}

struct AuthMetricsInner {
    /// Requests that presented a valid API key.
    success: AtomicU64,
    /// Requests with no (or a malformed) `Authorization: Bearer` header.
    failure_missing_key: AtomicU64,
    /// Requests with a well-formed key that didn't match any merchant.
    failure_invalid_key: AtomicU64,
    /// Requests that failed the key lookup itself (database error).
    failure_internal_error: AtomicU64,
}

impl Default for AuthMetricsInner {
    fn default() -> Self {
        Self {
            success: AtomicU64::new(0),
            failure_missing_key: AtomicU64::new(0),
            failure_invalid_key: AtomicU64::new(0),
            failure_internal_error: AtomicU64::new(0),
        }
    }
}

impl AuthMetrics {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(AuthMetricsInner::default()),
        }
    }

    pub fn record_success(&self) {
        self.inner.success.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_failure_missing_key(&self) {
        self.inner.failure_missing_key.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_failure_invalid_key(&self) {
        self.inner.failure_invalid_key.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_failure_internal_error(&self) {
        self.inner
            .failure_internal_error
            .fetch_add(1, Ordering::Relaxed);
    }

    // ── Snapshot accessors ────────────────────────────────────────────────

    pub fn success(&self) -> u64 {
        self.inner.success.load(Ordering::Relaxed)
    }
    pub fn failure_missing_key(&self) -> u64 {
        self.inner.failure_missing_key.load(Ordering::Relaxed)
    }
    pub fn failure_invalid_key(&self) -> u64 {
        self.inner.failure_invalid_key.load(Ordering::Relaxed)
    }
    pub fn failure_internal_error(&self) -> u64 {
        self.inner.failure_internal_error.load(Ordering::Relaxed)
    }
}

impl Default for AuthMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// HTTP request counters and a latency histogram, so throughput and error
/// rates are visible without parsing logs (issue #133).
///
/// Counted by `(method, route, status)`, where `route` is the matched route
/// *template* (e.g. `/payments/:id`), not the raw request path — this keeps
/// the label set bounded regardless of how many distinct ids clients request.
/// Requests that matched no route (404s on arbitrary paths) are counted under
/// the fixed `"unmatched"` route for the same reason.
#[derive(Clone)]
pub struct RequestMetrics {
    inner: Arc<RequestMetricsInner>,
}

struct RequestMetricsInner {
    counts: Mutex<HashMap<(String, String, u16), u64>>,
    latency_sum_ms: AtomicU64,
    latency_count: AtomicU64,
    latency_buckets: [AtomicU64; REQUEST_LATENCY_BUCKETS_MS.len() + 1],
}

impl RequestMetrics {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RequestMetricsInner {
                counts: Mutex::new(HashMap::new()),
                latency_sum_ms: AtomicU64::new(0),
                latency_count: AtomicU64::new(0),
                latency_buckets: Default::default(),
            }),
        }
    }

    /// Record one completed request. `route` should be a matched-path
    /// template, not a raw path, to keep cardinality bounded.
    pub fn record(&self, method: &str, route: &str, status: u16) {
        let mut counts = self.inner.counts.lock().unwrap();
        *counts
            .entry((method.to_string(), route.to_string(), status))
            .or_insert(0) += 1;
    }

    pub fn record_latency_ms(&self, ms: u64) {
        self.inner.latency_sum_ms.fetch_add(ms, Ordering::Relaxed);
        self.inner.latency_count.fetch_add(1, Ordering::Relaxed);
        for (i, &bound) in REQUEST_LATENCY_BUCKETS_MS.iter().enumerate() {
            if ms <= bound {
                self.inner.latency_buckets[i].fetch_add(1, Ordering::Relaxed);
            }
        }
        self.inner.latency_buckets[REQUEST_LATENCY_BUCKETS_MS.len()]
            .fetch_add(1, Ordering::Relaxed);
    }

    // ── Snapshot accessors ────────────────────────────────────────────────

    /// Snapshot of `(method, route, status) -> count`. Cloned out from under
    /// the lock so rendering never holds it.
    pub fn counts(&self) -> HashMap<(String, String, u16), u64> {
        self.inner.counts.lock().unwrap().clone()
    }
    pub fn latency_sum_ms(&self) -> u64 {
        self.inner.latency_sum_ms.load(Ordering::Relaxed)
    }
    pub fn latency_count(&self) -> u64 {
        self.inner.latency_count.load(Ordering::Relaxed)
    }
    pub fn latency_bucket(&self, i: usize) -> u64 {
        self.inner.latency_buckets[i].load(Ordering::Relaxed)
    }
}

impl Default for RequestMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Payment settlement outcome counters and a latency histogram (time from a
/// payment intent's `created_at` to it reaching a terminal state), so
/// throughput and detection latency are observable without parsing logs
/// (issue #133).
#[derive(Clone)]
pub struct SettlementMetrics {
    inner: Arc<SettlementMetricsInner>,
}

struct SettlementMetricsInner {
    completed: AtomicU64,
    overpaid: AtomicU64,
    underpaid: AtomicU64,
    expired: AtomicU64,
    latency_sum_secs: AtomicU64,
    latency_count: AtomicU64,
    latency_buckets: [AtomicU64; SETTLEMENT_LATENCY_BUCKETS_SECS.len() + 1],
}

impl Default for SettlementMetricsInner {
    fn default() -> Self {
        Self {
            completed: AtomicU64::new(0),
            overpaid: AtomicU64::new(0),
            underpaid: AtomicU64::new(0),
            expired: AtomicU64::new(0),
            latency_sum_secs: AtomicU64::new(0),
            latency_count: AtomicU64::new(0),
            latency_buckets: Default::default(),
        }
    }
}

impl SettlementMetrics {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(SettlementMetricsInner::default()),
        }
    }

    pub fn record_completed(&self) {
        self.inner.completed.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_overpaid(&self) {
        self.inner.overpaid.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_underpaid(&self) {
        self.inner.underpaid.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_expired(&self) {
        self.inner.expired.fetch_add(1, Ordering::Relaxed);
    }

    /// Record settlement latency in seconds (`settled_at - created_at`).
    pub fn record_latency_secs(&self, secs: u64) {
        self.inner
            .latency_sum_secs
            .fetch_add(secs, Ordering::Relaxed);
        self.inner.latency_count.fetch_add(1, Ordering::Relaxed);
        for (i, &bound) in SETTLEMENT_LATENCY_BUCKETS_SECS.iter().enumerate() {
            if secs <= bound {
                self.inner.latency_buckets[i].fetch_add(1, Ordering::Relaxed);
            }
        }
        self.inner.latency_buckets[SETTLEMENT_LATENCY_BUCKETS_SECS.len()]
            .fetch_add(1, Ordering::Relaxed);
    }

    // ── Snapshot accessors ────────────────────────────────────────────────

    pub fn completed(&self) -> u64 {
        self.inner.completed.load(Ordering::Relaxed)
    }
    pub fn overpaid(&self) -> u64 {
        self.inner.overpaid.load(Ordering::Relaxed)
    }
    pub fn underpaid(&self) -> u64 {
        self.inner.underpaid.load(Ordering::Relaxed)
    }
    pub fn expired(&self) -> u64 {
        self.inner.expired.load(Ordering::Relaxed)
    }
    pub fn latency_sum_secs(&self) -> u64 {
        self.inner.latency_sum_secs.load(Ordering::Relaxed)
    }
    pub fn latency_count(&self) -> u64 {
        self.inner.latency_count.load(Ordering::Relaxed)
    }
    pub fn latency_bucket(&self, i: usize) -> u64 {
        self.inner.latency_buckets[i].load(Ordering::Relaxed)
    }
}

impl Default for SettlementMetrics {
    fn default() -> Self {
        Self::new()
    }
}

// ── Prometheus text exposition ────────────────────────────────────────────────

/// Render webhook delivery, auth outcome, HTTP request, and settlement
/// metrics as a Prometheus-compatible plain-text snapshot. Called by
/// `GET /metrics`.
pub fn render(
    webhook: &WebhookMetrics,
    auth: &AuthMetrics,
    request: &RequestMetrics,
    settlement: &SettlementMetrics,
) -> String {
    let mut out = String::with_capacity(1024);

    // stellargate_webhook_deliveries_total — counter vec by outcome
    out.push_str(
        "# HELP stellargate_webhook_deliveries_total Total webhook delivery attempts by outcome.\n",
    );
    out.push_str("# TYPE stellargate_webhook_deliveries_total counter\n");
    out.push_str(&format!(
        "stellargate_webhook_deliveries_total{{outcome=\"delivered\"}} {}\n",
        webhook.delivered()
    ));
    out.push_str(&format!(
        "stellargate_webhook_deliveries_total{{outcome=\"failed\"}} {}\n",
        webhook.failed()
    ));

    // stellargate_webhook_retries_total — counter
    out.push_str("# HELP stellargate_webhook_retries_total Total webhook retry attempts (excludes the first try).\n");
    out.push_str("# TYPE stellargate_webhook_retries_total counter\n");
    out.push_str(&format!(
        "stellargate_webhook_retries_total {}\n",
        webhook.retried()
    ));

    // stellargate_webhook_delivery_latency_ms — histogram
    out.push_str("# HELP stellargate_webhook_delivery_latency_ms End-to-end webhook delivery latency in milliseconds.\n");
    out.push_str("# TYPE stellargate_webhook_delivery_latency_ms histogram\n");
    for (i, &bound) in LATENCY_BUCKETS_MS.iter().enumerate() {
        out.push_str(&format!(
            "stellargate_webhook_delivery_latency_ms_bucket{{le=\"{}\"}} {}\n",
            bound,
            webhook.latency_bucket(i)
        ));
    }
    out.push_str(&format!(
        "stellargate_webhook_delivery_latency_ms_bucket{{le=\"+Inf\"}} {}\n",
        webhook.latency_bucket(LATENCY_BUCKETS_MS.len())
    ));
    out.push_str(&format!(
        "stellargate_webhook_delivery_latency_ms_sum {}\n",
        webhook.latency_sum_ms()
    ));
    out.push_str(&format!(
        "stellargate_webhook_delivery_latency_ms_count {}\n",
        webhook.latency_count()
    ));

    // stellargate_auth_attempts_total — counter vec by outcome/reason
    out.push_str(
        "# HELP stellargate_auth_attempts_total Total auth middleware decisions by outcome and reason.\n",
    );
    out.push_str("# TYPE stellargate_auth_attempts_total counter\n");
    out.push_str(&format!(
        "stellargate_auth_attempts_total{{outcome=\"success\"}} {}\n",
        auth.success()
    ));
    out.push_str(&format!(
        "stellargate_auth_attempts_total{{outcome=\"failure\",reason=\"missing_key\"}} {}\n",
        auth.failure_missing_key()
    ));
    out.push_str(&format!(
        "stellargate_auth_attempts_total{{outcome=\"failure\",reason=\"invalid_key\"}} {}\n",
        auth.failure_invalid_key()
    ));
    out.push_str(&format!(
        "stellargate_auth_attempts_total{{outcome=\"failure\",reason=\"internal_error\"}} {}\n",
        auth.failure_internal_error()
    ));

    // stellargate_http_requests_total — counter vec by method/route/status
    out.push_str(
        "# HELP stellargate_http_requests_total Total HTTP requests by method, matched route, and status code.\n",
    );
    out.push_str("# TYPE stellargate_http_requests_total counter\n");
    let mut counts: Vec<((String, String, u16), u64)> = request.counts().into_iter().collect();
    counts.sort();
    for ((method, route, status), count) in counts {
        out.push_str(&format!(
            "stellargate_http_requests_total{{method=\"{method}\",route=\"{route}\",status=\"{status}\"}} {count}\n"
        ));
    }

    // stellargate_http_request_duration_ms — histogram
    out.push_str("# HELP stellargate_http_request_duration_ms HTTP request latency in milliseconds.\n");
    out.push_str("# TYPE stellargate_http_request_duration_ms histogram\n");
    for (i, &bound) in REQUEST_LATENCY_BUCKETS_MS.iter().enumerate() {
        out.push_str(&format!(
            "stellargate_http_request_duration_ms_bucket{{le=\"{}\"}} {}\n",
            bound,
            request.latency_bucket(i)
        ));
    }
    out.push_str(&format!(
        "stellargate_http_request_duration_ms_bucket{{le=\"+Inf\"}} {}\n",
        request.latency_bucket(REQUEST_LATENCY_BUCKETS_MS.len())
    ));
    out.push_str(&format!(
        "stellargate_http_request_duration_ms_sum {}\n",
        request.latency_sum_ms()
    ));
    out.push_str(&format!(
        "stellargate_http_request_duration_ms_count {}\n",
        request.latency_count()
    ));

    // stellargate_payments_settled_total — counter vec by outcome
    out.push_str(
        "# HELP stellargate_payments_settled_total Total payment intents reaching a terminal state, by outcome.\n",
    );
    out.push_str("# TYPE stellargate_payments_settled_total counter\n");
    out.push_str(&format!(
        "stellargate_payments_settled_total{{outcome=\"completed\"}} {}\n",
        settlement.completed()
    ));
    out.push_str(&format!(
        "stellargate_payments_settled_total{{outcome=\"overpaid\"}} {}\n",
        settlement.overpaid()
    ));
    out.push_str(&format!(
        "stellargate_payments_settled_total{{outcome=\"underpaid\"}} {}\n",
        settlement.underpaid()
    ));
    out.push_str(&format!(
        "stellargate_payments_settled_total{{outcome=\"expired\"}} {}\n",
        settlement.expired()
    ));

    // stellargate_payment_settlement_latency_seconds — histogram
    out.push_str("# HELP stellargate_payment_settlement_latency_seconds Time from a payment intent's creation to it reaching a terminal state, in seconds.\n");
    out.push_str("# TYPE stellargate_payment_settlement_latency_seconds histogram\n");
    for (i, &bound) in SETTLEMENT_LATENCY_BUCKETS_SECS.iter().enumerate() {
        out.push_str(&format!(
            "stellargate_payment_settlement_latency_seconds_bucket{{le=\"{}\"}} {}\n",
            bound,
            settlement.latency_bucket(i)
        ));
    }
    out.push_str(&format!(
        "stellargate_payment_settlement_latency_seconds_bucket{{le=\"+Inf\"}} {}\n",
        settlement.latency_bucket(SETTLEMENT_LATENCY_BUCKETS_SECS.len())
    ));
    out.push_str(&format!(
        "stellargate_payment_settlement_latency_seconds_sum {}\n",
        settlement.latency_sum_secs()
    ));
    out.push_str(&format!(
        "stellargate_payment_settlement_latency_seconds_count {}\n",
        settlement.latency_count()
    ));

    out
}
