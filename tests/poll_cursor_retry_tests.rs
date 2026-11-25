//! Poller cursor and retry behaviour when reconciliation fails (issue #978).
//!
//! `poll_once` used to advance its cursor past every record before reconciling
//! it, and only logged a reconcile error. A transient DB failure on a real
//! payment therefore skipped that payment for good. These tests inject a
//! failure into the settlement write with a SQLite trigger and assert the
//! payment settles on the next cycle instead.

use std::str::FromStr;
use std::sync::Arc;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use stellargate::{
    AppState,
    config::{AcceptedAsset, Config, ListenerMode},
    db, horizon,
};
use tokio::sync::watch;
use wiremock::{Mock, MockServer, ResponseTemplate};

const GATEWAY: &str = "GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5";
/// Key the poller checkpoints its cursor under (`PAYMENT_CURSOR_KEY` in horizon.rs).
const PAYMENT_CURSOR_KEY: &str = "horizon_payment_cursor";
const MEMO: &str = "RETRYMEMO";

async fn make_state(horizon_url: String) -> Arc<AppState> {
    let pool = SqlitePoolOptions::new()
        .connect_with(
            SqliteConnectOptions::from_str("sqlite::memory:")
                .unwrap()
                .create_if_missing(true),
        )
        .await
        .unwrap();
    db::migrate(&pool).await.unwrap();

    Arc::new(AppState {
        pool,
        config: Config {
            port: 0,
            bind_addr: std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            database_url: "sqlite::memory:".into(),
            network: "testnet".into(),
            horizon_url,
            gateway_public: GATEWAY.into(),
            accepted_assets: vec![AcceptedAsset {
                code: "XLM".into(),
                issuer: None,
            }],
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
            rate_limit_requests_per_sec: 10000,
            db_pool_max_connections: 5,
            db_busy_timeout_ms: 5000,
            cors_allowed_origins: vec![],
            listener_mode: ListenerMode::Poll,
            webhook_allow_private_targets: true,
            admin_provisioning_secret: String::new(),
            metrics_token: String::new(),
            request_timeout_secs: 30,
            stream_idle_timeout_secs: 30,
            trusted_proxy_cidrs: vec![],
        },
        http: reqwest::Client::new(),
        webhook_http: reqwest::Client::new(),
        webhook_metrics: stellargate::metrics::WebhookMetrics::new(),
        auth_metrics: stellargate::metrics::AuthMetrics::new(),
        horizon_metrics: stellargate::metrics::HorizonMetrics::new(),
        trustline_metrics: stellargate::metrics::TrustlineMetrics::new(),
        http_metrics: stellargate::metrics::HttpMetrics::new(),
        payment_metrics: stellargate::metrics::PaymentMetrics::new(),
        task_health: stellargate::TaskHealth::new(),
    })
}

/// RFC 3339 timestamp for a synthetic record token.
fn ts(token: u32) -> String {
    format!("2024-01-01T00:00:{token:02}Z")
}

fn record_json(token: u32, memo: Option<&str>) -> serde_json::Value {
    let mut tx = serde_json::json!({ "successful": true });
    if let Some(m) = memo {
        tx["memo"] = serde_json::json!(m);
        tx["memo_type"] = serde_json::json!("text");
    }
    serde_json::json!({
        "type": "payment",
        "amount": "5.0000000",
        "asset_type": "native",
        "to": GATEWAY,
        "transaction_hash": format!("TX{token}"),
        "transaction": tx,
        "paging_token": token.to_string(),
        "created_at": ts(token),
    })
}

/// Serves three payments in ascending order: token 1 is noise, token 2 pays
/// the pending intent, token 3 is noise. Honours `cursor` like Horizon does.
async fn mount_three_payments(server: &MockServer) {
    Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path(format!(
            "/accounts/{GATEWAY}/payments"
        )))
        .respond_with(move |req: &wiremock::Request| {
            let query: std::collections::HashMap<_, _> = req.url.query_pairs().collect();
            let cursor: u32 = query
                .get("cursor")
                .and_then(|c| c.parse().ok())
                .unwrap_or(0);
            let records: Vec<_> = (1..=3u32)
                .filter(|t| *t > cursor)
                .map(|t| record_json(t, (t == 2).then_some(MEMO)))
                .collect();
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "_embedded": { "records": records } }))
        })
        .mount(server)
        .await;
}

async fn payment_status(state: &AppState) -> String {
    db::find_by_memo_any_status(&state.pool, MEMO)
        .await
        .unwrap()
        .expect("intent must exist")
        .status
}

/// A failed settlement write must fail the cycle, hold the cursor before the
/// failed record, and leave the intent pending. The next cycle then settles it.
#[tokio::test]
async fn payment_settles_on_the_cycle_after_a_db_failure() {
    let server = MockServer::start().await;
    mount_three_payments(&server).await;
    let state = make_state(server.uri()).await;

    db::create_payment(
        &state.pool,
        db::NewPayment {
            id: "pay_retry",
            merchant_id: "merchant1",
            destination_address: GATEWAY,
            memo: MEMO,
            amount: "5.0000000",
            asset: "XLM",
            asset_issuer: None,
            webhook_url: None,
            ttl_secs: 3600,
        },
    )
    .await
    .unwrap();
    db::set_state(&state.pool, PAYMENT_CURSOR_KEY, "0")
        .await
        .unwrap();

    // Inject a DB failure on the status write that settles the intent.
    sqlx::query(
        "CREATE TRIGGER fail_settle BEFORE UPDATE OF status ON payments \
         BEGIN SELECT RAISE(ABORT, 'injected failure'); END;",
    )
    .execute(&state.pool)
    .await
    .unwrap();

    let (_shutdown_tx, shutdown_rx) = watch::channel(false);
    let first = horizon::poll_once(&state, &shutdown_rx).await;
    assert!(first.is_err(), "a failed reconcile must fail the cycle");
    assert_eq!(
        db::get_state(&state.pool, PAYMENT_CURSOR_KEY)
            .await
            .unwrap()
            .as_deref(),
        Some("1"),
        "cursor must stop at the last good record, before the failed payment"
    );
    assert_eq!(payment_status(&state).await, "pending");

    // Clear the fault. The retry must settle the payment exactly once.
    sqlx::query("DROP TRIGGER fail_settle")
        .execute(&state.pool)
        .await
        .unwrap();
    let settled = horizon::poll_once(&state, &shutdown_rx)
        .await
        .expect("retry cycle must succeed");
    assert_eq!(settled, 1, "the failed payment is settled on the retry");
    assert_eq!(payment_status(&state).await, "completed");
    assert_eq!(
        db::get_state(&state.pool, PAYMENT_CURSOR_KEY)
            .await
            .unwrap()
            .as_deref(),
        Some("3"),
        "cursor advances past the page once everything reconciled"
    );
}
