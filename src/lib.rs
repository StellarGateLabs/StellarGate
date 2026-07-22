pub mod api;
pub mod config;
pub mod db;
pub mod expiry;
pub mod horizon;
pub mod metrics;
pub mod money;
pub mod ssrf;
pub mod strkey;
pub mod webhook;

/// Shared application state handed to every request handler and the background
/// Horizon poller. Cloning is cheap — the pool and HTTP client are internally
/// reference-counted.
pub struct AppState {
    pub pool: db::Db,
    pub config: config::Config,
    pub http: reqwest::Client,
    pub webhook_http: reqwest::Client,
    pub webhook_metrics: metrics::WebhookMetrics,
    pub task_health: metrics::TaskHealth,
}
