pub mod api;
pub mod config;
pub mod db;
pub mod expiry;
pub mod horizon;
pub mod metrics;
pub mod money;
pub mod retention;
pub mod ssrf;
pub mod strkey;
pub mod supervise;
pub mod webhook;

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub const CRASH_LOOP_THRESHOLD: u32 = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSnapshot {
    pub name: &'static str,
    pub running: bool,
    pub restarts: u64,
    pub consecutive_failures: u32,
    pub disabled_reason: Option<&'static str>,
}

/// Consecutive panics at or above this mark a required task as crash-looping.
/// `/health` fails while any required task is crash-looping, even if the
/// supervisor has already spawned a replacement.
pub const CRASH_LOOP_THRESHOLD: u32 = 3;

/// Snapshot of one background task, used to render `/metrics`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSnapshot {
    pub name: &'static str,
    pub running: bool,
    pub restarts: u64,
    pub consecutive_failures: u32,
    pub disabled_reason: Option<&'static str>,
}

/// Tracks background task health for liveness, readiness, and monitoring.
#[derive(Clone)]
pub struct TaskHealth {
    inner: Arc<TaskHealthInner>,
}

struct TaskHealthInner {
    started: AtomicU64,
    stopped: AtomicU64,
    failed: AtomicU64,
    running: Mutex<HashMap<&'static str, bool>>,
    restarts: Mutex<HashMap<&'static str, u64>>,
    consecutive_failures: Mutex<HashMap<&'static str, u32>>,
    disabled: Mutex<HashMap<&'static str, &'static str>>,
    required: Mutex<Vec<&'static str>>,
    last_success_unix: AtomicI64,
    gateway_account_exists: std::sync::atomic::AtomicBool,
}

impl Default for TaskHealthInner {
    fn default() -> Self {
        Self {
            started: AtomicU64::new(0),
            stopped: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            running: Mutex::new(HashMap::new()),
            restarts: Mutex::new(HashMap::new()),
            consecutive_failures: Mutex::new(HashMap::new()),
            disabled: Mutex::new(HashMap::new()),
            required: Mutex::new(Vec::new()),
            last_success_unix: AtomicI64::new(0),
            gateway_account_exists: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl TaskHealth {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(TaskHealthInner::default()),
        }
    }

    pub fn require(&self, name: &'static str) {
        lock(&self.inner.required).push(name);
    }

    pub fn task_started(&self, name: &'static str) {
        self.inner.started.fetch_add(1, Ordering::Relaxed);
        lock(&self.inner.running).insert(name, true);
    }

    pub fn task_stopped(&self, name: &'static str) {
        self.inner.stopped.fetch_add(1, Ordering::Relaxed);
        lock(&self.inner.running).insert(name, false);
    }

    pub fn task_failed(&self, name: &'static str) {
        self.inner.failed.fetch_add(1, Ordering::Relaxed);
        lock(&self.inner.running).insert(name, false);
        let mut consecutive = lock(&self.inner.consecutive_failures);
        *consecutive.entry(name).or_insert(0) += 1;
    }

    pub fn task_restarted(&self, name: &'static str) {
        let mut restarts = lock(&self.inner.restarts);
        *restarts.entry(name).or_insert(0) += 1;
    }

    pub fn task_disabled(&self, name: &'static str, reason: &'static str) {
        self.inner.stopped.fetch_add(1, Ordering::Relaxed);
        lock(&self.inner.running).insert(name, false);
        lock(&self.inner.disabled).insert(name, reason);
    }

    pub fn disabled_reason(&self, name: &'static str) -> Option<&'static str> {
        lock(&self.inner.disabled).get(name).copied()
    }

    pub fn expected_tasks(&self) -> usize {
        let required = lock(&self.inner.required);
        let disabled = lock(&self.inner.disabled);
        required.iter().filter(|name| !disabled.contains_key(*name)).count()
    }

    pub fn live_tasks(&self) -> usize {
        let running = lock(&self.inner.running);
        let required = lock(&self.inner.required);
        let disabled = lock(&self.inner.disabled);
        required.iter()
            .filter(|name| !disabled.contains_key(*name))
            .filter(|name| running.get(*name) == Some(&true))
            .count()
    }

    pub fn note_stable(&self, name: &'static str) {
        lock(&self.inner.consecutive_failures).insert(name, 0);
    }

    pub fn started(&self) -> u64 { self.inner.started.load(Ordering::Relaxed) }
    pub fn stopped(&self) -> u64 { self.inner.stopped.load(Ordering::Relaxed) }
    pub fn failed(&self) -> u64 { self.inner.failed.load(Ordering::Relaxed) }

    pub fn restarts(&self, name: &'static str) -> u64 {
        lock(&self.inner.restarts).get(name).copied().unwrap_or(0)
    }

    pub fn consecutive_failures(&self, name: &'static str) -> u32 {
        lock(&self.inner.consecutive_failures).get(name).copied().unwrap_or(0)
    }

    pub fn dead_required_tasks(&self) -> Vec<&'static str> {
        let running = lock(&self.inner.running);
        let required = lock(&self.inner.required);
        let disabled = lock(&self.inner.disabled);
        required.iter().copied()
            .filter(|name| !disabled.contains_key(name))
            .filter(|name| running.get(name) != Some(&true))
            .collect()
    }

    pub fn crash_looping_required_tasks(&self) -> Vec<&'static str> {
        let consecutive = lock(&self.inner.consecutive_failures);
        let required = lock(&self.inner.required);
1        required.iter().copied()
            .filter(|name| consecutive.get(name).copied().unwrap_or(0) >= CRASH_LOOP_THRESHOLD)
            .collect()
    }

    pub fn snapshot(&self) -> Vec<TaskSnapshot> {
        let running = lock(&self.inner.running);
        let restarts = lock(&self.inner.restarts);
        let consecutive = lock(&self.inner.consecutive_failures);
        let required = lock(&self.inner.required);
        let disabled = lock(&self.inner.disabled);
        let mut names = required.clone();
        for name in running.keys().chain(restarts.keys()).chain(consecutive.keys()).chain(disabled.keys()) {
            if !names.contains(name) { names.push(*name); }
        }
        names.sort_unstable();
        names.into_iter().map(|name| TaskSnapshot {
            name,
            running: running.get(name).copied().unwrap_or(false),
            restarts: restarts.get(name).copied().unwrap_or(0),
            consecutive_failures: consecutive.get(name).copied().unwrap_or(0),
            disabled_reason: disabled.get(name).copied(),
        }).collect()
    }

    pub fn note_success(&self) {
        self.set_last_success_unix(unix_now_secs());
    }

    pub fn set_last_success_unix(&self, unix_secs: i64) {
        self.inner.last_success_unix.store(unix_secs, Ordering::Relaxed);
    }

    pub fn last_success_age_secs(&self) -> i64 {
        unix_now_secs().saturating_sub(self.inner.last_success_unix.load(Ordering::Relaxed))
    }

    pub fn last_success_unix(&self) -> i64 {
        self.inner.last_success_unix.load(Ordering::Relaxed)
    }

    pub fn set_gateway_account_exists(&self, exists: bool) {
        self.inner
            .gateway_account_exists
            .store(exists, Ordering::Relaxed);
    }

    pub fn gateway_account_exists(&self) -> bool {
        self.inner.gateway_account_exists.load(Ordering::Relaxed)
    }
}

fn unix_now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

impl Default for TaskHealth {
    fn default() -> Self {
        Self::new()
    }
}

/// Shared application state handed to every request handler and the background
/// Horizon poller. Cloning is cheap — the pool and HTTP client are internally
/// reference-counted.
pub struct AppState {
    pub pool: db::Db,
    pub config: config::Config,
    pub http: reqwest::Client,
    pub webhook_http: reqwest::Client,
    /// Webhook delivery metrics: delivered/failed/retried counts and a latency
    /// histogram. Exposed via `GET /metrics` so operators can see delivery
    /// success rate, retry volume, and failure spikes at a glance.
    pub webhook_metrics: metrics::WebhookMetrics,
    /// Auth middleware outcome counters: success/failure (by reason) counts.
    /// Exposed via `GET /metrics` so credential-stuffing or misconfigured
    /// clients are visible without grepping logs.
    pub auth_metrics: metrics::AuthMetrics,
    /// Horizon reconciliation counters: currently records skipped for having no
    /// usable transaction hash, so an unexpected Horizon payload is visible in
    /// `GET /metrics` and not only in the logs.
    pub horizon_metrics: metrics::HorizonMetrics,
    /// Background task health: tracks started, stopped, and failed task counts
    /// for monitoring and alerting.
    pub task_health: TaskHealth,
}
