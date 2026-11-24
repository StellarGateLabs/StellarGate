//! Regression tests for closed security and correctness issues.
//!
//! Convention (see CONTRIBUTING.md): every closed security or correctness
//! issue gets one fast, black-box test here, named after the issue it pins
//! (`issue_<number>_<short_description>`). These tests go through the public
//! router only, so they survive refactors of internal modules and are hard to
//! delete accidentally during a conflict resolution.
//!
//! If a test here starts failing, the fix it guards has regressed — do not
//! delete the test, restore the fix.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use std::sync::Arc;
use stellargate::{
    AppState, TaskHealth, api,
    config::{AcceptedAsset, Config, ListenerMode},
    db,
    metrics::{
        AuthMetrics, HorizonMetrics, HttpMetrics, PaymentMetrics, TrustlineMetrics, WebhookMetrics,
    },
};
use tower::ServiceExt;

/// Shared admin secret and metrics token, mirroring `tests/api_tests.rs`.
const TEST_ADMIN_SECRET: &str = "test-admin-secret";
const TEST_METRICS_TOKEN: &str = "test-metrics-token-32-chars-long";

/// Build the application router under test, wired the same way the binary
/// wires it, against a fresh shared-cache in-memory database.
///
/// Kept in one place so the guard file stays black-box: tests only ever touch
/// the public HTTP surface.
async fn app() -> axum::Router {
    let dsn = format!(
        "sqlite:file:{}?mode=memory&cache=shared",
        uuid::Uuid::new_v4()
    );
    let pool = db::open_pool(&dsn, 2, std::time::Duration::from_millis(5000))
        .await
        .expect("open test pool");
    db::migrate(&pool).await.expect("migrate test pool");

    let config = Config {
        port: 0,
        bind_addr: std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
        database_url: dsn,
        network: "testnet".into(),
        horizon_url: String::new(),
        gateway_public: "UNCONFIGURED".into(),
        accepted_assets: AcceptedAsset::default_list(),
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
        expiry_sweep_interval_secs: 10,
        trustline_check_interval_secs: 3600,
        poll_max_pages_per_cycle: 50,
        payment_ttl_secs: 3600,
        rate_limit_requests_per_sec: 1000,
        db_pool_max_connections: 2,
        db_busy_timeout_ms: 5000,
        cors_allowed_origins: vec![],
        listener_mode: ListenerMode::Poll,
        webhook_allow_private_targets: false,
        admin_provisioning_secret: TEST_ADMIN_SECRET.into(),
        metrics_token: TEST_METRICS_TOKEN.into(),
        request_timeout_secs: 30,
        stream_idle_timeout_secs: 30,
        trusted_proxy_cidrs: vec![],
    };

    api::router(Arc::new(AppState {
        pool,
        config,
        http: reqwest::Client::new(),
        webhook_http: reqwest::Client::new(),
        webhook_metrics: WebhookMetrics::new(),
        auth_metrics: AuthMetrics::new(),
        horizon_metrics: HorizonMetrics::new(),
        trustline_metrics: TrustlineMetrics::new(),
        http_metrics: HttpMetrics::new(),
        payment_metrics: PaymentMetrics::new(),
        task_health: TaskHealth::new(),
    }))
}

/// Send a request through the router and return the response status.
async fn status_of(router: axum::Router, request: Request<Body>) -> StatusCode {
    let response = router.oneshot(request).await.expect("router call failed");
    response.status()
}

/// Regression: #254 — idempotency keys must reject a reused key whose body
/// differs from the original request.
#[tokio::test]
async fn issue_254_idempotency_key_rejects_different_body() {
    let router = app().await;

    let first = Request::builder()
        .method("POST")
        .uri("/payments")
        .header("content-type", "application/json")
        .header("idempotency-key", "regression-254")
        .body(Body::from(r#"{"amount":"10.00","asset":"XLM"}"#))
        .expect("request");
    let first_status = status_of(router.clone(), first).await;
    assert!(
        first_status.is_success() || first_status == StatusCode::ACCEPTED,
        "first request with a fresh idempotency key should be accepted, got {first_status}"
    );

    let second = Request::builder()
        .method("POST")
        .uri("/payments")
        .header("content-type", "application/json")
        .header("idempotency-key", "regression-254")
        .body(Body::from(r#"{"amount":"99.00","asset":"XLM"}"#))
        .expect("request");
    let second_status = status_of(router, second).await;
    assert_eq!(
        second_status,
        StatusCode::CONFLICT,
        "reusing an idempotency key with a different body must be rejected"
    );
}

/// Regression: #261 — payment amounts must be bounded; zero and negative
/// amounts are rejected before reaching the ledger.
#[tokio::test]
async fn issue_261_payment_amount_must_be_positive() {
    let router = app().await;

    let request = Request::builder()
        .method("POST")
        .uri("/payments")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"amount":"0","asset":"XLM"}"#))
        .expect("request");
    let status = status_of(router, request).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a zero payment amount must be rejected"
    );
}

/// Regression: #268 — list endpoints must clamp oversized pagination limits
/// instead of honouring them, so a single request cannot exhaust the DB.
#[tokio::test]
async fn issue_268_pagination_limit_is_clamped() {
    let router = app().await;

    let request = Request::builder()
        .method("GET")
        .uri("/payments?limit=100000")
        .body(Body::empty())
        .expect("request");
    let status = status_of(router, request).await;
    assert!(
        status.is_success(),
        "an oversized pagination limit should be clamped, not rejected, got {status}"
    );
}

/// Regression: #272 — health endpoint must report readiness through the
/// public router so probes can rely on it.
#[tokio::test]
async fn issue_272_health_endpoint_reports_ready() {
    let router = app().await;

    let request = Request::builder()
        .method("GET")
        .uri("/health")
        .body(Body::empty())
        .expect("request");
    let status = status_of(router, request).await;
    assert_eq!(status, StatusCode::OK, "health endpoint must report ready");
}

/// Regression: #279 — memo fields must carry sufficient entropy; a
/// low-entropy memo is rejected rather than silently accepted.
#[tokio::test]
async fn issue_279_low_entropy_memo_is_rejected() {
    let router = app().await;

    let request = Request::builder()
        .method("POST")
        .uri("/payments")
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"amount":"10.00","asset":"XLM","memo":"aaaaaaaa"}"#,
        ))
        .expect("request");
    let status = status_of(router, request).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a low-entropy memo must be rejected"
    );
}

/// Send an admin request and return its status and JSON body (`Null` if none).
async fn admin_call(
    router: axum::Router,
    method: &str,
    uri: &str,
    content_type: Option<&str>,
    body: &str,
) -> (StatusCode, serde_json::Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("x-admin-secret", TEST_ADMIN_SECRET);
    if let Some(ct) = content_type {
        builder = builder.header("content-type", ct);
    }
    let response = router
        .oneshot(builder.body(Body::from(body.to_owned())).expect("request"))
        .await
        .expect("router call failed");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

/// Regression: #1076 — issuing an API key ignored a body that was present but
/// invalid, answering `201` with an unlabelled key. Only an absent or empty
/// body may mean "no label"; anything else must be rejected and issue nothing.
#[tokio::test]
async fn issue_1076_issue_key_rejects_an_invalid_body() {
    let router = app().await;
    let (status, merchant) = admin_call(router.clone(), "POST", "/merchants", None, "").await;
    assert_eq!(status, StatusCode::CREATED);
    let id = merchant["merchant_id"].as_str().expect("merchant_id");
    let keys_uri = &format!("/merchants/{id}/keys");
    let json = Some("application/json");

    let rejected = [
        (json, r#"{"label": 123}"#, StatusCode::BAD_REQUEST),
        (json, r#"{"label":"ci""#, StatusCode::BAD_REQUEST),
        (
            Some("text/plain"),
            r#"{"label":"ci"}"#,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
        ),
        (
            None,
            r#"{"label":"ci"}"#,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
        ),
    ];
    for (ct, body, want) in rejected {
        let (status, err) = admin_call(router.clone(), "POST", keys_uri, ct, body).await;
        assert_eq!(status, want, "body {body:?} with content type {ct:?}");
        if want == StatusCode::BAD_REQUEST {
            assert_eq!(err["code"], "invalid_request", "body {body:?}");
        }
    }
    let (_, listed) = admin_call(router.clone(), "GET", keys_uri, None, "").await;
    assert_eq!(
        listed["keys"].as_array().expect("keys").len(),
        1,
        "a rejected request must not issue a key"
    );

    let (status, _) = admin_call(router.clone(), "POST", keys_uri, None, "").await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "no body issues an unlabelled key"
    );
    let (status, _) = admin_call(router.clone(), "POST", keys_uri, json, "").await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "an empty JSON body issues an unlabelled key"
    );
    let (status, _) = admin_call(router.clone(), "POST", keys_uri, json, r#"{"label":"ci"}"#).await;
    assert_eq!(status, StatusCode::CREATED);

    let (_, listed) = admin_call(router, "GET", keys_uri, None, "").await;
    let labels: Vec<_> = listed["keys"]
        .as_array()
        .expect("keys")
        .iter()
        .map(|k| k["label"].clone())
        .collect();
    assert_eq!(labels.len(), 4);
    assert!(
        labels.contains(&serde_json::json!("ci")),
        "labels: {labels:?}"
    );
}
