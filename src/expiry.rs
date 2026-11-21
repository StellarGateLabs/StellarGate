//! Expiry sweeper: retiring pending payment intents that were never paid.
//!
//! Every intent is created with an `expires_at` timestamp (`created_at +
//! PAYMENT_TTL_SECS`). A background task periodically transitions any `pending`
//! intent past that deadline to the terminal `expired` status and fires a
//! `payment.expired` webhook, so the Horizon poller stops watching dead intents
//! and merchants get a definite signal that the intent will never settle.
//!
//! Expiry is purely time- and database-driven, so the sweeper runs even when no
//! Stellar gateway is configured (unlike the Horizon poller).
//!
//! `cancelled` is another terminal state (see `POST /v1/payments/:id/cancel`).
//! A cancelled intent is no longer `pending`, so `db::expire_overdue` — which
//! only selects rows with `status = 'pending'` — never touches it: the sweeper
//! must not re-expire a cancelled intent or fire a spurious `payment.expired`.
=======

use crate::supervise::TaskExit;
use crate::{AppState, db, webhook};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tracing::{debug, info, warn};

/// The webhook event emitted when an intent is swept to `expired`.
const EXPIRED_EVENT: &str = "payment.expired";

/// Maximum intents expired per database round trip (issue #885). A backlog
/// larger than this is drained over several batches within one sweep.
const EXPIRY_BATCH_SIZE: i64 = 500;

/// Run one sweep: expire overdue pending intents in bounded batches until a
/// short batch comes back, firing each expired intent's webhook. Returns how
/// many intents were expired. Safe to call repeatedly — an intent is
/// transitioned at most once.
pub async fn sweep_once(state: &Arc<AppState>) -> anyhow::Result<usize> {
    let batch_size = EXPIRY_BATCH_SIZE;
    let mut total = 0usize;
    loop {
        let expired = db::expire_overdue(&state.pool, batch_size).await?;
        let n = expired.len();
        for payment in &expired {
            info!(payment_id = %payment.id, "payment intent expired");
            webhook::dispatch(state, payment, EXPIRED_EVENT, None).await;
        }
        total += n;
        // A short batch means the backlog is drained; stop looping.
        if (n as i64) < batch_size {
            break;
        }
    }
    Ok(total)
}

/// Background loop that sweeps expired intents on the configured poll interval
/// until the process shuts down.
///
/// The first sweep runs immediately at startup, so intents that expired while
/// the process was down are expired straight away rather than after a full
/// interval. Later sweeps wait the configured interval.
pub async fn run_sweeper(state: Arc<AppState>, mut shutdown: watch::Receiver<bool>) -> TaskExit {
    let interval = Duration::from_secs(state.config.expiry_sweep_interval_secs.max(1));
    info!(
        ttl_secs = state.config.payment_ttl_secs,
        interval_secs = state.config.expiry_sweep_interval_secs,
        "expiry sweeper started"
    );

    // Zero for the first pass: sweep once right away, then wait the interval.
    let mut wait = Duration::ZERO;
    loop {
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = shutdown.changed() => {
                info!("expiry sweeper shutting down");
                return TaskExit::ShutdownRequested;
            }
        }
        wait = interval;
        match sweep_once(&state).await {
            Ok(0) => debug!("sweep: nothing to expire"),
            Ok(n) => info!(expired = n, "sweep expired payments"),
            Err(e) => warn!(error = %e, "sweep cycle failed"),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::config::{AcceptedAsset, Config, ListenerMode};
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    pub(crate) fn test_config(webhook_url_allowed: bool) -> Config {
        Config {
            port: 0,
            bind_addr: std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            database_url: "sqlite::memory:".into(),
            network: "testnet".into(),
            horizon_url: String::new(),
            gateway_public: "GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5".into(),
            accepted_assets: AcceptedAsset::default_list(),
            webhook_secret: "a-very-long-and-secure-webhook-signing-secret-32-chars".into(),
            webhook_retry_attempts: 1,
            webhook_retry_delay_ms: 0,
            webhook_retry_max_delay_ms: 60_000,
            allowed_webhook_schemes: vec!["https".into()],
            webhook_timeout_secs: 10,
            webhook_redrive_interval_secs: 30,
            webhook_redrive_concurrency: 4,
            webhook_redrive_max_attempts: 8,
            webhook_redrive_grace_secs: 60,
            webhook_redrive_backoff_initial_secs: 0,
            webhook_redrive_backoff_max_secs: 0,
            retention_interval_secs: 3600,
            webhook_delivery_retention_days: 30,
            idempotency_retention_days: 7,
            poll_interval_secs: 10,
            expiry_sweep_interval_secs: 10,
            trustline_check_interval_secs: 3600,
            poll_max_pages_per_cycle: 50,
            payment_ttl_secs: 3600,
            rate_limit_requests_per_sec: 1000,
            db_pool_max_connections: 5,
            db_busy_timeout_ms: 5000,
            cors_allowed_origins: vec![],
            listener_mode: ListenerMode::Poll,
            // Allow loopback targets so tests can dispatch to a wiremock server.
            webhook_allow_private_targets: webhook_url_allowed,
            admin_provisioning_secret: String::new(),
            metrics_token: String::new(),
            request_timeout_secs: 30,
            stream_idle_timeout_secs: 30,
            trusted_proxy_cidrs: vec![],
        }
    }

    pub(crate) async fn memory_state(cfg: Config) -> AppState {
        let pool = SqlitePoolOptions::new()
            .connect_with(
                SqliteConnectOptions::from_str(&cfg.database_url)
                    .unwrap()
                    .create_if_missing(true),
            )
            .await
            .unwrap();
        db::migrate(&pool).await.unwrap();
        AppState {
            pool,
            config: cfg,
            http: reqwest::Client::new(),
            webhook_http: reqwest::Client::new(),
            webhook_metrics: crate::metrics::WebhookMetrics::new(),
            auth_metrics: crate::metrics::AuthMetrics::new(),
            horizon_metrics: crate::metrics::HorizonMetrics::new(),
            trustline_metrics: crate::metrics::TrustlineMetrics::new(),
            http_metrics: crate::metrics::HttpMetrics::new(),
            payment_metrics: crate::metrics::PaymentMetrics::new(),
            task_health: crate::TaskHealth::new(),
        }
    }

    /// End-to-end coverage for the `payment.expired` webhook: an overdue
    /// intent swept by `sweep_once` must fire a real, correctly-signed HTTP
    /// POST to the merchant's webhook endpoint (issue #156 — this path had no
    /// test beyond the sweeper's own DB-level unit tests).
    #[tokio::test]
    async fn sweep_fires_payment_expired_webhook_end_to_end() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let state = Arc::new(memory_state(test_config(true)).await);
        let webhook_url = format!("{}/hook", server.uri());
        let payment = db::create_payment(
            &state.pool,
            db::NewPayment {
                id: "pay_expiring",
                merchant_id: "merchant1",
                destination_address: "GDESTINATION",
                memo: "EXPMEMO",
                amount: "10",
                asset: "XLM",
                asset_issuer: None,
                webhook_url: Some(&webhook_url),
                // Already overdue: sweep_once must expire it immediately.
                ttl_secs: -10,
            },
        )
        .await
        .expect("expired payment fixture should be created");

        let expired_count = sweep_once(&state).await.unwrap();
        assert_eq!(expired_count, 1, "the overdue intent must be swept");

        let updated = db::get_payment(&state.pool, &payment.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.status, "expired");

        let received = server
            .received_requests()
            .await
            .expect("mock server should capture the expiry webhook");
        assert_eq!(received.len(), 1, "exactly one webhook POST must be sent");
        let req = &received[0];
        assert_eq!(
            req.headers.get("X-StellarGate-Event").unwrap(),
            EXPIRED_EVENT
        );

        let body: serde_json::Value =
            serde_json::from_slice(&req.body).expect("expiry webhook body should be valid JSON");
        assert_eq!(body["event"], EXPIRED_EVENT);
        assert_eq!(body["payment_id"], payment.id);
        assert_eq!(body["status"], "expired");

        // The signature is valid over the body the mock server actually received.
        let timestamp: i64 = req
            .headers
            .get("X-StellarGate-Timestamp")
            .expect("expiry webhook should include a timestamp header")
            .to_str()
            .unwrap()
            .parse()
            .expect("timestamp header should be an integer");
        let signature = req
            .headers
            .get("X-StellarGate-Signature")
            .expect("expiry webhook should include a signature header")
            .to_str()
            .unwrap();
        assert!(
            webhook::verify_signature(
                &state.config.webhook_secret,
                timestamp,
                &req.body,
                signature
            ),
            "expiry webhook signature must verify against the received body"
        );
    }

    /// Issue #984: with a long poll interval, the first sweep must still run
    /// at startup rather than after a full interval. An intent that expired
    /// while the process was down is expired within seconds of boot.
    #[tokio::test]
    async fn sweeper_runs_its_first_pass_at_startup_without_waiting_an_interval() {
        let mut cfg = test_config(true);
        cfg.poll_interval_secs = 3600;
        cfg.expiry_sweep_interval_secs = 3600;
        let state = Arc::new(memory_state(cfg).await);
        let payment = db::create_payment(
            &state.pool,
            db::NewPayment {
                id: "pay_overdue_at_boot",
                merchant_id: "merchant1",
                destination_address: "GDESTINATION",
                memo: "BOOTMEMO",
                amount: "10",
                asset: "XLM",
                asset_issuer: None,
                webhook_url: None,
                ttl_secs: -10,
            },
        )
        .await
        .expect("overdue payment fixture should be created");

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let sweeper = tokio::spawn(run_sweeper(state.clone(), shutdown_rx));

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let current = db::get_payment(&state.pool, &payment.id)
                .await
                .unwrap()
                .unwrap();
            if current.status == "expired" {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the first sweep must run at startup, not after POLL_INTERVAL_SECS"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        shutdown_tx.send(true).unwrap();
        sweeper.await.unwrap();
    }
}
