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

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Number of consecutive panics that puts a task into the crash-loop state.
/// Exposed as a `pub const` so tests in `supervise.rs` can reference it
/// without hard-coding the threshold twice.
pub const CRASH_LOOP_THRESHOLD: u32 = 3;

// ── Per-task state ────────────────────────────────────────────────────────────

/// A point-in-time snapshot of one background task's health, used by
/// `GET /metrics` to render per-task Prometheus gauges.
pub struct TaskSnapshot {
    pub name: String,
    /// How many supervisor-triggered restarts this task has had.
    pub restarts: u64,
    /// Whether the task is currently considered running.
    pub running: bool,
    /// Consecutive panics since the last stable run.
    pub consecutive_failures: u32,
    /// Set if the task exited because configuration gave it nothing to do.
    pub disabled_reason: Option<String>,
}

/// Mutable state for a single named background task, held inside the
/// `Mutex`-guarded map in [`TaskHealthInner`].
#[derive(Default)]
struct TaskState {
    /// Task is currently running.
    running: bool,
    /// Supervisor-triggered restarts (not panics — those go to `failed` on
    /// `TaskHealthInner`).
    restarts: u64,
    /// Consecutive panics / `task_failed` calls since the last `note_stable`.
    consecutive_failures: u32,
    /// Reason the task exited because of a configuration choice, if any.
    disabled_reason: Option<&'static str>,
}

// ── Inner ─────────────────────────────────────────────────────────────────────

struct TaskHealthInner {
    /// Per-task mutable state, keyed by the task's static name.
    ///
    /// The `Mutex` is acquired with `unwrap_or_else(|e| e.into_inner())`:
    /// if a prior holder panicked while holding the lock the data is still
    /// valid — we take the guard and continue rather than propagating a
    /// secondary panic. This is the standard poison-recovery pattern for
    /// locks that guard plain data (no invariant was broken by the panic).
    tasks: Mutex<HashMap<&'static str, TaskState>>,

    /// Set of task names that must be running for the process to be
    /// considered healthy. Populated by [`TaskHealth::require`] at boot.
    required: Mutex<HashSet<&'static str>>,

    /// Total panics recorded across all tasks. Incremented by
    /// [`TaskHealth::task_failed`] and read by [`TaskHealth::failed`].
    failed: AtomicU64,

    /// Total `task_started` calls across all tasks (including restarts).
    /// Read by [`TaskHealth::started`].
    started: AtomicU64,

    /// Total clean `task_stopped` calls across all tasks. Read by
    /// [`TaskHealth::stopped`].
    stopped: AtomicU64,

    /// Unix timestamp (seconds) of the last successful Horizon poll or stream
    /// event. `0` until the first call to [`TaskHealth::note_success`].
    last_success_unix: AtomicI64,

    /// Flag that is set once on startup to confirm the gateway account exists.
    gateway_account_exists: AtomicBool,
}

impl Default for TaskHealthInner {
    fn default() -> Self {
        Self {
            tasks: Mutex::new(HashMap::new()),
            required: Mutex::new(HashSet::new()),
            failed: AtomicU64::new(0),
            started: AtomicU64::new(0),
            stopped: AtomicU64::new(0),
            last_success_unix: AtomicI64::new(0),
            gateway_account_exists: AtomicBool::new(false),
        }
    }
}

// ── Public handle ─────────────────────────────────────────────────────────────

/// Tracks background task health: per-task running state, restart and
/// consecutive-failure counts, and a global panic counter.
///
/// Cheap to clone — the inner data is behind an [`Arc`].
#[derive(Clone)]
pub struct TaskHealth {
    inner: Arc<TaskHealthInner>,
}

impl TaskHealth {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(TaskHealthInner::default()),
        }
    }

    // ── Registration ─────────────────────────────────────────────────────────

    /// Declare `name` as a required background task. Must be called at boot,
    /// before the supervisor starts the task, so that `dead_required_tasks`
    /// and `expected_tasks` / `live_tasks` report correctly from the first
    /// moment.
    pub fn require(&self, name: &'static str) {
        self.inner
            .required
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name);
        self.inner
            .tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(name)
            .or_default();
    }

    // ── Named task lifecycle ──────────────────────────────────────────────────

    /// Record that `name` started running. Called by the supervisor just
    /// before spawning the child task.
    pub fn task_started(&self, name: &'static str) {
        self.inner.started.fetch_add(1, Ordering::Relaxed);
        let mut tasks = self
            .inner
            .tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let state = tasks.entry(name).or_default();
        state.running = true;
        // A fresh start clears the disabled marker: a restarted task is no
        // longer disabled.
        state.disabled_reason = None;
    }

    /// Record that `name` stopped cleanly (shutdown requested or ordinary
    /// return). Does **not** increment the failure counter.
    pub fn task_stopped(&self, name: &'static str) {
        self.inner.stopped.fetch_add(1, Ordering::Relaxed);
        let mut tasks = self
            .inner
            .tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let state = tasks.entry(name).or_default();
        state.running = false;
    }

    /// Record that `name` panicked. Increments the global `failed` counter
    /// **and** the task's `consecutive_failures` streak; marks it not running.
    pub fn task_failed(&self, name: &'static str) {
        self.inner.failed.fetch_add(1, Ordering::Relaxed);
        let mut tasks = self
            .inner
            .tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let state = tasks.entry(name).or_default();
        state.running = false;
        state.consecutive_failures = state.consecutive_failures.saturating_add(1);
    }

    /// Record that `name` exited because configuration gave it nothing to do.
    /// Removes it from `required` so it is no longer counted in
    /// `expected_tasks` / `live_tasks`, and marks the reason so the `/health`
    /// response and Prometheus can distinguish it from a fault.
    pub fn task_disabled(&self, name: &'static str, reason: &'static str) {
        self.inner
            .required
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(name);
        let mut tasks = self
            .inner
            .tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let state = tasks.entry(name).or_default();
        state.running = false;
        state.disabled_reason = Some(reason);
    }

    /// Called by the supervisor's stability timer: `name` has been running
    /// long enough to be considered stable. Resets consecutive-failure count.
    pub fn note_stable(&self, name: &'static str) {
        let mut tasks = self
            .inner
            .tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(state) = tasks.get_mut(name) {
            state.consecutive_failures = 0;
        }
    }

    /// Called by the supervisor after scheduling a restart for `name`.
    /// Increments the restart counter without changing the running state
    /// (the supervisor marks it running again on the next `task_started`).
    pub fn task_restarted(&self, name: &'static str) {
        let mut tasks = self
            .inner
            .tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let state = tasks.entry(name).or_default();
        state.restarts = state.restarts.saturating_add(1);
    }

    // ── Queries ───────────────────────────────────────────────────────────────

    /// The reason `name` was disabled by configuration, if any.
    pub fn disabled_reason(&self, name: &'static str) -> Option<&'static str> {
        self.inner
            .tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .and_then(|s| s.disabled_reason)
    }

    /// Names of required tasks that are not currently running (and not
    /// disabled). Used by `GET /health` to surface dead workers.
    pub fn dead_required_tasks(&self) -> Vec<&'static str> {
        let required = self
            .inner
            .required
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tasks = self
            .inner
            .tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        required
            .iter()
            .copied()
            .filter(|name| {
                tasks
                    .get(name)
                    .map(|s| !s.running && s.disabled_reason.is_none())
                    .unwrap_or(true)
            })
            .collect()
    }

    /// How many required tasks are not disabled. Used by Prometheus:
    /// `stellargate_tasks_expected`.
    pub fn expected_tasks(&self) -> u64 {
        let required = self
            .inner
            .required
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tasks = self
            .inner
            .tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        required
            .iter()
            .filter(|name| {
                tasks
                    .get(*name)
                    .map(|s| s.disabled_reason.is_none())
                    .unwrap_or(true)
            })
            .count() as u64
    }

    /// How many required, non-disabled tasks are currently running.
    /// Used by Prometheus: `stellargate_tasks_live`.
    pub fn live_tasks(&self) -> u64 {
        let required = self
            .inner
            .required
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tasks = self
            .inner
            .tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        required
            .iter()
            .filter(|name| {
                tasks
                    .get(*name)
                    .map(|s| s.running && s.disabled_reason.is_none())
                    .unwrap_or(false)
            })
            .count() as u64
    }

    /// Total task panics across all tasks (not Fatal exits — those are not
    /// panics).
    pub fn failed(&self) -> u64 {
        self.inner.failed.load(Ordering::Relaxed)
    }

    /// Total `task_started` calls across all tasks, including restarts.
    pub fn started(&self) -> u64 {
        self.inner.started.load(Ordering::Relaxed)
    }

    /// Total clean `task_stopped` calls across all tasks.
    pub fn stopped(&self) -> u64 {
        self.inner.stopped.load(Ordering::Relaxed)
    }

    /// Restart count for `name`, or 0 if `name` is unknown.
    pub fn restarts(&self, name: &'static str) -> u64 {
        self.inner
            .tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .map(|s| s.restarts)
            .unwrap_or(0)
    }

    /// Required tasks that have exceeded [`CRASH_LOOP_THRESHOLD`] consecutive
    /// panics. Used by `GET /health` and tests.
    pub fn crash_looping_required_tasks(&self) -> Vec<&'static str> {
        let required = self
            .inner
            .required
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tasks = self
            .inner
            .tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        required
            .iter()
            .copied()
            .filter(|name| {
                tasks
                    .get(name)
                    .map(|s| s.consecutive_failures >= CRASH_LOOP_THRESHOLD)
                    .unwrap_or(false)
            })
            .collect()
    }

    /// Snapshot of all known tasks for Prometheus rendering.
    pub fn snapshot(&self) -> Vec<TaskSnapshot> {
        self.inner
            .tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(name, state)| TaskSnapshot {
                name: name.to_string(),
                restarts: state.restarts,
                running: state.running,
                consecutive_failures: state.consecutive_failures,
                disabled_reason: state.disabled_reason.map(|s| s.to_string()),
            })
            .collect()
    }

    // ── Horizon cursor freshness ──────────────────────────────────────────────

    /// Record a successful Horizon poll or stream event. Updates
    /// `last_success_unix` so `/ready` and Prometheus can track detection lag.
    pub fn note_success(&self) {
        use std::time::{SystemTime, UNIX_EPOCH};
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        self.inner.last_success_unix.store(now, Ordering::Relaxed);
    }

    /// Unix timestamp of the last successful Horizon poll or stream event.
    /// Returns `0` until the first call to [`note_success`].
    pub fn last_success_unix(&self) -> i64 {
        self.inner.last_success_unix.load(Ordering::Relaxed)
    }

    // ── Gateway account existence ─────────────────────────────────────────────

    pub fn set_gateway_account_exists(&self, exists: bool) {
        self.inner
            .gateway_account_exists
            .store(exists, Ordering::Relaxed);
    }

    pub fn gateway_account_exists(&self) -> bool {
        self.inner.gateway_account_exists.load(Ordering::Relaxed)
    }

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
    /// Per-asset gateway trustline state, refreshed at boot and on a
    /// recurring interval thereafter (trustlines can be revoked, or an asset
    /// added to `ACCEPTED_ASSETS`, at any time after boot). Exposed via
    /// `GET /metrics` and consulted by `POST /payments` to reject intents in
    /// an asset currently confirmed unpayable.
    pub trustline_metrics: metrics::TrustlineMetrics,
    /// HTTP request counters and latency histogram, labelled by matched route
    /// and method. Exposed via `GET /metrics` so traffic volume and latency
    /// are queryable facts rather than invisible to an operator.
    pub http_metrics: metrics::HttpMetrics,
    /// Payment lifecycle counters and settlement-latency histogram. Exposed
    /// via `GET /metrics`.
    pub payment_metrics: metrics::PaymentMetrics,
    /// Background task health: tracks started, stopped, and failed task counts
    /// for monitoring and alerting.
    pub task_health: TaskHealth,
}
