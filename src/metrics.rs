//! In-process metrics: atomic counters and a latency histogram for webhook
//! delivery outcomes.
//!
//! All types are cheaply clonable (backed by `Arc`-wrapped atomics) so they
//! can be stored on `AppState` and shared across handlers and background tasks
//! without additional synchronisation.
//!
//! ## Exposition
//! `GET /metrics` returns a plain-text Prometheus-compatible snapshot so any
//! standard scraper can ingest the data with zero configuration.

// LABEL SAFETY: All metric labels are restricted to non-sensitive values.
// Allowed: outcome, reason (subsystem names only), method, route (matched
// template only — never the raw request URI), status, task, state, file,
// asset (asset code only, never issuer key material or per-tenant data).
// Forbidden: merchant_id, API keys, internal hostnames, file system paths,
// stack traces, per-tenant identifiers, or any value derived from request
// bodies or URL path parameters.
//
// The `route` label specifically uses the matched axum route pattern
// (e.g. "/v1/payments/:id") and never the raw request URI, so payment IDs,
// merchant IDs, or delivery IDs never appear in metric label values regardless
// of how many unique identifiers flow through the service.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

fn lock_or_recover<'a, T>(mutex: &'a Mutex<T>, what: &str) -> std::sync::MutexGuard<'a, T> {
    mutex.lock().unwrap_or_else(|poisoned| {
        tracing::warn!(mutex = what, "mutex poisoned; recovering and continuing");
        poisoned.into_inner()
    })
}

/// Histogram buckets for webhook delivery latency (milliseconds).
/// Covers the range from sub-10 ms fast paths up to the 10 s default timeout.
const LATENCY_BUCKETS_MS: &[u64] = &[10, 50, 100, 250, 500, 1_000, 2_500, 5_000, 10_000];

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
        self.inner
            .failure_missing_key
            .fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_failure_invalid_key(&self) {
        self.inner
            .failure_invalid_key
            .fetch_add(1, Ordering::Relaxed);
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

/// Counters and gauges for Horizon record handling and poll cycle outcomes.
///
/// All label values (outcome, reason) are fixed subsystem names — never raw
/// URLs, transaction hashes, payment IDs, or any per-tenant data.
#[derive(Clone)]
pub struct HorizonMetrics {
    inner: Arc<HorizonMetricsInner>,
}

#[derive(Default)]
struct HorizonMetricsInner {
    /// Horizon payment records skipped because they carried no usable
    /// `transaction_hash`. A healthy Horizon never produces these, so any
    /// non-zero value means an unexpected payload (a proxy, a mock, a
    /// truncated response) is reaching the reconciler (issue #224).
    unhashed_records_skipped: AtomicU64,
    /// Successful Horizon poll cycles.
    poll_success: AtomicU64,
    /// Poll cycles that hit a Horizon rate limit (429 / 503 with Retry-After).
    poll_rate_limited: AtomicU64,
    /// Poll cycles that failed for any other reason (network error, 5xx, etc).
    poll_error: AtomicU64,
    /// Cursor incidents: three consecutive non-rate-limit 4xx responses from
    /// Horizon for the same cursor position, indicating the cursor is invalid.
    repeated_cursor_4xx: AtomicU64,
    /// Times the Horizon SSE stream listener has reconnected.
    stream_reconnects: AtomicU64,
    /// Age in seconds of the most recently processed Horizon payment record,
    /// as of the last poll or stream event. A store, not an accumulator.
    cursor_age_secs: AtomicU64,
}

impl HorizonMetrics {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(HorizonMetricsInner::default()),
        }
    }

    /// Record one Horizon record skipped for having no transaction hash.
    pub fn record_unhashed_record_skipped(&self) {
        self.inner
            .unhashed_records_skipped
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn unhashed_records_skipped(&self) -> u64 {
        self.inner.unhashed_records_skipped.load(Ordering::Relaxed)
    }

    /// Record a successful Horizon poll cycle.
    pub fn record_success(&self) {
        self.inner.poll_success.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a rate-limited Horizon poll cycle.
    pub fn record_rate_limited(&self) {
        self.inner.poll_rate_limited.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a failed Horizon poll cycle (non-rate-limit error).
    pub fn record_error(&self) {
        self.inner.poll_error.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a repeated-cursor-4xx incident.
    pub fn record_repeated_cursor_4xx(&self) {
        self.inner.repeated_cursor_4xx.fetch_add(1, Ordering::Relaxed);
    }

    /// Record one SSE stream reconnect.
    pub fn record_stream_reconnect(&self) {
        self.inner.stream_reconnects.fetch_add(1, Ordering::Relaxed);
    }

    /// Store (overwrite) the age of the most recently processed Horizon record,
    /// in seconds. This is a gauge — the latest value is what matters.
    pub fn record_cursor_age_secs(&self, age: u64) {
        self.inner.cursor_age_secs.store(age, Ordering::Relaxed);
    }

    // ── Snapshot accessors ────────────────────────────────────────────────

    pub fn success(&self) -> u64 {
        self.inner.poll_success.load(Ordering::Relaxed)
    }
    pub fn rate_limited(&self) -> u64 {
        self.inner.poll_rate_limited.load(Ordering::Relaxed)
    }
    pub fn error(&self) -> u64 {
        self.inner.poll_error.load(Ordering::Relaxed)
    }
    pub fn repeated_cursor_4xx(&self) -> u64 {
        self.inner.repeated_cursor_4xx.load(Ordering::Relaxed)
    }
    pub fn stream_reconnects(&self) -> u64 {
        self.inner.stream_reconnects.load(Ordering::Relaxed)
    }
    pub fn cursor_age_secs(&self) -> u64 {
        self.inner.cursor_age_secs.load(Ordering::Relaxed)
    }
}

impl Default for HorizonMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Histogram buckets for HTTP request latency (milliseconds). Roughly the
/// Prometheus client library defaults (5ms .. 10s), rendered as seconds in
/// the exposition to match the `_seconds` metric name convention.
const HTTP_LATENCY_BUCKETS_MS: &[u64] =
    &[5, 10, 25, 50, 100, 250, 500, 1_000, 2_500, 5_000, 10_000];

/// One route+method's latency distribution.
#[derive(Default, Clone)]
struct RouteLatency {
    sum_ms: u64,
    count: u64,
    /// Cumulative per-bucket counts; one slot past
    /// `HTTP_LATENCY_BUCKETS_MS` for `+Inf`.
    buckets: Vec<u64>,
}

impl RouteLatency {
    fn record(&mut self, elapsed_ms: u64) {
        if self.buckets.is_empty() {
            self.buckets = vec![0; HTTP_LATENCY_BUCKETS_MS.len() + 1];
        }
        self.sum_ms += elapsed_ms;
        self.count += 1;
        for (i, &bound) in HTTP_LATENCY_BUCKETS_MS.iter().enumerate() {
            if elapsed_ms <= bound {
                self.buckets[i] += 1;
            }
        }
        if let Some(last) = self.buckets.last_mut() {
            *last += 1;
        } else {
            self.buckets.push(1);
        }
    }
}

/// HTTP request counters and a latency histogram, labelled by the matched
/// route pattern (e.g. `/v1/payments/:id`) and method — never the raw URI or
/// a path parameter — so cardinality stays bounded by the fixed route table
/// regardless of how many distinct payment or merchant ids are requested.
///
/// A request that hit no route at all (a genuine 404 on an unmapped path) is
/// labelled `<unmatched>` rather than the raw path, for the same reason.
#[derive(Clone)]
pub struct HttpMetrics {
    pub(crate) inner: Arc<Mutex<HttpMetricsInner>>,
}

#[derive(Default)]
struct HttpMetricsInner {
    /// (method, route, status) -> count.
    requests: HashMap<(String, String, u16), u64>,
    /// (method, route) -> latency distribution.
    latency: HashMap<(String, String), RouteLatency>,
}

impl HttpMetrics {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HttpMetricsInner::default())),
        }
    }

    /// Record one completed HTTP request.
    ///
    /// `route` MUST be the matched axum route template (e.g. `/v1/payments/:id`),
    /// never the raw request URI. This is enforced by convention: the HTTP
    /// metrics middleware extracts the route from axum's `MatchedPath` extension,
    /// which only contains the template. Raw URIs contain path parameters
    /// (payment IDs, merchant IDs) that would create unbounded label cardinality
    /// and expose per-tenant identifiers in the metrics scrape.
    pub fn record(&self, method: &str, route: &str, status: u16, elapsed_ms: u64) {
        // A poisoned metrics mutex means a previous worker panicked while the
        // lock was held. Recover the underlying state instead of crashing the
        // process and continue recording metrics.
        let mut inner = lock_or_recover(&self.inner, "http_metrics.inner");
        *inner
            .requests
            .entry((method.to_string(), route.to_string(), status))
            .or_insert(0) += 1;
        inner
            .latency
            .entry((method.to_string(), route.to_string()))
            .or_default()
            .record(elapsed_ms);
    }

    /// Snapshot of request counts, sorted for deterministic exposition.
    fn requests_snapshot(&self) -> Vec<(String, String, u16, u64)> {
        let inner = lock_or_recover(&self.inner, "http_metrics.inner");
        let mut rows: Vec<_> = inner
            .requests
            .iter()
            .map(|((method, route, status), count)| {
                (method.clone(), route.clone(), *status, *count)
            })
            .collect();
        rows.sort_unstable_by(|a, b| (a.0.as_str(), a.1.as_str(), a.2).cmp(&(&b.0, &b.1, b.2)));
        rows
    }

    /// Snapshot of latency distributions, sorted for deterministic exposition.
    fn latency_snapshot(&self) -> Vec<(String, String, RouteLatency)> {
        let inner = lock_or_recover(&self.inner, "http_metrics.inner");
        let mut rows: Vec<_> = inner
            .latency
            .iter()
            .map(|((method, route), lat)| (method.clone(), route.clone(), lat.clone()))
            .collect();
        rows.sort_unstable_by(|a, b| (a.0.as_str(), a.1.as_str()).cmp(&(&b.0, &b.1)));
        rows
    }
}

impl Default for HttpMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Histogram buckets for payment settlement latency (seconds): creation to
/// the poller/stream settling the intent. Spans a few seconds (a fast
/// on-chain confirmation) up to a couple of hours (a slow top-up on an
/// underpaid intent).
const SETTLEMENT_LATENCY_BUCKETS_SECS: &[u64] =
    &[1, 5, 15, 30, 60, 120, 300, 600, 1_800, 3_600, 7_200];

/// Payment lifecycle counters and settlement-latency histogram, so payment
/// creation and settlement throughput/latency are queryable facts on
/// `/metrics` rather than only visible via `GET /payments` polling or log
/// lines (missing-metrics issue).
#[derive(Clone)]
pub struct PaymentMetrics {
    inner: Arc<PaymentMetricsInner>,
}

struct PaymentMetricsInner {
    created: AtomicU64,
    completed: AtomicU64,
    overpaid: AtomicU64,
    underpaid: AtomicU64,
    expired: AtomicU64,
    settlement_latency_sum_secs: AtomicU64,
    settlement_latency_count: AtomicU64,
    settlement_latency_buckets: [AtomicU64; 12],
}

impl Default for PaymentMetricsInner {
    fn default() -> Self {
        Self {
            created: AtomicU64::new(0),
            completed: AtomicU64::new(0),
            overpaid: AtomicU64::new(0),
            underpaid: AtomicU64::new(0),
            expired: AtomicU64::new(0),
            settlement_latency_sum_secs: AtomicU64::new(0),
            settlement_latency_count: AtomicU64::new(0),
            settlement_latency_buckets: Default::default(),
        }
    }
}

impl PaymentMetrics {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(PaymentMetricsInner::default()),
        }
    }

    /// Record a new payment intent (`POST /payments` minting a fresh id, not
    /// an idempotent replay of an existing one).
    pub fn record_created(&self) {
        self.inner.created.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a payment intent swept to `expired` by the sweeper.
    pub fn record_expired(&self) {
        self.inner.expired.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a settlement outcome (`completed`, `overpaid`, or `underpaid`)
    /// and, when known, the elapsed time since the intent was created.
    /// `underpaid` is an intermediate rather than terminal state, but is
    /// still counted here — it is the reconciler's verdict for this
    /// transaction, distinct from the intent's eventual final status.
    pub fn record_settlement(&self, status: &str, latency_secs: Option<i64>) {
        let counter = match status {
            "completed" => &self.inner.completed,
            "overpaid" => &self.inner.overpaid,
            "underpaid" => &self.inner.underpaid,
            _ => return,
        };
        counter.fetch_add(1, Ordering::Relaxed);

        if let Some(secs) = latency_secs {
            // A negative value (clock skew) has no meaningful bucket; floor at 0.
            let secs = secs.max(0) as u64;
            self.inner
                .settlement_latency_sum_secs
                .fetch_add(secs, Ordering::Relaxed);
            self.inner
                .settlement_latency_count
                .fetch_add(1, Ordering::Relaxed);
            for (i, &bound) in SETTLEMENT_LATENCY_BUCKETS_SECS.iter().enumerate() {
                if secs <= bound {
                    self.inner.settlement_latency_buckets[i].fetch_add(1, Ordering::Relaxed);
                }
            }
            self.inner.settlement_latency_buckets[SETTLEMENT_LATENCY_BUCKETS_SECS.len()]
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    // ── Snapshot accessors ────────────────────────────────────────────────

    pub fn created(&self) -> u64 {
        self.inner.created.load(Ordering::Relaxed)
    }
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
    pub fn settlement_latency_sum_secs(&self) -> u64 {
        self.inner
            .settlement_latency_sum_secs
            .load(Ordering::Relaxed)
    }
    pub fn settlement_latency_count(&self) -> u64 {
        self.inner.settlement_latency_count.load(Ordering::Relaxed)
    }
    pub fn settlement_latency_bucket(&self, i: usize) -> u64 {
        self.inner.settlement_latency_buckets[i].load(Ordering::Relaxed)
    }
}

impl Default for PaymentMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Database pool and on-disk file-size metrics, gathered fresh on each
/// `/metrics` scrape (issue: missing DB metrics). Cheap: pool state is two
/// atomic reads and file sizes are `stat()` calls on at most three files.
pub struct DbSnapshot {
    pub pool_size: u32,
    pub pool_idle: u32,
    pub pool_max: u32,
    /// Bytes of the main database file. `None` for an in-memory database.
    pub main_bytes: Option<u64>,
    /// Bytes of the `-wal` file. `None` if absent (e.g. no writes yet, or
    /// in-memory) — not rendered as a zero series in that case.
    pub wal_bytes: Option<u64>,
    /// Bytes of the `-shm` shared-memory index file. Same absence semantics
    /// as `wal_bytes`.
    pub shm_bytes: Option<u64>,
}

/// Per-asset gateway trustline state, refreshed by every call to
/// `horizon::check_trustlines` — at boot and, since trustlines can be revoked
/// or `ACCEPTED_ASSETS` extended at any time after that, on the recurring
/// trustline-checker task as well.
///
/// A Horizon failure while checking must not read the same as a confirmed
/// absence: [`Self::record_check_failure`] only bumps `check_failures` and
/// leaves the per-asset map untouched, so a stale "missing" or "present"
/// entry survives an outage rather than being overwritten by a guess.
/// `last_success_unix` (0 until the first successful check) is how a scrape
/// tells "we have never confirmed this" apart from "confirmed and stale".
///
/// Label safety: the `asset` label contains only the asset code (e.g. "USDC",
/// "XLM") — never the issuer address, which is sensitive key-like material.
#[derive(Clone)]
pub struct TrustlineMetrics {
    inner: Arc<TrustlineMetricsInner>,
}

struct TrustlineMetricsInner {
    /// Asset code -> confirmed missing/unauthorized (`true`) or confirmed
    /// usable (`false`). Only ever written by a successful check; a code
    /// absent from the map has simply never been confirmed either way.
    missing: Mutex<HashMap<String, bool>>,
    /// Asset code -> confirmed unauthorized (trustline exists but `is_authorized`
    /// is false). Only ever written by a successful check.
    unauthorized: Mutex<HashMap<String, bool>>,
    /// Asset code -> remaining headroom in stroops (limit - balance).
    /// Present only when both `limit` and `balance` were parseable.
    headroom_stroops: Mutex<HashMap<String, i64>>,
    /// Checks that could not reach Horizon or got a non-2xx response.
    check_failures: AtomicU64,
    /// Unix timestamp of the last check that got a confirmed answer from
    /// Horizon; `0` means never.
    last_success_unix: AtomicI64,
}

impl Default for TrustlineMetricsInner {
    fn default() -> Self {
        Self {
            missing: Mutex::new(HashMap::new()),
            unauthorized: Mutex::new(HashMap::new()),
            headroom_stroops: Mutex::new(HashMap::new()),
            check_failures: AtomicU64::new(0),
            last_success_unix: AtomicI64::new(0),
        }
    }
}

impl TrustlineMetrics {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(TrustlineMetricsInner::default()),
        }
    }

    /// Record a successful check.
    ///
    /// - `checked` — every non-native accepted asset the check evaluated.
    /// - `missing` — the subset with no usable trustline (absent or
    ///   unauthorized).
    ///
    /// This 2-argument form clears the unauthorized and headroom maps.
    /// Use the 4-argument form `record_check_full` when those details are
    /// available.
    ///
    /// Replaces the prior state for exactly the assets checked, so an asset
    /// removed from `ACCEPTED_ASSETS` between checks simply stops being
    /// reported rather than lingering at its last known value.
    pub fn record_check<'a>(
        &self,
        checked: impl IntoIterator<Item = &'a str>,
        missing: &[String],
    ) {
        self.record_check_full(checked, missing, &[], &[]);
    }

    /// Record a successful check with full detail.
    ///
    /// - `checked` — every non-native accepted asset the check evaluated.
    /// - `missing` — the subset with no usable trustline (absent or
    ///   unauthorized).
    /// - `unauthorized` — the subset where a trustline exists but
    ///   `is_authorized` is `false`.
    /// - `headroom` — per-asset remaining capacity in stroops (`limit -
    ///   balance`), for assets where both values were parseable.
    pub fn record_check_full<'a>(
        &self,
        checked: impl IntoIterator<Item = &'a str>,
        missing: &[String],
        unauthorized: &[String],
        headroom: &[(&str, i64)],
    ) {
        let checked_codes: Vec<&str> = checked.into_iter().collect();

        let mut map = self
            .inner
            .missing
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        map.clear();
        for &code in &checked_codes {
            map.insert(code.to_string(), missing.iter().any(|m| m == code));
        }
        drop(map);

        let mut unauth_map = lock_or_recover(
            &self.inner.unauthorized,
            "trustline_metrics.unauthorized",
        );
        unauth_map.clear();
        for &code in &checked_codes {
            unauth_map.insert(
                code.to_string(),
                unauthorized.iter().any(|u| u == code),
            );
        }
        drop(unauth_map);

        let mut hr_map = lock_or_recover(
            &self.inner.headroom_stroops,
            "trustline_metrics.headroom_stroops",
        );
        hr_map.clear();
        for (code, stroops) in headroom {
            hr_map.insert((*code).to_string(), *stroops);
        }
        drop(hr_map);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        self.inner.last_success_unix.store(now, Ordering::Relaxed);
    }

    pub fn record_check_failure(&self) {
        self.inner.check_failures.fetch_add(1, Ordering::Relaxed);
    }

    /// `Some(true)` — confirmed missing/unusable. `Some(false)` — confirmed
    /// usable. `None` — never confirmed either way (not yet checked, or
    /// dropped from `ACCEPTED_ASSETS`).
    pub fn is_missing(&self, code: &str) -> Option<bool> {
        lock_or_recover(&self.inner.missing, "trustline_metrics.missing")
            .get(code)
            .copied()
    }

    pub fn check_failures(&self) -> u64 {
        self.inner.check_failures.load(Ordering::Relaxed)
    }

    pub fn last_success_unix(&self) -> i64 {
        self.inner.last_success_unix.load(Ordering::Relaxed)
    }

    /// Snapshot of missing/usable state, sorted by asset code for
    /// deterministic output.
    pub fn snapshot(&self) -> Vec<(String, bool)> {
        let map = lock_or_recover(&self.inner.missing, "trustline_metrics.missing");
        let mut out: Vec<_> = map.iter().map(|(k, v)| (k.clone(), *v)).collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Snapshot of unauthorized state, sorted by asset code.
    pub fn snapshot_unauthorized(&self) -> Vec<(String, bool)> {
        let map = lock_or_recover(
            &self.inner.unauthorized,
            "trustline_metrics.unauthorized",
        );
        let mut out: Vec<_> = map.iter().map(|(k, v)| (k.clone(), *v)).collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Snapshot of headroom (limit - balance) in stroops, sorted by asset code.
    pub fn snapshot_headroom(&self) -> Vec<(String, i64)> {
        let map = lock_or_recover(
            &self.inner.headroom_stroops,
            "trustline_metrics.headroom_stroops",
        );
        let mut out: Vec<_> = map.iter().map(|(k, v)| (k.clone(), *v)).collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }
}

impl Default for TrustlineMetrics {
    fn default() -> Self {
        Self::new()
    }
}

// ── Prometheus text exposition ────────────────────────────────────────────────

/// Render all metrics as a Prometheus-compatible plain-text snapshot.
/// Called by `GET /metrics`.
///
/// Label safety guarantee: every label value in the rendered output is a
/// fixed subsystem name, enum value, or bounded route template. No label
/// value is derived from request bodies, URL path parameters, merchant data,
/// or any other per-tenant identifier. See the LABEL SAFETY comment at the
/// top of this module for the full policy.
pub fn render(
    webhook: &WebhookMetrics,
    auth: &AuthMetrics,
    tasks: &crate::TaskHealth,
    horizon: &HorizonMetrics,
    http: &HttpMetrics,
    payments: &PaymentMetrics,
    db: &DbSnapshot,
    trustlines: &TrustlineMetrics,
) -> String {
    let mut out = String::with_capacity(4096);

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

    // stellargate_horizon_records_skipped_total — counter vec by reason
    out.push_str(
        "# HELP stellargate_horizon_records_skipped_total Horizon payment records the reconciler refused to credit, by reason.\n",
    );
    out.push_str("# TYPE stellargate_horizon_records_skipped_total counter\n");
    out.push_str(&format!(
        "stellargate_horizon_records_skipped_total{{reason=\"no_tx_hash\"}} {}\n",
        horizon.unhashed_records_skipped()
    ));

    // stellargate_tasks_* — background task health gauges and counters
    out.push_str(
        "# HELP stellargate_tasks_started_total Total background task starts (including restarts).\n",
    );
    out.push_str("# TYPE stellargate_tasks_started_total counter\n");
    out.push_str(&format!(
        "stellargate_tasks_started_total {}\n",
        tasks.started()
    ));
    out.push_str(
        "# HELP stellargate_tasks_stopped_total Total clean background task stops.\n",
    );
    out.push_str("# TYPE stellargate_tasks_stopped_total counter\n");
    out.push_str(&format!(
        "stellargate_tasks_stopped_total {}\n",
        tasks.stopped()
    ));
    out.push_str(
        "# HELP stellargate_tasks_failed_total Total background task panics.\n",
    );
    out.push_str("# TYPE stellargate_tasks_failed_total counter\n");
    out.push_str(&format!(
        "stellargate_tasks_failed_total {}\n",
        tasks.failed()
    ));
    out.push_str(
        "# HELP stellargate_task_restarts_total Supervisor restarts of a background task after panic or unexpected return.\n",
    );
    out.push_str("# TYPE stellargate_task_restarts_total counter\n");
    let snaps = tasks.snapshot();
    for snap in &snaps {
        out.push_str(&format!(
            "stellargate_task_restarts_total{{task=\"{}\"}} {}\n",
            snap.name, snap.restarts
        ));
    }
    out.push_str(
        "# HELP stellargate_task_running Whether the named background task is currently running (1) or not (0).\n",
    );
    out.push_str("# TYPE stellargate_task_running gauge\n");
    for snap in &snaps {
        out.push_str(&format!(
            "stellargate_task_running{{task=\"{}\"}} {}\n",
            snap.name,
            if snap.running { 1 } else { 0 }
        ));
    }
    out.push_str(
        "# HELP stellargate_task_consecutive_failures Consecutive panics of a background task since it last ran stably.\n",
    );
    out.push_str("# TYPE stellargate_task_consecutive_failures gauge\n");
    for snap in &snaps {
        out.push_str(&format!(
            "stellargate_task_consecutive_failures{{task=\"{}\"}} {}\n",
            snap.name, snap.consecutive_failures
        ));
    }

    /* Expected-versus-live (issue #317). The raw counters could not answer
    "how many workers should be running, and how many are?": `stopped` was
    overloaded across clean shutdown, configuration-disabled exit and fault, so
    `started - stopped - failed` was not a live count and there was nothing to
    compare it against. These two gauges are that comparison, and
    `expected` already excludes deliberately-disabled workers. Alert on
    `stellargate_tasks_live < stellargate_tasks_expected`. */
    out.push_str(
        "# HELP stellargate_tasks_expected Background workers this deployment expects to be running, excluding any disabled by configuration.\n",
    );
    out.push_str("# TYPE stellargate_tasks_expected gauge\n");
    out.push_str(&format!(
        "stellargate_tasks_expected {}\n",
        tasks.expected_tasks()
    ));
    out.push_str("# HELP stellargate_tasks_live Expected background workers currently running.\n");
    out.push_str("# TYPE stellargate_tasks_live gauge\n");
    out.push_str(&format!("stellargate_tasks_live {}\n", tasks.live_tasks()));

    /* Separates "switched off on purpose" from "not running", which
    `stellargate_task_running` alone reports identically. */
    out.push_str(
        "# HELP stellargate_task_disabled Whether the named background task exited because configuration gave it nothing to do (1) or not (0).\n",
    );
    out.push_str("# TYPE stellargate_task_disabled gauge\n");
    for snap in &snaps {
        out.push_str(&format!(
            "stellargate_task_disabled{{task=\"{}\"}} {}\n",
            snap.name,
            if snap.disabled_reason.is_some() { 1 } else { 0 }
        ));
    }

    // stellargate_horizon_poll_cycles_total — counter vec by outcome (#313)
    out.push_str(
        "# HELP stellargate_horizon_poll_cycles_total Total Horizon poll cycles by outcome.\n",
    );
    out.push_str("# TYPE stellargate_horizon_poll_cycles_total counter\n");
    out.push_str(&format!(
        "stellargate_horizon_poll_cycles_total{{outcome=\"success\"}} {}\n",
        horizon.success()
    ));
    out.push_str(&format!(
        "stellargate_horizon_poll_cycles_total{{outcome=\"rate_limited\"}} {}\n",
        horizon.rate_limited()
    ));
    out.push_str(&format!(
        "stellargate_horizon_poll_cycles_total{{outcome=\"error\"}} {}\n",
        horizon.error()
    ));

    out.push_str(
        "# HELP stellargate_horizon_repeated_cursor_4xx_total Horizon cursor incidents that reached three consecutive non-rate-limit 4xx responses.\n",
    );
    out.push_str("# TYPE stellargate_horizon_repeated_cursor_4xx_total counter\n");
    out.push_str(&format!(
        "stellargate_horizon_repeated_cursor_4xx_total {}\n",
        horizon.repeated_cursor_4xx()
    ));

    // stellargate_horizon_stream_reconnects_total — counter (#312)
    out.push_str(
        "# HELP stellargate_horizon_stream_reconnects_total Total times the Horizon SSE stream listener reconnected.\n",
    );
    out.push_str("# TYPE stellargate_horizon_stream_reconnects_total counter\n");
    out.push_str(&format!(
        "stellargate_horizon_stream_reconnects_total {}\n",
        horizon.stream_reconnects()
    ));

    /* Reuses TaskHealth's last-success timestamp rather than tracking a
    second one: `note_success()` is already called at the end of every
    successful `poll_once` (and by the stream listener), so it is already the
    authoritative "on-chain detection last made progress" instant that
    /ready's cursor-freshness check reads. */
    out.push_str(
        "# HELP stellargate_horizon_last_successful_poll_timestamp_seconds Unix timestamp of the last successful Horizon poll or stream event.\n",
    );
    out.push_str("# TYPE stellargate_horizon_last_successful_poll_timestamp_seconds gauge\n");
    out.push_str(&format!(
        "stellargate_horizon_last_successful_poll_timestamp_seconds {}\n",
        tasks.last_success_unix()
    ));

    /* stellargate_horizon_cursor_age_seconds — gauge. Previously computed
    correctly in both poll_once and handle_stream_event and only ever logged
    (`info!(cursor_age_secs, ...)`); this is the same value, exported so
    payment-detection lag is alertable rather than something an operator has
    to grep logs for. */
    out.push_str(
        "# HELP stellargate_horizon_cursor_age_seconds Age, in seconds, of the most recently processed Horizon payment record.\n",
    );
    out.push_str("# TYPE stellargate_horizon_cursor_age_seconds gauge\n");
    out.push_str(&format!(
        "stellargate_horizon_cursor_age_seconds {}\n",
        horizon.cursor_age_secs()
    ));

    // stellargate_http_requests_total — counter vec by method/route/status.
    // Labelled by the matched route pattern, never the raw URI, so
    // cardinality is bounded by the fixed route table regardless of how many
    // distinct payment/merchant ids are requested.
    out.push_str(
        "# HELP stellargate_http_requests_total Total HTTP requests by matched route, method, and status.\n",
    );
    out.push_str("# TYPE stellargate_http_requests_total counter\n");
    for (method, route, status, count) in http.requests_snapshot() {
        out.push_str(&format!(
            "stellargate_http_requests_total{{method=\"{method}\",route=\"{route}\",status=\"{status}\"}} {count}\n"
        ));
    }

    // stellargate_http_request_duration_seconds — histogram vec by method/route.
    out.push_str(
        "# HELP stellargate_http_request_duration_seconds HTTP request latency by matched route and method.\n",
    );
    out.push_str("# TYPE stellargate_http_request_duration_seconds histogram\n");
    for (method, route, lat) in http.latency_snapshot() {
        for (i, &bound_ms) in HTTP_LATENCY_BUCKETS_MS.iter().enumerate() {
            out.push_str(&format!(
                "stellargate_http_request_duration_seconds_bucket{{method=\"{method}\",route=\"{route}\",le=\"{}\"}} {}\n",
                bound_ms as f64 / 1000.0,
                lat.buckets[i]
            ));
        }
        out.push_str(&format!(
            "stellargate_http_request_duration_seconds_bucket{{method=\"{method}\",route=\"{route}\",le=\"+Inf\"}} {}\n",
            lat.buckets[HTTP_LATENCY_BUCKETS_MS.len()]
        ));
        out.push_str(&format!(
            "stellargate_http_request_duration_seconds_sum{{method=\"{method}\",route=\"{route}\"}} {}\n",
            lat.sum_ms as f64 / 1000.0
        ));
        out.push_str(&format!(
            "stellargate_http_request_duration_seconds_count{{method=\"{method}\",route=\"{route}\"}} {}\n",
            lat.count
        ));
    }

    // stellargate_payments_total — counter vec by lifecycle status.
    out.push_str("# HELP stellargate_payments_total Total payments by lifecycle status.\n");
    out.push_str("# TYPE stellargate_payments_total counter\n");
    out.push_str(&format!(
        "stellargate_payments_total{{status=\"created\"}} {}\n",
        payments.created()
    ));
    out.push_str(&format!(
        "stellargate_payments_total{{status=\"completed\"}} {}\n",
        payments.completed()
    ));
    out.push_str(&format!(
        "stellargate_payments_total{{status=\"overpaid\"}} {}\n",
        payments.overpaid()
    ));
    out.push_str(&format!(
        "stellargate_payments_total{{status=\"underpaid\"}} {}\n",
        payments.underpaid()
    ));
    out.push_str(&format!(
        "stellargate_payments_total{{status=\"expired\"}} {}\n",
        payments.expired()
    ));

    // stellargate_payment_settlement_latency_seconds — histogram: creation to
    // the poller/stream settling (or partially settling) the intent.
    out.push_str(
        "# HELP stellargate_payment_settlement_latency_seconds Time from payment creation to a settlement outcome (completed, overpaid, or underpaid).\n",
    );
    out.push_str("# TYPE stellargate_payment_settlement_latency_seconds histogram\n");
    for (i, &bound) in SETTLEMENT_LATENCY_BUCKETS_SECS.iter().enumerate() {
        out.push_str(&format!(
            "stellargate_payment_settlement_latency_seconds_bucket{{le=\"{bound}\"}} {}\n",
            payments.settlement_latency_bucket(i)
        ));
    }
    out.push_str(&format!(
        "stellargate_payment_settlement_latency_seconds_bucket{{le=\"+Inf\"}} {}\n",
        payments.settlement_latency_bucket(SETTLEMENT_LATENCY_BUCKETS_SECS.len())
    ));
    out.push_str(&format!(
        "stellargate_payment_settlement_latency_seconds_sum {}\n",
        payments.settlement_latency_sum_secs()
    ));
    out.push_str(&format!(
        "stellargate_payment_settlement_latency_seconds_count {}\n",
        payments.settlement_latency_count()
    ));

    // stellargate_db_pool_connections — gauge vec by state (issue: missing DB metrics).
    out.push_str(
        "# HELP stellargate_db_pool_connections Current SQLite connection pool size by state.\n",
    );
    out.push_str("# TYPE stellargate_db_pool_connections gauge\n");
    out.push_str(&format!(
        "stellargate_db_pool_connections{{state=\"idle\"}} {}\n",
        db.pool_idle
    ));
    out.push_str(&format!(
        "stellargate_db_pool_connections{{state=\"in_use\"}} {}\n",
        db.pool_size.saturating_sub(db.pool_idle)
    ));
    out.push_str(
        "# HELP stellargate_db_pool_max_connections Configured maximum SQLite connection pool size.\n",
    );
    out.push_str("# TYPE stellargate_db_pool_max_connections gauge\n");
    out.push_str(&format!(
        "stellargate_db_pool_max_connections {}\n",
        db.pool_max
    ));

    // stellargate_db_file_size_bytes — gauge vec by file. Omitted entirely
    // (no series) for an in-memory database or a file that hasn't been
    // created yet, rather than rendered as a misleading zero.
    out.push_str(
        "# HELP stellargate_db_file_size_bytes On-disk size of the SQLite database files, in bytes.\n",
    );
    out.push_str("# TYPE stellargate_db_file_size_bytes gauge\n");
    for (file, bytes) in [
        ("main", db.main_bytes),
        ("wal", db.wal_bytes),
        ("shm", db.shm_bytes),
    ] {
        if let Some(bytes) = bytes {
            out.push_str(&format!(
                "stellargate_db_file_size_bytes{{file=\"{file}\"}} {bytes}\n"
            ));
        }
    }

    // stellargate_missing_trustlines — gauge vec by asset
    out.push_str(
        "# HELP stellargate_missing_trustlines Whether the gateway account is currently confirmed to have no usable trustline for an accepted asset (1) or confirmed to have one (0). A trustline is unusable when absent or when is_authorized=false. An asset is absent from this metric until the first successful trustline check evaluates it.\n",
    );
    out.push_str("# TYPE stellargate_missing_trustlines gauge\n");
    for (asset, missing) in trustlines.snapshot() {
        out.push_str(&format!(
            "stellargate_missing_trustlines{{asset=\"{asset}\"}} {}\n",
            if missing { 1 } else { 0 }
        ));
    }

    // stellargate_trustline_unauthorized — gauge vec by asset
    // Distinguishes a trustline that is present but unauthorized from a
    // missing trustline entirely; both surface as stellargate_missing_trustlines=1,
    // but only the former shows here.
    out.push_str(
        "# HELP stellargate_trustline_unauthorized Whether the gateway account's trustline for this asset is present but unauthorized (is_authorized=false). 1 means the issuer has not granted (or has revoked) authorization; payments in this asset will be rejected on-chain.\n",
    );
    out.push_str("# TYPE stellargate_trustline_unauthorized gauge\n");
    for (asset, unauth) in trustlines.snapshot_unauthorized() {
        out.push_str(&format!(
            "stellargate_trustline_unauthorized{{asset=\"{asset}\"}} {}\n",
            if unauth { 1 } else { 0 }
        ));
    }

    // stellargate_trustline_headroom_stroops — gauge vec by asset
    // Remaining capacity (limit - balance) so an approaching ceiling is
    // visible before payments start bouncing.
    out.push_str(
        "# HELP stellargate_trustline_headroom_stroops Remaining trustline capacity in stroops (limit - balance). A payment that would push balance past limit fails on-chain. Alert when this approaches the typical payment size.\n",
    );
    out.push_str("# TYPE stellargate_trustline_headroom_stroops gauge\n");
    for (asset, headroom) in trustlines.snapshot_headroom() {
        out.push_str(&format!(
            "stellargate_trustline_headroom_stroops{{asset=\"{asset}\"}} {headroom}\n"
        ));
    }

    out.push_str(
        "# HELP stellargate_trustline_check_failures_total Total trustline checks that could not reach Horizon or got a non-2xx response. Does not affect stellargate_missing_trustlines, which only reflects confirmed answers.\n",
    );
    out.push_str("# TYPE stellargate_trustline_check_failures_total counter\n");
    out.push_str(&format!(
        "stellargate_trustline_check_failures_total {}\n",
        trustlines.check_failures()
    ));

    out.push_str(
        "# HELP stellargate_trustline_check_last_success_timestamp_seconds Unix timestamp of the last trustline check that got a confirmed answer from Horizon. 0 means never — treat stellargate_missing_trustlines as unknown, not confirmed, until this is nonzero.\n",
    );
    out.push_str("# TYPE stellargate_trustline_check_last_success_timestamp_seconds gauge\n");
    out.push_str(&format!(
        "stellargate_trustline_check_last_success_timestamp_seconds {}\n",
        trustlines.last_success_unix()
    ));

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_db_snapshot() -> DbSnapshot {
        DbSnapshot {
            pool_size: 3,
            pool_idle: 1,
            pool_max: 10,
            main_bytes: None,
            wal_bytes: None,
            shm_bytes: None,
        }
    }

    #[test]
    fn poisoned_metrics_mutex_does_not_panic() {
        let http = HttpMetrics::new();

        let panic_handle = std::thread::spawn({
            let http = http.clone();
            move || {
                let _guard = http.inner.lock().unwrap();
                panic!("deliberate poison");
            }
        });
        let _ = panic_handle.join();

        http.record("GET", "/health", 200, 7);
        assert_eq!(http.requests_snapshot().len(), 1);
        assert_eq!(http.latency_snapshot().len(), 1);
    }

    fn render_all(
        webhook: &WebhookMetrics,
        auth: &AuthMetrics,
        tasks: &crate::TaskHealth,
        horizon: &HorizonMetrics,
        http: &HttpMetrics,
        payments: &PaymentMetrics,
        db: &DbSnapshot,
    ) -> String {
        render(
            webhook,
            auth,
            tasks,
            horizon,
            http,
            payments,
            db,
            &TrustlineMetrics::new(),
        )
    }

    #[test]
    fn metrics_lock_poison_is_recovered_without_panicking() {
        let http = HttpMetrics::new();

        let poison = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = http.inner.lock().unwrap();
            panic!("poison the metrics mutex");
        }));
        assert!(poison.is_err(), "test must intentionally poison the lock");

        http.record("GET", "/health", 200, 42);
        assert_eq!(http.requests_snapshot().len(), 1);
        assert_eq!(http.latency_snapshot().len(), 1);
    }

    // ── HttpMetrics ──────────────────────────────────────────────────────

    #[test]
    fn http_metrics_labels_are_bounded_by_route_not_raw_path() {
        let http = HttpMetrics::new();
        http.record("GET", "/v1/payments/:id", 200, 5);
        http.record("GET", "/v1/payments/:id", 200, 15);
        http.record("GET", "/v1/payments/:id", 404, 3);

        let rendered = render_all(
            &WebhookMetrics::new(),
            &AuthMetrics::new(),
            &crate::TaskHealth::new(),
            &HorizonMetrics::new(),
            &http,
            &PaymentMetrics::new(),
            &empty_db_snapshot(),
        );

        assert!(
            rendered.contains(
                "stellargate_http_requests_total{method=\"GET\",route=\"/v1/payments/:id\",status=\"200\"} 2"
            ),
            "got:\n{rendered}"
        );
        assert!(
            rendered.contains(
                "stellargate_http_requests_total{method=\"GET\",route=\"/v1/payments/:id\",status=\"404\"} 1"
            ),
            "got:\n{rendered}"
        );
        assert!(
            rendered.contains(
                "stellargate_http_request_duration_seconds_count{method=\"GET\",route=\"/v1/payments/:id\"} 3"
            ),
            "the latency histogram must aggregate over the same bounded route \
             label as the counter:\n{rendered}"
        );
        assert!(
            rendered.contains("stellargate_http_request_duration_seconds_sum{method=\"GET\",route=\"/v1/payments/:id\"} 0.023"),
            "sum must be in seconds, not milliseconds:\n{rendered}"
        );
    }

    #[test]
    fn http_latency_histogram_buckets_are_cumulative() {
        let http = HttpMetrics::new();
        http.record("GET", "/health", 200, 3); // falls in every bucket
        http.record("GET", "/health", 200, 600); // falls in buckets >= 1000ms

        let rendered = render_all(
            &WebhookMetrics::new(),
            &AuthMetrics::new(),
            &crate::TaskHealth::new(),
            &HorizonMetrics::new(),
            &http,
            &PaymentMetrics::new(),
            &empty_db_snapshot(),
        );

        assert!(rendered.contains(
            "stellargate_http_request_duration_seconds_bucket{method=\"GET\",route=\"/health\",le=\"0.005\"} 1"
        ));
        assert!(rendered.contains(
            "stellargate_http_request_duration_seconds_bucket{method=\"GET\",route=\"/health\",le=\"1\"} 2"
        ));
        assert!(rendered.contains(
            "stellargate_http_request_duration_seconds_bucket{method=\"GET\",route=\"/health\",le=\"+Inf\"} 2"
        ));
    }

    // ── PaymentMetrics ───────────────────────────────────────────────────

    #[test]
    fn payment_metrics_count_by_lifecycle_status() {
        let payments = PaymentMetrics::new();
        payments.record_created();
        payments.record_created();
        payments.record_settlement("completed", Some(42));
        payments.record_settlement("underpaid", Some(10));
        payments.record_expired();

        assert_eq!(payments.created(), 2);
        assert_eq!(payments.completed(), 1);
        assert_eq!(payments.underpaid(), 1);
        assert_eq!(payments.expired(), 1);
        assert_eq!(payments.settlement_latency_count(), 2);
        assert_eq!(payments.settlement_latency_sum_secs(), 52);
    }

    #[test]
    fn payment_settlement_clock_skew_floors_at_zero_rather_than_wrapping() {
        let payments = PaymentMetrics::new();
        payments.record_settlement("completed", Some(-5));
        // A negative latency must not wrap around as a huge u64 via `as u64`.
        assert_eq!(payments.settlement_latency_sum_secs(), 0);
        assert_eq!(payments.settlement_latency_count(), 1);
    }

    #[test]
    fn payment_metrics_are_exported_on_render() {
        let payments = PaymentMetrics::new();
        payments.record_created();
        payments.record_settlement("overpaid", Some(90));

        let rendered = render_all(
            &WebhookMetrics::new(),
            &AuthMetrics::new(),
            &crate::TaskHealth::new(),
            &HorizonMetrics::new(),
            &HttpMetrics::new(),
            &payments,
            &empty_db_snapshot(),
        );

        assert!(rendered.contains("stellargate_payments_total{status=\"created\"} 1"));
        assert!(rendered.contains("stellargate_payments_total{status=\"overpaid\"} 1"));
        assert!(rendered.contains("stellargate_payment_settlement_latency_seconds_count 1"));
        assert!(rendered.contains("stellargate_payment_settlement_latency_seconds_sum 90"));
    }

    // ── HorizonMetrics cursor age ────────────────────────────────────────

    #[test]
    fn horizon_cursor_age_is_a_gauge_not_only_a_log_line() {
        let horizon = HorizonMetrics::new();
        assert_eq!(horizon.cursor_age_secs(), 0);
        horizon.record_cursor_age_secs(37);
        assert_eq!(horizon.cursor_age_secs(), 37);

        let rendered = render_all(
            &WebhookMetrics::new(),
            &AuthMetrics::new(),
            &crate::TaskHealth::new(),
            &horizon,
            &HttpMetrics::new(),
            &PaymentMetrics::new(),
            &empty_db_snapshot(),
        );
        assert!(
            rendered.contains("# TYPE stellargate_horizon_cursor_age_seconds gauge"),
            "got:\n{rendered}"
        );
        assert!(
            rendered.contains("stellargate_horizon_cursor_age_seconds 37"),
            "got:\n{rendered}"
        );
    }

    #[test]
    fn repeated_cursor_4xx_incidents_are_exported_as_a_counter() {
        let horizon = HorizonMetrics::new();
        horizon.record_repeated_cursor_4xx();
        assert_eq!(horizon.repeated_cursor_4xx(), 1);

        let rendered = render_all(
            &WebhookMetrics::new(),
            &AuthMetrics::new(),
            &crate::TaskHealth::new(),
            &horizon,
            &HttpMetrics::new(),
            &PaymentMetrics::new(),
            &empty_db_snapshot(),
        );
        assert!(rendered.contains("# TYPE stellargate_horizon_repeated_cursor_4xx_total counter"));
        assert!(rendered.contains("stellargate_horizon_repeated_cursor_4xx_total 1"));
    }

    // ── DbSnapshot ───────────────────────────────────────────────────────

    #[test]
    fn db_pool_metrics_are_exported() {
        let db = DbSnapshot {
            pool_size: 4,
            pool_idle: 3,
            pool_max: 10,
            main_bytes: None,
            wal_bytes: None,
            shm_bytes: None,
        };
        let rendered = render_all(
            &WebhookMetrics::new(),
            &AuthMetrics::new(),
            &crate::TaskHealth::new(),
            &HorizonMetrics::new(),
            &HttpMetrics::new(),
            &PaymentMetrics::new(),
            &db,
        );
        assert!(rendered.contains("stellargate_db_pool_connections{state=\"idle\"} 3"));
        assert!(rendered.contains("stellargate_db_pool_connections{state=\"in_use\"} 1"));
        assert!(rendered.contains("stellargate_db_pool_max_connections 10"));
    }

    #[test]
    fn db_file_size_series_are_omitted_when_absent() {
        let rendered = render_all(
            &WebhookMetrics::new(),
            &AuthMetrics::new(),
            &crate::TaskHealth::new(),
            &HorizonMetrics::new(),
            &HttpMetrics::new(),
            &PaymentMetrics::new(),
            &empty_db_snapshot(),
        );
        assert!(
            !rendered.contains("stellargate_db_file_size_bytes{"),
            "an in-memory database must render no file-size series, not a \
             misleading zero:\n{rendered}"
        );
    }

    #[test]
    fn db_file_size_series_are_rendered_when_present() {
        let db = DbSnapshot {
            pool_size: 1,
            pool_idle: 1,
            pool_max: 10,
            main_bytes: Some(4096),
            wal_bytes: Some(128),
            shm_bytes: None,
        };
        let rendered = render_all(
            &WebhookMetrics::new(),
            &AuthMetrics::new(),
            &crate::TaskHealth::new(),
            &HorizonMetrics::new(),
            &HttpMetrics::new(),
            &PaymentMetrics::new(),
            &db,
        );
        assert!(rendered.contains("stellargate_db_file_size_bytes{file=\"main\"} 4096"));
        assert!(rendered.contains("stellargate_db_file_size_bytes{file=\"wal\"} 128"));
        assert!(!rendered.contains("stellargate_db_file_size_bytes{file=\"shm\"}"));
    }

    // ── #440 new tests ────────────────────────────────────────────────────

    // WebhookMetrics ─────────────────────────────────────────────────────

    #[test]
    fn webhook_counters_increment_independently() {
        let wm = WebhookMetrics::new();
        wm.record_delivered();
        wm.record_delivered();
        wm.record_delivered();
        wm.record_failed();
        wm.record_failed();
        wm.record_retry();

        assert_eq!(wm.delivered(), 3);
        assert_eq!(wm.failed(), 2);
        assert_eq!(wm.retried(), 1);
    }

    #[test]
    fn webhook_latency_histogram_buckets_are_cumulative_75ms() {
        let wm = WebhookMetrics::new();
        wm.record_latency_ms(75);

        // LATENCY_BUCKETS_MS = [10, 50, 100, 250, 500, 1_000, 2_500, 5_000, 10_000]
        // 75 <= 100 (index 2), so buckets 2..=8 (all from le=100 up) and +Inf increment.
        // le=50 (index 1) should be 0; le=100 (index 2) should be 1.
        assert_eq!(wm.latency_bucket(1), 0, "le=50 bucket should be 0 for 75ms");
        assert_eq!(wm.latency_bucket(2), 1, "le=100 bucket should be 1 for 75ms");
        // +Inf bucket is always at index LATENCY_BUCKETS_MS.len() = 9
        assert_eq!(
            wm.latency_bucket(LATENCY_BUCKETS_MS.len()),
            1,
            "+Inf bucket should be 1"
        );
        assert_eq!(wm.latency_sum_ms(), 75);
        assert_eq!(wm.latency_count(), 1);
    }

    #[test]
    fn webhook_latency_histogram_exact_boundary_100ms() {
        let wm = WebhookMetrics::new();
        wm.record_latency_ms(100);

        // 100 <= 100 (index 2) — increments le=100.
        // 100 > 50 — le=50 (index 1) stays 0.
        assert_eq!(wm.latency_bucket(1), 0, "le=50 bucket should be 0 for 100ms");
        assert_eq!(
            wm.latency_bucket(2),
            1,
            "le=100 bucket should be 1 for exactly 100ms"
        );
        assert_eq!(
            wm.latency_bucket(LATENCY_BUCKETS_MS.len()),
            1,
            "+Inf bucket should be 1"
        );
    }

    #[test]
    fn webhook_latency_record_zero_ms() {
        let wm = WebhookMetrics::new();
        wm.record_latency_ms(0);

        // 0 <= 10 (index 0, smallest bucket) — increments le=10 and +Inf.
        assert_eq!(wm.latency_bucket(0), 1, "le=10 bucket should be 1 for 0ms");
        assert_eq!(
            wm.latency_bucket(LATENCY_BUCKETS_MS.len()),
            1,
            "+Inf bucket should be 1"
        );
    }

    #[test]
    fn webhook_latency_large_value_only_inf_bucket() {
        let wm = WebhookMetrics::new();
        wm.record_latency_ms(99_999);

        // 99_999 exceeds all named buckets (max is 10_000).
        // All named buckets (indices 0..8) should be 0.
        for i in 0..LATENCY_BUCKETS_MS.len() {
            assert_eq!(
                wm.latency_bucket(i),
                0,
                "named bucket {i} (le={}) should be 0 for 99_999ms",
                LATENCY_BUCKETS_MS[i]
            );
        }
        assert_eq!(
            wm.latency_bucket(LATENCY_BUCKETS_MS.len()),
            1,
            "+Inf bucket should be 1 for 99_999ms"
        );
    }

    // AuthMetrics ────────────────────────────────────────────────────────

    #[test]
    fn auth_counters_are_independent() {
        let am = AuthMetrics::new();
        am.record_success();
        am.record_success();
        am.record_success();
        am.record_failure_missing_key();
        am.record_failure_missing_key();
        am.record_failure_invalid_key();
        am.record_failure_internal_error();
        am.record_failure_internal_error();
        am.record_failure_internal_error();
        am.record_failure_internal_error();

        assert_eq!(am.success(), 3);
        assert_eq!(am.failure_missing_key(), 2);
        assert_eq!(am.failure_invalid_key(), 1);
        assert_eq!(am.failure_internal_error(), 4);
    }

    // HorizonMetrics ─────────────────────────────────────────────────────

    #[test]
    fn horizon_all_five_counters_are_independent() {
        let hm = HorizonMetrics::new();
        hm.record_success();
        hm.record_success();
        hm.record_rate_limited();
        hm.record_rate_limited();
        hm.record_rate_limited();
        hm.record_error();
        hm.record_repeated_cursor_4xx();
        hm.record_repeated_cursor_4xx();
        hm.record_repeated_cursor_4xx();
        hm.record_repeated_cursor_4xx();
        hm.record_stream_reconnect();

        assert_eq!(hm.success(), 2);
        assert_eq!(hm.rate_limited(), 3);
        assert_eq!(hm.error(), 1);
        assert_eq!(hm.repeated_cursor_4xx(), 4);
        assert_eq!(hm.stream_reconnects(), 1);
    }

    #[test]
    fn horizon_cursor_age_stores_and_overwrites() {
        let hm = HorizonMetrics::new();
        hm.record_cursor_age_secs(100);
        assert_eq!(hm.cursor_age_secs(), 100);
        hm.record_cursor_age_secs(5);
        assert_eq!(hm.cursor_age_secs(), 5, "store should overwrite, not add");
    }

    // TrustlineMetrics ───────────────────────────────────────────────────

    #[test]
    fn trustline_record_check_marks_assets_correctly() {
        let tm = TrustlineMetrics::new();
        tm.record_check(["USDC", "BTC"], &["USDC".to_string()]);

        assert_eq!(tm.is_missing("USDC"), Some(true), "USDC should be missing");
        assert_eq!(tm.is_missing("BTC"), Some(false), "BTC should be present");
        assert_eq!(tm.is_missing("ETH"), None, "ETH was never checked");
    }

    #[test]
    fn trustline_record_check_replaces_prior_state() {
        let tm = TrustlineMetrics::new();
        // First check: USDC is missing.
        tm.record_check(["USDC"], &["USDC".to_string()]);
        assert_eq!(tm.is_missing("USDC"), Some(true));
        // Second check: USDC now has a trustline.
        tm.record_check(["USDC"], &[]);
        assert_eq!(
            tm.is_missing("USDC"),
            Some(false),
            "second check should mark USDC as present"
        );
    }

    #[test]
    fn trustline_record_check_clears_dropped_assets() {
        let tm = TrustlineMetrics::new();
        // First check evaluates both USDC and BTC.
        tm.record_check(["USDC", "BTC"], &[]);
        assert_eq!(tm.is_missing("BTC"), Some(false));
        // Second check only evaluates USDC; BTC is dropped.
        tm.record_check(["USDC"], &[]);
        assert_eq!(
            tm.is_missing("BTC"),
            None,
            "BTC dropped from checked set should become None"
        );
    }

    #[test]
    fn trustline_record_check_failure_increments_and_does_not_update_last_success() {
        let tm = TrustlineMetrics::new();
        tm.record_check_failure();
        tm.record_check_failure();
        tm.record_check_failure();

        assert_eq!(tm.check_failures(), 3);
        assert_eq!(
            tm.last_success_unix(),
            0,
            "failures must not update last_success_unix"
        );
    }

    #[test]
    fn trustline_snapshot_is_sorted_by_asset_code() {
        let tm = TrustlineMetrics::new();
        tm.record_check(["USDC", "BTC", "ETH"], &[]);

        let snap = tm.snapshot();
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].0, "BTC");
        assert_eq!(snap[1].0, "ETH");
        assert_eq!(snap[2].0, "USDC");
    }

    #[test]
    fn trustline_last_success_unix_is_set_after_record_check() {
        let tm = TrustlineMetrics::new();
        assert_eq!(tm.last_success_unix(), 0, "should start at 0");
        tm.record_check(["USDC"], &[]);
        assert!(
            tm.last_success_unix() > 0,
            "last_success_unix should be set after record_check"
        );
    }

    // render() — trustline output ─────────────────────────────────────────

    #[test]
    fn render_includes_missing_trustlines_as_1() {
        let trustlines = TrustlineMetrics::new();
        trustlines.record_check(["USDC"], &["USDC".to_string()]);

        let rendered = render(
            &WebhookMetrics::new(),
            &AuthMetrics::new(),
            &crate::TaskHealth::new(),
            &HorizonMetrics::new(),
            &HttpMetrics::new(),
            &PaymentMetrics::new(),
            &empty_db_snapshot(),
            &trustlines,
        );

        assert!(
            rendered.contains("stellargate_missing_trustlines{asset=\"USDC\"} 1"),
            "missing trustline must render as 1:\n{rendered}"
        );
    }

    #[test]
    fn render_includes_present_trustlines_as_0() {
        let trustlines = TrustlineMetrics::new();
        trustlines.record_check(["USDC"], &[]);

        let rendered = render(
            &WebhookMetrics::new(),
            &AuthMetrics::new(),
            &crate::TaskHealth::new(),
            &HorizonMetrics::new(),
            &HttpMetrics::new(),
            &PaymentMetrics::new(),
            &empty_db_snapshot(),
            &trustlines,
        );

        assert!(
            rendered.contains("stellargate_missing_trustlines{asset=\"USDC\"} 0"),
            "present trustline must render as 0:\n{rendered}"
        );
    }

    #[test]
    fn render_includes_check_failures_and_last_success() {
        let trustlines = TrustlineMetrics::new();
        trustlines.record_check_failure();

        let rendered = render(
            &WebhookMetrics::new(),
            &AuthMetrics::new(),
            &crate::TaskHealth::new(),
            &HorizonMetrics::new(),
            &HttpMetrics::new(),
            &PaymentMetrics::new(),
            &empty_db_snapshot(),
            &trustlines,
        );

        assert!(
            rendered.contains("stellargate_trustline_check_failures_total 1"),
            "check_failures counter must be rendered:\n{rendered}"
        );
        // last_success_unix should be 0 (no successful check yet).
        assert!(
            rendered.contains("stellargate_trustline_check_last_success_timestamp_seconds 0"),
            "last_success_unix must be 0 when no check has succeeded:\n{rendered}"
        );
    }
}
