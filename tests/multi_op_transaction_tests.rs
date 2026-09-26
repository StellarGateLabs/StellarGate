//! Regression tests for multi-operation transaction under-credit (issue #613).
//!
//! ## The bug
//!
//! When a single Stellar transaction contains multiple payment operations that
//! all target the same intent (same destination address + memo), Horizon returns
//! each operation as a separate [`HorizonPayment`] record — all sharing the
//! same `transaction_hash`.
//!
//! Before the fix the `processed_transactions` dedup key was `(payment_id,
//! tx_hash)` only.  The first operation was recorded fine, but the second one
//! hit a PRIMARY KEY conflict and was silently discarded (`ON CONFLICT DO
//! NOTHING`).  The gateway credited only the first operation, leaving the
//! intent stuck `underpaid` even though the customer had fully paid on-chain.
//!
//! ## The fix (issue #615 / #616)
//!
//! The dedup key now includes `operation_index` (derived from the Horizon
//! paging token, which is unique per operation).  Each operation within a
//! multi-op transaction therefore gets its own row and is credited
//! independently.
//!
//! ## Tests
//!
//! 1. **multi_op_same_tx_credits_full_amount** — two 5 XLM operations sharing
//!    one transaction hash both credit an intent that expects 10 XLM; the
//!    intent reaches `completed` instead of staying `underpaid`.
//!
//! 2. **multi_op_idempotent_on_rescan** — re-processing the same two operations
//!    (as a poller rescan would do) is a no-op; the intent does not become
//!    over-credited.
//!
//! 3. **single_op_tx_still_works** — a normal, single-operation transaction
//!    still settles correctly (regression guard).

use std::sync::Arc;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::str::FromStr;
use stellargate::{
    AppState,
    config::{AcceptedAsset, Config, ListenerMode},
    db::{self, NewPayment},
    horizon::{HorizonPayment, TransactionRef, reconcile_payment},
};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

// ── helpers ───────────────────────────────────────────────────────────────────

/// Build a minimal in-memory SQLite pool with migrations applied.
async fn memory_pool() -> db::Db {
    let name = uuid::Uuid::new_v4().to_string();
    let url = format!("sqlite:file:{}?mode=memory&cache=shared", name);
    let pool = SqlitePoolOptions::new()
        .min_connections(1)
        .max_connections(5)
        .connect_with(
            SqliteConnectOptions::from_str(&url)
                .unwrap()
                .create_if_missing(true),
        )
        .await
        .unwrap();
    db::migrate(&pool).await.unwrap();
    pool
}

/// Build an [`AppState`] wired to `pool`, pointing webhooks at `webhook_url`.
fn make_state(pool: db::Db, _webhook_url: Option<String>) -> Arc<AppState> {
    let accepted_assets = vec![AcceptedAsset {
        code: "XLM".into(),
        issuer: None,
    }];

    Arc::new(AppState {
        pool,
        config: Config {
            port: 0,
            database_url: "sqlite::memory:".into(),
            network: "testnet".into(),
            horizon_url: String::new(),
            gateway_public: "GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5".into(),
            accepted_assets,
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
            poll_max_pages_per_cycle: 50,
            payment_ttl_secs: 3600,
            rate_limit_requests_per_sec: 10000,
            db_pool_max_connections: 5,
            db_busy_timeout_ms: 5000,
            cors_allowed_origins: vec![],
            listener_mode: ListenerMode::Stream,
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

/// Create a 10 XLM pending payment intent and return its id.
async fn seed_pending_payment(pool: &db::Db, webhook_url: Option<&str>) -> String {
    let id = uuid::Uuid::new_v4().to_string();
    db::create_payment(
        pool,
        NewPayment {
            id: &id,
            merchant_id: "test-merchant",
            destination_address: "GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5",
            memo: "MULTIOP01",
            amount: "10",
            asset: "XLM",
            asset_issuer: None,
            webhook_url,
            ttl_secs: 3600,
        },
    )
    .await
    .unwrap();
    id
}

/// Build a 5 XLM Horizon payment operation for the seeded intent.
///
/// `paging_token` must be unique per operation — Horizon encodes the ledger,
/// transaction position, and operation index into this value.  Passing
/// different tokens for op 0 and op 1 of the same transaction simulates what
/// Horizon actually returns for a multi-operation transaction.
fn make_half_payment(paging_token: &str) -> HorizonPayment {
    HorizonPayment {
        kind: "payment".into(),
        amount: Some("5.0000000".into()),
        asset_type: Some("native".into()),
        asset_code: None,
        asset_issuer: None,
        to: Some("GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5".into()),
        transaction_hash: Some("MULTIOP_TX_HASH_SHARED_0001".into()),
        transaction: Some(TransactionRef {
            memo: Some("MULTIOP01".into()),
            memo_type: Some("text".into()),
            successful: Some(true),
        }),
        paging_token: Some(paging_token.into()),
        created_at: None,
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

/// **Core regression test for issue #613.**
///
/// A single Stellar transaction contains two 5 XLM payment operations that
/// both target the same pending intent (10 XLM expected).  Both operations
/// share the same `transaction_hash` but have different paging tokens (and
/// therefore different `operation_index` values).
///
/// Expected outcome: both operations are credited, the intent reaches
/// `completed`, and exactly one `payment.completed` webhook is dispatched.
///
/// Before the fix, the second operation would silently hit the `(payment_id,
/// tx_hash)` PRIMARY KEY conflict, be discarded, and the intent would remain
/// `underpaid` — the bug described in issue #613.
#[tokio::test]
async fn multi_op_same_tx_credits_full_amount() {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/webhook"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&mock_server)
        .await;

    let webhook_url = format!("{}/webhook", mock_server.uri());
    let pool = memory_pool().await;
    let payment_id = seed_pending_payment(&pool, Some(&webhook_url)).await;
    let state = make_state(pool.clone(), Some(webhook_url));

    // Op 0: first 5 XLM operation — different paging token → operation_index 100
    let op0 = make_half_payment("100");
    // Op 1: second 5 XLM operation in the same tx — paging token → operation_index 101
    let op1 = make_half_payment("101");

    // Process op 0 — intent goes underpaid (5 of 10 XLM received).
    let settled0 = reconcile_payment(&state, &op0)
        .await
        .expect("op0 reconcile must not error");
    assert!(!settled0, "first half-payment must not complete the intent");

    let payment = db::get_payment(&pool, &payment_id)
        .await
        .unwrap()
        .expect("payment must exist");
    assert_eq!(
        payment.status, "underpaid",
        "after first op intent should be underpaid"
    );

    // Process op 1 — cumulative total reaches 10 XLM → completed.
    let settled1 = reconcile_payment(&state, &op1)
        .await
        .expect("op1 reconcile must not error");
    assert!(
        settled1,
        "second half-payment must complete the intent (multi-op fix)"
    );

    let payment = db::get_payment(&pool, &payment_id)
        .await
        .unwrap()
        .expect("payment must exist after second op");
    assert_eq!(
        payment.status, "completed",
        "intent must be completed after both ops are credited"
    );

    // Exactly one completed webhook should have been dispatched.
    tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;
    let received = mock_server.received_requests().await.unwrap();
    assert_eq!(
        received.len(),
        1,
        "exactly one webhook must be dispatched (payment.completed); got {}",
        received.len()
    );

    // Both operations must be in the processed_transactions ledger.
    let total = db::sum_processed_stroops(&pool, &payment_id).await.unwrap();
    assert_eq!(
        total,
        100_000_000, // 10 XLM in stroops
        "processed_transactions total must be 10 XLM (100_000_000 stroops); got {total}"
    );
}

/// Re-processing the same two operations (as a poller rescan would) must be a
/// complete no-op: the intent stays `completed` and no extra webhooks fire.
#[tokio::test]
async fn multi_op_idempotent_on_rescan() {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/webhook"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&mock_server)
        .await;

    let webhook_url = format!("{}/webhook", mock_server.uri());
    let pool = memory_pool().await;
    let payment_id = seed_pending_payment(&pool, Some(&webhook_url)).await;
    let state = make_state(pool.clone(), Some(webhook_url));

    let op0 = make_half_payment("200");
    let op1 = make_half_payment("201");

    // First pass: settle the intent.
    reconcile_payment(&state, &op0).await.unwrap();
    reconcile_payment(&state, &op1).await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Verify completed.
    let payment = db::get_payment(&pool, &payment_id).await.unwrap().unwrap();
    assert_eq!(payment.status, "completed");

    let after_first_pass = mock_server.received_requests().await.unwrap().len();
    assert_eq!(
        after_first_pass, 1,
        "should have exactly one webhook after first pass"
    );

    // Second pass: rescan with the same operations.
    reconcile_payment(&state, &op0).await.unwrap();
    reconcile_payment(&state, &op1).await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Status must not change.
    let payment = db::get_payment(&pool, &payment_id).await.unwrap().unwrap();
    assert_eq!(
        payment.status, "completed",
        "rescan must not change a completed intent"
    );

    // No new webhooks.
    let after_rescan = mock_server.received_requests().await.unwrap().len();
    assert_eq!(
        after_rescan, after_first_pass,
        "rescan must not fire additional webhooks; got {} total after rescan",
        after_rescan
    );

    // Stroop total must remain exactly 10 XLM.
    let total = db::sum_processed_stroops(&pool, &payment_id).await.unwrap();
    assert_eq!(
        total, 100_000_000,
        "rescan must not double-count stroops; got {total}"
    );
}

/// A normal single-operation transaction still settles correctly after the fix.
#[tokio::test]
async fn single_op_tx_still_works() {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/webhook"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&mock_server)
        .await;

    let webhook_url = format!("{}/webhook", mock_server.uri());
    let pool = memory_pool().await;

    // Seed a 10 XLM intent.
    let id = uuid::Uuid::new_v4().to_string();
    db::create_payment(
        &pool,
        NewPayment {
            id: &id,
            merchant_id: "test-merchant",
            destination_address: "GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5",
            memo: "SINGLEOP1",
            amount: "10",
            asset: "XLM",
            asset_issuer: None,
            webhook_url: Some(&webhook_url),
            ttl_secs: 3600,
        },
    )
    .await
    .unwrap();

    let state = make_state(pool.clone(), Some(webhook_url));

    let hp = HorizonPayment {
        kind: "payment".into(),
        amount: Some("10.0000000".into()),
        asset_type: Some("native".into()),
        asset_code: None,
        asset_issuer: None,
        to: Some("GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5".into()),
        transaction_hash: Some("SINGLEOP_TX_HASH_001".into()),
        transaction: Some(TransactionRef {
            memo: Some("SINGLEOP1".into()),
            memo_type: Some("text".into()),
            successful: Some(true),
        }),
        paging_token: Some("300".into()),
        created_at: None,
    };

    let settled = reconcile_payment(&state, &hp)
        .await
        .expect("single-op reconcile must not error");
    assert!(settled, "single-op payment must settle the intent");

    let payment = db::get_payment(&pool, &id).await.unwrap().unwrap();
    assert_eq!(payment.status, "completed");

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    let received = mock_server.received_requests().await.unwrap();
    assert_eq!(
        received.len(),
        1,
        "exactly one webhook must fire for a single-op tx"
    );
}
