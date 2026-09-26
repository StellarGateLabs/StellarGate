//! Dashboard asset checks.
//!
//! The static checks below inspect the compiled-in JS directly; the router
//! checks serve each asset through the real `api::router` so a response-builder
//! or header API change (e.g. the axum 0.7 → 0.8 upgrade, #637) that drops a
//! content type or the Content-Security-Policy fails CI.

use axum_test::TestServer;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::str::FromStr;
use std::sync::Arc;
use stellargate::{
    AppState, api,
    config::{Config, ListenerMode},
    db,
};

fn make_config() -> Config {
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
        rate_limit_requests_per_sec: 1000,
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

const DASHBOARD_HTML: &str = include_str!("../static/dashboard.html");
const DASHBOARD_CSS: &str = include_str!("../static/dashboard.css");
const DASHBOARD_JS: &str = include_str!("../static/dashboard.js");

#[test]
fn dashboard_api_requests_use_canonical_v1_base() {
    assert!(
        DASHBOARD_JS.contains(r#"var API_BASE = "/v1";"#),
        "the dashboard must define the canonical API version once"
    );
    assert!(
        DASHBOARD_JS.contains("return fetch(API_BASE + path,"),
        "the authenticated API helper must prefix every request with API_BASE"
    );

    let direct_fetches: Vec<_> = DASHBOARD_JS
        .lines()
        .filter(|line| line.contains("fetch("))
        .map(str::trim)
        .collect();
    assert_eq!(
        direct_fetches,
        [
            "return fetch(API_BASE + path, { method: opts.method || \"GET\", headers: headers }).then(",
            "fetch(\"/\")",
            "fetch(\"/ready\", { headers: { Accept: \"application/json\" } })",
        ],
        "new dashboard fetches must use the versioned API helper unless they target an explicitly unversioned operational endpoint"
    );

    assert_eq!(
        DASHBOARD_JS.matches("/v1").count(),
        1,
        "API_BASE must be the only /v1 literal so requests cannot become /v1/v1/..."
    );
}

const TEST_ADMIN_SECRET: &str = "test-admin-secret";

async fn test_server() -> TestServer {
    let cfg = make_config();
    let pool = SqlitePoolOptions::new()
        .connect_with(
            SqliteConnectOptions::from_str(&cfg.database_url)
                .unwrap()
                .create_if_missing(true),
        )
        .await
        .unwrap();
    db::migrate(&pool).await.unwrap();
    let router = api::router(Arc::new(AppState {
        pool,
        config: cfg,
        http: reqwest::Client::new(),
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
    TestServer::new(router)
}

/// Mirrors `DASHBOARD_CSP` in `src/api/mod.rs`. Compared verbatim so any
/// change to the policy — or the header being dropped by a framework upgrade —
/// is a deliberate, reviewed edit rather than a silent regression.
const EXPECTED_CSP: &str = "default-src 'none'; \
     script-src 'self'; \
     style-src 'self'; \
     img-src 'self' data:; \
     connect-src 'self'; \
     form-action 'none'; \
     frame-ancestors 'none'; \
     base-uri 'none'";

/// Every dashboard asset must be served with its exact content type, the full
/// CSP, and the baseline security headers the outer layers add (#637).
#[tokio::test]
async fn dashboard_assets_keep_content_type_and_csp() {
    let server = test_server().await;

    for (path, content_type, body) in [
        ("/dashboard", "text/html; charset=utf-8", DASHBOARD_HTML),
        (
            "/dashboard/app.css",
            "text/css; charset=utf-8",
            DASHBOARD_CSS,
        ),
        (
            "/dashboard/app.js",
            "text/javascript; charset=utf-8",
            DASHBOARD_JS,
        ),
    ] {
        let res = server.get(path).await;
        res.assert_status_ok();
        assert_eq!(
            res.header("content-type"),
            content_type,
            "{path} served with the wrong content type"
        );
        assert_eq!(
            res.header("content-security-policy"),
            EXPECTED_CSP,
            "{path} must carry the dashboard CSP unchanged"
        );
        assert_eq!(res.header("x-content-type-options"), "nosniff", "{path}");
        assert_eq!(res.header("referrer-policy"), "no-referrer", "{path}");
        assert_eq!(res.header("cache-control"), "no-store", "{path}");
        assert_eq!(
            res.text(),
            body,
            "{path} must serve the include_str! asset byte-for-byte"
        );
    }
}

/// Each header must appear exactly once — a response builder that appends
/// instead of replacing would otherwise emit a duplicate `nosniff` alongside
/// the outer security-header layer's copy.
#[tokio::test]
async fn dashboard_security_headers_are_not_duplicated() {
    let server = test_server().await;

    for path in ["/dashboard", "/dashboard/app.css", "/dashboard/app.js"] {
        let res = server.get(path).await;
        res.assert_status_ok();
        for name in [
            "content-type",
            "content-security-policy",
            "x-content-type-options",
        ] {
            assert_eq!(
                res.headers().get_all(name).iter().count(),
                1,
                "{path} must send exactly one {name} header"
            );
        }
    }
}
