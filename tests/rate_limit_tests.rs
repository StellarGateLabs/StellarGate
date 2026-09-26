//! Rate-limit behaviour lives in its own integration binary on purpose.
//!
//! The broader API tests run at a high limit and exercise merchant auth heavily.
//! Keeping the low-quota assertion here makes the expected 429 path explicit.

use axum::http::StatusCode;
use axum_test::TestServer;
use serde_json::{Value, json};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::future::IntoFuture;
use std::str::FromStr;
use std::sync::Arc;
use stellargate::{
    AppState, api,
    config::{Config, ListenerMode},
    db,
};

fn make_config(rate_limit_requests_per_sec: u32) -> Config {
    Config {
        port: 0,
        database_url: "sqlite::memory:".into(),
        network: "testnet".into(),
        horizon_url: String::new(),
        gateway_public: "UNCONFIGURED".into(),
        accepted_assets: stellargate::config::AcceptedAsset::default_list(),
        webhook_secret: String::new(),
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
        poll_max_pages_per_cycle: 50,
        payment_ttl_secs: 3600,
        rate_limit_requests_per_sec,
        db_pool_max_connections: 10,
        db_busy_timeout_ms: 5000,
        cors_allowed_origins: vec![],
        listener_mode: ListenerMode::Poll,
        webhook_allow_private_targets: false,
        admin_provisioning_secret: TEST_ADMIN_SECRET.into(),
        metrics_token: String::new(),
        request_timeout_secs: 30,
        stream_idle_timeout_secs: 30,
        trusted_proxy_cidrs: vec![],
    }
}

const TEST_ADMIN_SECRET: &str = "test-admin-secret";

async fn server_with_config(cfg: Config) -> (TestServer, db::Db) {
    let pool = SqlitePoolOptions::new()
        .connect_with(
            SqliteConnectOptions::from_str(&cfg.database_url)
                .unwrap()
                .create_if_missing(true),
        )
        .await
        .unwrap();
    db::migrate(&pool).await.unwrap();
    let http = reqwest::Client::new();
    let router = api::router(Arc::new(AppState {
        pool: pool.clone(),
        config: cfg,
        http,
        webhook_http: reqwest::Client::new(),
        webhook_metrics: stellargate::metrics::WebhookMetrics::new(),
        auth_metrics: stellargate::metrics::AuthMetrics::new(),
        horizon_metrics: stellargate::metrics::HorizonMetrics::new(),
        trustline_metrics: stellargate::metrics::TrustlineMetrics::new(),
        http_metrics: stellargate::metrics::HttpMetrics::new(),
        payment_metrics: stellargate::metrics::PaymentMetrics::new(),
        task_health: stellargate::TaskHealth::new(),
    }))
    .into_make_service_with_connect_info::<std::net::SocketAddr>();
    (TestServer::new(router), pool)
}

async fn provision_merchant(server: &TestServer) -> String {
    let res = server
        .post("/merchants")
        .add_header("X-Admin-Secret", TEST_ADMIN_SECRET)
        .await;
    res.assert_status(StatusCode::CREATED);
    res.json::<Value>()["api_key"].as_str().unwrap().to_string()
}

fn header(res: &axum_test::TestResponse, name: &str) -> u64 {
    res.headers()
        .get(name)
        .unwrap_or_else(|| panic!("response is missing the {name} header"))
        .to_str()
        .unwrap()
        .parse()
        .unwrap()
}

#[tokio::test]
async fn rate_limit_headers_track_quota_before_and_after_exhaustion() {
    let (server, _pool) = server_with_config(make_config(2)).await;
    let _key = provision_merchant(&server).await;
    let auth = "******";
    let body = json!({ "amount": "1", "asset": "XLM" });

    let first = server
        .post("/v1/payments")
        .add_header("Authorization", auth)
        .json(&body)
        .await;
    first.assert_status(StatusCode::CREATED);
    assert_eq!(header(&first, "x-ratelimit-limit"), 2);
    assert_eq!(header(&first, "x-ratelimit-remaining"), 1);

    let second = server
        .post("/v1/payments")
        .add_header("Authorization", auth)
        .json(&body)
        .await;
    second.assert_status(StatusCode::CREATED);
    assert_eq!(header(&second, "x-ratelimit-limit"), 2);
    assert_eq!(header(&second, "x-ratelimit-remaining"), 0);

    let throttled = server
        .post("/v1/payments")
        .add_header("Authorization", auth)
        .json(&body)
        .await;
    throttled.assert_status(StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(header(&throttled, "x-ratelimit-limit"), 2);
    assert_eq!(header(&throttled, "x-ratelimit-remaining"), 0);
    assert!(header(&throttled, "x-ratelimit-reset") >= header(&throttled, "retry-after"));
}

#[tokio::test]
async fn test_rate_limit_exceeded_returns_429() {
    let (server, _pool) = server_with_config(make_config(1)).await;
    let key = provision_merchant(&server).await;
    let auth = format!("Bearer {key}");

    // The first request consumes the single per-second token.
    let first = server
        .post("/payments")
        .add_header("Authorization", auth.clone())
        .json(&json!({ "amount": "1", "asset": "XLM" }))
        .await;
    first.assert_status(StatusCode::CREATED);

    // A second immediate request exceeds the quota and is rejected.
    let second = server
        .post("/payments")
        .add_header("Authorization", auth)
        .json(&json!({ "amount": "1", "asset": "XLM" }))
        .await;
    second.assert_status(StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(second.json::<Value>()["code"], "rate_limit_exceeded");
}

/// Redelivery is rate-limited independently of `POST /payments` — a merchant
/// (or anyone who knows a payment/delivery id) can't use it to trigger
/// unbounded outbound requests to the stored webhook_url.
#[tokio::test]
async fn test_redeliver_rate_limit_exceeded_returns_429() {
    let (server, pool) = server_with_config(make_config(1)).await;
    let key = provision_merchant(&server).await;
    let auth = format!("Bearer {key}");

    let id = server
        .post("/payments")
        .add_header("Authorization", auth.clone())
        .json(&json!({ "amount": "1", "asset": "XLM" }))
        .await
        .json::<Value>()["id"]
        .as_str()
        .unwrap()
        .to_string();

    // A port nothing is listening on: the redelivery attempt fails fast
    // (connection refused) without depending on real network access.
    stellargate::db::save_webhook_delivery(
        &pool,
        "delivery-1",
        &id,
        "http://127.0.0.1:1/hook",
        r#"{"event":"payment.completed"}"#,
        "payment.completed",
    )
    .await
    .unwrap();

    // The first redelivery consumes the single per-second token (its outcome
    // doesn't matter — the rate limiter runs before the handler).
    let first = server
        .post(&format!("/payments/{id}/webhooks/delivery-1/redeliver"))
        .add_header("Authorization", auth.clone())
        .await;
    assert_ne!(first.status_code(), StatusCode::TOO_MANY_REQUESTS);

    // A second immediate redelivery exceeds the quota and is rejected.
    let second = server
        .post(&format!("/payments/{id}/webhooks/delivery-1/redeliver"))
        .add_header("Authorization", auth)
        .await;
    second.assert_status(StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(second.json::<Value>()["code"], "rate_limit_exceeded");
}

/// `POST /merchants` sits in the base-rate "merchants" bucket, not the 5×
/// read bucket — an admin secret leaked or brute-forced can't be used to
/// mass-create merchant records any faster than any other write (issue #461).
#[tokio::test]
async fn test_provision_merchant_rate_limit_exceeded_returns_429() {
    let (server, _pool) = server_with_config(make_config(1)).await;

    let first = server
        .post("/merchants")
        .add_header("X-Admin-Secret", TEST_ADMIN_SECRET)
        .await;
    first.assert_status(StatusCode::CREATED);

    let second = server
        .post("/merchants")
        .add_header("X-Admin-Secret", TEST_ADMIN_SECRET)
        .await;
    second.assert_status(StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(second.json::<Value>()["code"], "rate_limit_exceeded");
}

/// Layer order on redelivery (#634): `auth_middleware` must run before
/// `merchant_redeliver_limit_middleware`. The limiter extracts the
/// `AuthenticatedMerchant` extension auth inserts, so if the order were ever
/// flipped (e.g. by an axum upgrade changing `route_layer` semantics) an
/// unauthenticated call would surface as a 500 missing-extension rejection
/// instead of the auth middleware's JSON 401.
#[tokio::test]
async fn test_redeliver_runs_auth_before_merchant_limiter() {
    let (server, _pool) = server_with_config(make_config(1000)).await;

    for path in [
        "/v1/payments/any/webhooks/any/redeliver",
        "/payments/any/webhooks/any/redeliver",
    ] {
        let res = server.post(path).await;
        res.assert_status(StatusCode::UNAUTHORIZED);
        assert_eq!(
            res.json::<Value>()["code"],
            "unauthorized",
            "{path}: auth must reject before the merchant limiter runs"
        );

        let res = server
            .post(path)
            .add_header("Authorization", "Bearer not-a-real-key")
            .await;
        res.assert_status(StatusCode::UNAUTHORIZED);
    }
}

/// Regression for the per-merchant limiter check-then-act race: with a cold
/// limiter cache, many concurrent requests must share one limiter (atomic
/// `get_with`) rather than each building a fresh full-burst limiter. At most
/// the configured quota (plus at most one cell replenished mid-test) may be
/// admitted.
#[tokio::test]
async fn test_merchant_redeliver_limiter_cold_cache_burst_is_bounded() {
    const QUOTA: u32 = 3;
    const N: usize = 40;

    let (server, _pool) = server_with_config(make_config(QUOTA)).await;
    let key = provision_merchant(&server).await;
    let auth = format!("Bearer {key}");

    // First authenticated redeliver request for this merchant: limiter absent.
    let responses = futures_util::future::join_all((0..N).map(|_| {
        server
            .post("/payments/nope/webhooks/nope/redeliver")
            .add_header("Authorization", auth.clone())
            .into_future()
    }))
    .await;

    let admitted = responses
        .iter()
        .filter(|r| r.status_code() != StatusCode::TOO_MANY_REQUESTS)
        .count();
    assert!(
        admitted <= QUOTA as usize + 1,
        "admitted {admitted} of {N} concurrent requests; quota is {QUOTA}"
    );
}
