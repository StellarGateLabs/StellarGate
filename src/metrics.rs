//! In-process metrics: webhook delivery counters/histogram and background-task
//! health gauges. All types are cheaply clonable (Arc-wrapped atomics) and can
//! be stored on AppState without additional synchronisation.
//!
//! GET /metrics returns a Prometheus plain-text snapshot.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;

const LATENCY_BUCKETS_MS: &[u64] = &[10, 50, 100, 250, 500, 1_000, 2_500, 5_000, 10_000];

#[derive(Clone)]
pub struct WebhookMetrics {
    inner: Arc<WebhookMetricsInner>,
}
struct WebhookMetricsInner {
    delivered: AtomicU64, failed: AtomicU64, retried: AtomicU64,
    latency_sum_ms: AtomicU64, latency_count: AtomicU64,
    latency_buckets: [AtomicU64; 10],
}
impl Default for WebhookMetricsInner {
    fn default() -> Self {
        Self {
            delivered: AtomicU64::new(0), failed: AtomicU64::new(0),
            retried: AtomicU64::new(0), latency_sum_ms: AtomicU64::new(0),
            latency_count: AtomicU64::new(0),
            latency_buckets: [
                AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
                AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
                AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
                AtomicU64::new(0),
            ],
        }
    }
}
impl WebhookMetrics {
    pub fn new() -> Self { Self { inner: Arc::new(WebhookMetricsInner::default()) } }
    pub fn record_delivered(&self) { self.inner.delivered.fetch_add(1, Ordering::Relaxed); }
    pub fn record_failed(&self)    { self.inner.failed.fetch_add(1, Ordering::Relaxed); }
    pub fn record_retry(&self)     { self.inner.retried.fetch_add(1, Ordering::Relaxed); }
    pub fn record_latency_ms(&self, ms: u64) {
        self.inner.latency_sum_ms.fetch_add(ms, Ordering::Relaxed);
        self.inner.latency_count.fetch_add(1, Ordering::Relaxed);
        for (i, &b) in LATENCY_BUCKETS_MS.iter().enumerate() {
            if ms <= b { self.inner.latency_buckets[i].fetch_add(1, Ordering::Relaxed); }
        }
        self.inner.latency_buckets[LATENCY_BUCKETS_MS.len()].fetch_add(1, Ordering::Relaxed);
    }
    pub fn delivered(&self) -> u64      { self.inner.delivered.load(Ordering::Relaxed) }
    pub fn failed(&self) -> u64         { self.inner.failed.load(Ordering::Relaxed) }
    pub fn retried(&self) -> u64        { self.inner.retried.load(Ordering::Relaxed) }
    pub fn latency_sum_ms(&self) -> u64 { self.inner.latency_sum_ms.load(Ordering::Relaxed) }
    pub fn latency_count(&self) -> u64  { self.inner.latency_count.load(Ordering::Relaxed) }
    pub fn latency_bucket(&self, i: usize) -> u64 { self.inner.latency_buckets[i].load(Ordering::Relaxed) }
}
impl Default for WebhookMetrics { fn default() -> Self { Self::new() } }

#[derive(Clone)]
pub struct TaskHealth {
    inner: Arc<TaskHealthInner>,
}
struct TaskHealthInner {
    healthy: AtomicI64,
    failures: AtomicU64,
}
impl TaskHealth {
    pub fn new() -> Self {
        Self { inner: Arc::new(TaskHealthInner { healthy: AtomicI64::new(0), failures: AtomicU64::new(0) }) }
    }
    pub fn task_started(&self) { self.inner.healthy.fetch_add(1, Ordering::Relaxed); }
    pub fn task_stopped(&self) { self.inner.healthy.fetch_sub(1, Ordering::Relaxed); }
    pub fn task_failed(&self)  { self.inner.healthy.fetch_sub(1, Ordering::Relaxed); self.inner.failures.fetch_add(1, Ordering::Relaxed); }
    pub fn healthy_count(&self) -> i64 { self.inner.healthy.load(Ordering::Relaxed) }
    pub fn failure_count(&self) -> u64 { self.inner.failures.load(Ordering::Relaxed) }
}
impl Default for TaskHealth { fn default() -> Self { Self::new() } }

pub fn render(webhook: &WebhookMetrics, tasks: &TaskHealth) -> String {
    let mut out = String::with_capacity(1536);
    out.push_str("# HELP stellargate_webhook_deliveries_total Total webhook delivery attempts by outcome.\n");
    out.push_str("# TYPE stellargate_webhook_deliveries_total counter\n");
    out.push_str(&format!("stellargate_webhook_deliveries_total{{outcome=\"delivered\"}} {}\n", webhook.delivered()));
    out.push_str(&format!("stellargate_webhook_deliveries_total{{outcome=\"failed\"}} {}\n", webhook.failed()));
    out.push_str("# HELP stellargate_webhook_retries_total Total webhook retry attempts (excludes first try).\n");
    out.push_str("# TYPE stellargate_webhook_retries_total counter\n");
    out.push_str(&format!("stellargate_webhook_retries_total {}\n", webhook.retried()));
    out.push_str("# HELP stellargate_webhook_delivery_latency_ms End-to-end webhook delivery latency in milliseconds.\n");
    out.push_str("# TYPE stellargate_webhook_delivery_latency_ms histogram\n");
    for (i, &b) in LATENCY_BUCKETS_MS.iter().enumerate() {
        out.push_str(&format!("stellargate_webhook_delivery_latency_ms_bucket{{le=\"{}\"}} {}\n", b, webhook.latency_bucket(i)));
    }
    out.push_str(&format!("stellargate_webhook_delivery_latency_ms_bucket{{le=\"+Inf\"}} {}\n", webhook.latency_bucket(LATENCY_BUCKETS_MS.len())));
    out.push_str(&format!("stellargate_webhook_delivery_latency_ms_sum {}\n", webhook.latency_sum_ms()));
    out.push_str(&format!("stellargate_webhook_delivery_latency_ms_count {}\n", webhook.latency_count()));
    out.push_str("# HELP stellargate_background_tasks_healthy Number of background worker tasks currently running.\n");
    out.push_str("# TYPE stellargate_background_tasks_healthy gauge\n");
    out.push_str(&format!("stellargate_background_tasks_healthy {}\n", tasks.healthy_count()));
    out.push_str("# HELP stellargate_background_task_failures_total Cumulative count of background task unexpected exits.\n");
    out.push_str("# TYPE stellargate_background_task_failures_total counter\n");
    out.push_str(&format!("stellargate_background_task_failures_total {}\n", tasks.failure_count()));
    out
}
