pub mod api;
pub mod config;
pub mod db;
pub mod expiry;
pub mod horizon;
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
    /// General-purpose HTTP client used for Horizon API calls (30 s timeout).
    pub http: reqwest::Client,
    /// Dedicated HTTP client for outbound webhook POSTs. Uses the shorter
    /// `WEBHOOK_TIMEOUT_SECS` timeout (default 10 s) so that a slow receiver
    /// cannot block the reconciler or amplify retry latency.
    pub webhook_http: reqwest::Client,
}
