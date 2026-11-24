//! StellarGate binary entry point: boots configuration, storage and HTTP
//! clients, spawns the background listeners, serves the API, and drains
//! everything on shutdown.

use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use stellargate::{
    AppState, TaskHealth, api,
    config::{Config, ListenerMode},
    db, expiry, horizon,
    metrics::{
        AuthMetrics, HorizonMetrics, HttpMetrics, PaymentMetrics, TrustlineMetrics, WebhookMetrics,
    },
    retention, supervise, webhook,
};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

const USER_AGENT: &str = concat!("StellarGate/", env!("CARGO_PKG_VERSION"));

/// Timeout for general outbound HTTP (Horizon). Webhook delivery uses its own
/// configurable per-attempt timeout instead.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Overall shutdown deadline: the process exits after this even if HTTP or
/// background tasks are still draining, so it stays within the orchestrator's
/// stop grace period.
const DEFAULT_SHUTDOWN_GRACE_SECS: u64 = 30;

/// Timeout for the `healthcheck` subcommand's probe request. Kept short so a
/// hung server fails the probe quickly instead of stalling the orchestrator.
const HEALTHCHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// Default probe path when the subcommand is invoked without one.
const HEALTHCHECK_DEFAULT_PATH: &str = "health";

fn init_tracing() -> Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    match std::env::var("LOG_FORMAT")
        .unwrap_or_else(|_| "text".into())
        .to_ascii_lowercase()
        .as_str()
    {
        "text" | "" => {
            tracing_subscriber::fmt().with_env_filter(filter).init();
        }
        "json" => {
            tracing_subscriber::fmt()
                .json()
                .flatten_event(true)
                .with_env_filter(filter)
                .init();
        }
        other => {
            anyhow::bail!(
                "LOG_FORMAT={other:?} is not recognised; valid values are \"text\" or \"json\""
            );
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    // Load `.env` first so every setting in it is visible to the code below:
    // `RUST_LOG` to the tracing filter (#965), and `PORT` to the `healthcheck`
    // probe (#966). Variables already set in the process environment win.
    dotenvy::dotenv().ok();

    // Handle the `healthcheck` subcommand before anything else: no config
    // loading, tracing init, DB open, migrations, Horizon calls, or port
    // binding. Container probes must not start a second server.
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("healthcheck") {
        let path = args
            .get(2)
            .map(String::as_str)
            .unwrap_or(HEALTHCHECK_DEFAULT_PATH);
        let code = run_healthcheck(path).await;
        std::process::exit(code);
    }

    init_tracing()?;

    let cfg = Config::from_env()?;
    // `open_pool` builds every connection through `db::connect_options`, which
    // enables `PRAGMA foreign_keys = ON`, so referential integrity is enforced
    // on each pooled connection without any extra setup here.
    let pool = db::open_pool(
        &cfg.database_url,
        cfg.db_pool_max_connections,
        Duration::from_millis(cfg.db_busy_timeout_ms),
    )
    .await?;
    db::migrate(&pool).await?;
    /* One-off, best-effort reconstruction of `payments.asset_issuer` for rows
    created before the column existed. Needs the accepted-asset allow-list, so
    it runs here rather than inside `migrate`. */
    db::backfill_asset_issuers(&pool, &cfg.accepted_assets).await?;

    let state = Arc::new(AppState {
        pool,
        http: http_client(HTTP_TIMEOUT)?,
        webhook_http: http_client(Duration::from_secs(cfg.webhook_timeout_secs))?,
        webhook_metrics: WebhookMetrics::new(),
        auth_metrics: AuthMetrics::new(),
        horizon_metrics: HorizonMetrics::new(),
        trustline_metrics: TrustlineMetrics::new(),
        http_metrics: HttpMetrics::new(),
        payment_metrics: PaymentMetrics::new(),
        task_health: TaskHealth::new(),
        config: cfg,
    });

    if state.config.gateway_configured() {
        report_trustlines(&state).await;
    }

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let health = state.task_health.clone();

    // Required workers: a panic must be restarted, not silently dropped.
    health.require("poller");
    health.require("sweeper");
    health.require("retention");
    health.require("trustline_checker");
    health.require("redrive");
    if state.config.listener_mode == ListenerMode::Stream {
        health.require("stream");
    }

    let stream = (state.config.listener_mode == ListenerMode::Stream).then(|| {
        supervise::supervise(health.clone(), "stream", shutdown_rx.clone(), {
            let (state, rx) = (state.clone(), shutdown_rx.clone());
            move || horizon::run_stream_listener(state.clone(), rx.clone())
        })
    });
    let poller = supervise::supervise(health.clone(), "poller", shutdown_rx.clone(), {
        let (state, rx) = (state.clone(), shutdown_rx.clone());
        move || horizon::run_poller(state.clone(), rx.clone())
    });
    let sweeper = supervise::supervise(health.clone(), "sweeper", shutdown_rx.clone(), {
        let (state, rx) = (state.clone(), shutdown_rx.clone());
        move || expiry::run_sweeper(state.clone(), rx.clone())
    });
    let retention = supervise::supervise(health.clone(), "retention", shutdown_rx.clone(), {
        let (state, rx) = (state.clone(), shutdown_rx.clone());
        move || retention::run_retention_worker(state.clone(), rx.clone())
    });
    let trustline_checker =
        supervise::supervise(health.clone(), "trustline_checker", shutdown_rx.clone(), {
            let (state, rx) = (state.clone(), shutdown_rx.clone());
            move || horizon::run_trustline_checker(state.clone(), rx.clone())
        });
    let redrive = supervise::supervise(health.clone(), "redrive", shutdown_rx.clone(), {
        let (state, rx) = (state.clone(), shutdown_rx.clone());
        move || webhook::run_redrive_worker(state.clone(), rx.clone())
    });

    let addr = SocketAddr::new(state.config.bind_addr, state.config.port);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!("StellarGate API listening on {addr}");

    let shutdown_grace = Duration::from_secs(
        std::env::var("SHUTDOWN_GRACE_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_SHUTDOWN_GRACE_SECS),
    );

    // One overall deadline covers both the HTTP drain and the worker drain, so
    // the process can never overrun the orchestrator's stop grace period.
    let deadline = tokio::time::Instant::now() + shutdown_grace;

    // Signal the workers at the same time as the HTTP graceful shutdown starts
    // so both drain in parallel rather than sequentially.
    let shutdown_tx_signal = shutdown_tx.clone();
    let http_shutdown = async move {
        shutdown_signal().await;
        let _ = shutdown_tx_signal.send(true);
    };

    let serve = axum::serve(
        listener,
        api::router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(http_shutdown);

    let drain = async {
        join_task(poller, "poller", &health).await;
        join_task(sweeper, "sweeper", &health).await;
        join_task(redrive, "redrive", &health).await;
        join_task(retention, "retention", &health).await;
        join_task(trustline_checker, "trustline_checker", &health).await;
        if let Some(handle) = stream {
            join_task(handle, "stream", &health).await;
        }
    };

    let shutdown = async {
        if let Err(e) = serve.await {
            warn!(error = %e, "HTTP server error during shutdown");
        }
        drain.await;
    };

    if tokio::time::timeout_at(deadline, shutdown).await.is_err() {
        let running: Vec<String> = health
            .snapshot()
            .into_iter()
            .filter(|task| task.running)
            .map(|task| task.name)
            .collect();
        warn!(
            timeout_secs = shutdown_grace.as_secs(),
            ?running,
            "shutdown deadline reached; forcing exit with tasks still running"
        );
    }

    info!("shutdown complete");
    Ok(())
}

/// Build the probe URL for the `healthcheck` subcommand. Trims any leading
/// slashes from `path` so both `health` and `/health` produce the same URL.
fn healthcheck_url(port: u16, path: &str) -> String {
    let path = path.trim_start_matches('/');
    let path = if path.is_empty() {
        HEALTHCHECK_DEFAULT_PATH
    } else {
        path
    };
    format!("http://127.0.0.1:{port}/{path}")
}

/// Resolve the port the `healthcheck` subcommand probes from the raw `PORT`
/// value: the parsed port when it is a valid `u16`, otherwise the server's
/// default of 3000.
fn healthcheck_port(raw: Option<&str>) -> u16 {
    raw.and_then(|p| p.parse::<u16>().ok()).unwrap_or(3000)
}

/// Probe the local server's health endpoint and return the process exit code:
/// `0` when the endpoint answers with a success status, `1` otherwise.
async fn run_healthcheck(path: &str) -> i32 {
    let port = healthcheck_port(std::env::var("PORT").ok().as_deref());
    let url = healthcheck_url(port, path);

    let client = match reqwest::Client::builder()
        .timeout(HEALTHCHECK_TIMEOUT)
        .user_agent(USER_AGENT)
        .build()
    {
        Ok(client) => client,
        Err(e) => {
            eprintln!("healthcheck: failed to build HTTP client: {e}");
            return 1;
        }
    };

    match client.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => 0,
        Ok(resp) => {
            eprintln!("healthcheck: {url} returned {}", resp.status());
            1
        }
        Err(e) => {
            eprintln!("healthcheck: {url} failed: {e}");
            1
        }
    }
}

fn http_client(timeout: Duration) -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(timeout)
        .user_agent(USER_AGENT)
        .build()?)
}

/// Check whether every accepted asset has a trustline on the gateway account.
/// Advisory only: a missing trustline doesn't block boot, it just means
/// payments in that asset will bounce until the trustline is added.
///
/// `check_trustlines` logs the outcome itself, so this logs only a failure to
/// check; a successful boot check produces exactly one summary line.
async fn report_trustlines(state: &Arc<AppState>) {
    if let Err(e) = horizon::check_trustlines(state).await {
        warn!(error = %e, "could not verify gateway trustlines at startup");
    }
}

/// Await a supervised background task. A `JoinError` means the supervisor
/// itself panicked, which is recorded so the failure counter — and any alert
/// watching it — fires.
async fn join_task(handle: JoinHandle<()>, name: &'static str, health: &TaskHealth) {
    if let Err(e) = handle.await
        && e.is_panic()
    {
        warn!("background task panicked");
        health.task_failed(name);
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            tracing::warn!(error = %err, "failed to install Ctrl-C handler");
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(err) => {
                tracing::warn!(error = %err, "failed to install SIGTERM handler");
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    info!("shutdown signal received");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn healthcheck_port_uses_configured_value() {
        assert_eq!(healthcheck_port(Some("8080")), 8080);
    }

    #[test]
    fn healthcheck_port_defaults_to_3000_when_unset() {
        assert_eq!(healthcheck_port(None), 3000);
    }

    #[test]
    fn healthcheck_port_falls_back_on_unparseable_value() {
        assert_eq!(healthcheck_port(Some("not-a-port")), 3000);
        assert_eq!(healthcheck_port(Some("70000")), 3000);
    }

    #[test]
    fn healthcheck_url_uses_port_and_path() {
        assert_eq!(
            healthcheck_url(3000, "health"),
            "http://127.0.0.1:3000/health"
        );
        assert_eq!(
            healthcheck_url(8080, "ready"),
            "http://127.0.0.1:8080/ready"
        );
    }

    #[test]
    fn healthcheck_url_trims_leading_slashes() {
        assert_eq!(
            healthcheck_url(3000, "/health"),
            "http://127.0.0.1:3000/health"
        );
        assert_eq!(
            healthcheck_url(3000, "///ready"),
            "http://127.0.0.1:3000/ready"
        );
    }

    #[test]
    fn healthcheck_url_falls_back_to_default_path() {
        assert_eq!(healthcheck_url(3000, ""), "http://127.0.0.1:3000/health");
        assert_eq!(healthcheck_url(3000, "/"), "http://127.0.0.1:3000/health");
    }
}
