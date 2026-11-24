//! Payments that arrive after an intent's `expires_at` but before the expiry
//! sweeper has run (issue #979).
//!
//! The sweeper runs on its own interval, so an overdue intent still reads
//! `pending`. The active lookup skips overdue rows, which used to drop such a
//! payment silently. Policy: a payment made on time settles; a late one is
//! reported as `payment.unexpected` and the status is left to the sweeper.

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
const PAYMENT_CURSOR_KEY: &str = "horizon_payment_cursor";
const MEMO: &str = "OVERDUE1";
/// The intent's deadline. Well in the past, so the intent is overdue.
const DEADLINE: &str = "2024-06-01T00:00:00Z";

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
            allowed_webhook_schemes: vec!["https".into(), "http".into()],
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

/// One Horizon payment for the intent, stamped with `paid_at` as its ledger time.
async fn mount_one_payment(server: &MockServer, paid_at: &'static str) {
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
            let records: Vec<serde_json::Value> = if cursor < 1 {
                vec![serde_json::json!({
                    "type": "payment",
                    "amount": "5.0000000",
                    "asset_type": "native",
                    "to": GATEWAY,
                    "transaction_hash": "TXOVERDUE",
                    "transaction": { "successful": true, "memo": MEMO, "memo_type": "text" },
                    "paging_token": "1",
                    "created_at": paid_at,
                })]
            } else {
                vec![]
            };
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "_embedded": { "records": records } }))
        })
        .mount(server)
        .await;
}

/// Create a pending intent whose deadline has passed but which the sweeper has
/// not yet expired, and point the poller's cursor at the start of history.
async fn overdue_pending_intent(state: &AppState, webhook_url: Option<&str>) {
    db::create_payment(
        &state.pool,
        db::NewPayment {
            id: "pay_overdue",
            merchant_id: "merchant1",
            destination_address: GATEWAY,
            memo: MEMO,
            amount: "5.0000000",
            asset: "XLM",
            asset_issuer: None,
            webhook_url,
            ttl_secs: 3600,
        },
    )
    .await
    .unwrap();
    sqlx::query("UPDATE payments SET expires_at = ? WHERE id = 'pay_overdue'")
        .bind(DEADLINE)
        .execute(&state.pool)
        .await
        .unwrap();
    db::set_state(&state.pool, PAYMENT_CURSOR_KEY, "0")
        .await
        .unwrap();
}

async fn status_of(state: &AppState) -> String {
    db::find_by_memo_any_status(&state.pool, MEMO)
        .await
        .unwrap()
        .expect("intent must exist")
        .status
}

/// Paid before the deadline and reconciled after it: settled, as the customer
/// was on time.
#[tokio::test]
async fn payment_made_before_deadline_settles_even_after_it_passed() {
    let server = MockServer::start().await;
    mount_one_payment(&server, "2024-05-31T23:00:00Z").await;
    let state = make_state(server.uri()).await;
    overdue_pending_intent(&state, None).await;

    let (_tx, rx) = watch::channel(false);
    let settled = horizon::poll_once(&state, &rx).await.unwrap();

    assert_eq!(settled, 1, "an on-time payment must not be dropped");
    assert_eq!(status_of(&state).await, "completed");
}

/// Paid after the deadline but before the sweeper ran: the intent is not
/// settled, the merchant is told via `payment.unexpected`, and the status is
/// left for the sweeper.
#[tokio::test]
async fn late_payment_is_reported_as_unexpected_not_dropped() {
    let server = MockServer::start().await;
    mount_one_payment(&server, "2024-06-01T01:00:00Z").await;
    let hook = format!("{}/hook", server.uri());
    Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/hook"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let state = make_state(server.uri()).await;
    overdue_pending_intent(&state, Some(&hook)).await;

    let (_tx, rx) = watch::channel(false);
    let settled = horizon::poll_once(&state, &rx).await.unwrap();

    assert_eq!(settled, 0, "a late payment must not settle the intent");
    assert_eq!(
        status_of(&state).await,
        "pending",
        "the sweeper owns the expiry"
    );

    let deliveries: Vec<serde_json::Value> = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.url.path() == "/hook")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert_eq!(
        deliveries.len(),
        1,
        "exactly one webhook for the late payment"
    );
    assert_eq!(deliveries[0]["event"], "payment.unexpected");
    server.verify().await;
}
