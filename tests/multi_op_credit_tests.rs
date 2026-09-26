//! Regression tests for multi-operation silent underpayment drop
//! (issues #614, #615, #616).
//!
//! ## The bug
//!
//! A Stellar transaction can contain multiple payment operations, all of which
//! share the **same** `transaction_hash`. Before the fix, the dedup key in
//! `processed_transactions` was `(payment_id, tx_hash)` — so when two
//! operations in the same transaction both matched a pending intent, the first
//! was inserted and the second triggered `ON CONFLICT DO NOTHING`, silently
//! discarding it. The intent was credited with only the first operation's
//! amount and left in `underpaid` (or completed short) with no error.
//!
//! ## The fix
//!
//! The primary key is extended to `(payment_id, tx_hash, operation_index)`.
//! Each operation within a transaction carries a distinct `operation_index`
//! (0, 1, 2, …), so they are now each credited independently.
//!
//! ## What these tests verify
//!
//! 1. `two_ops_same_tx_credits_full_sum` — the core regression: two
//!    `reconcile_payment` calls with the same `tx_hash` but different
//!    `operation_index` values both credit the intent and the intent
//!    reaches `completed` with the correct cumulative amount.
//!
//! 2. `same_op_reprocessed_is_idempotent` — idempotency is preserved: the
//!    same `(tx_hash, operation_index)` pair re-processed on a later poll
//!    cycle is a no-op (no double-credit).
//!
//! 3. `three_ops_same_tx_all_credited` — three operations in one transaction
//!    each contribute their share so the intent completes at the correct total.

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

// ── helpers ──────────────────────────────────────────────────────────────────

/// Build a minimal in-memory SQLite pool with migrations applied.
async fn memory_pool() -> db::Db {
    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(
            SqliteConnectOptions::from_str("sqlite::memory:")
                .unwrap()
                .create_if_missing(true),
        )
        .await
        .unwrap();
    db::migrate(&pool).await.unwrap();
    pool
}

/// Build an `AppState` wired to `pool` pointing webhooks at `webhook_url`.
fn make_state(pool: db::Db, webhook_url: Option<String>) -> Arc<AppState> {
    let _ = webhook_url; // used only to wire the seeded payment; state has no per-URL config
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

/// Create a pending payment intent for the given `amount` and return its id.
async fn seed_pending_payment(pool: &db::Db, amount: &str, webhook_url: Option<&str>) -> String {
    let id = uuid::Uuid::new_v4().to_string();
    db::create_payment(
        pool,
        NewPayment {
            id: &id,
            merchant_id: "test-merchant",
            destination_address: "GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5",
            memo: "MULTIOP01",
            amount,
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

/// Build a Horizon payment operation for the seeded intent, with a specific
/// `tx_hash`, `operation_index`, and `amount`.
fn make_op(tx_hash: &str, operation_index: i64, amount: &str) -> HorizonPayment {
    HorizonPayment {
        kind: "payment".into(),
        amount: Some(amount.into()),
        asset_type: Some("native".into()),
        asset_code: None,
        asset_issuer: None,
        to: Some("GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5".into()),
        transaction_hash: Some(tx_hash.into()),
        transaction: Some(TransactionRef {
            memo: Some("MULTIOP01".into()),
            memo_type: Some("text".into()),
            successful: Some(true),
        }),
        paging_token: Some(format!("{}{}", tx_hash, operation_index)),
        created_at: None,
        operation_index,
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

/// **Core regression test for issues #614 / #615.**
///
/// A transaction with two payment operations, each sending 5 XLM into the
/// same gateway address with the same memo, must credit both operations to
/// the matching intent so it reaches `completed` at 10 XLM — not
/// `underpaid` at 5 XLM (the old behaviour).
///
/// Before the fix the dedup key was `(payment_id, tx_hash)`: after the first
/// operation was recorded, the second triggered `ON CONFLICT DO NOTHING` and
/// was silently dropped. After the fix the key includes `operation_index`, so
/// `(payment_id, "TX_MULTI", 0)` and `(payment_id, "TX_MULTI", 1)` are
/// independent rows and both are credited.
#[tokio::test]
async fn two_ops_same_tx_credits_full_sum() {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/webhook"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&mock_server)
        .await;

    let webhook_url = format!("{}/webhook", mock_server.uri());
    let pool = memory_pool().await;
    let payment_id = seed_pending_payment(&pool, "10", Some(&webhook_url)).await;
    let state = make_state(pool.clone(), Some(webhook_url));

    // Two operations in the same transaction, each sending 5 XLM.
    let op0 = make_op("TX_MULTI_AB", 0, "5.0000000");
    let op1 = make_op("TX_MULTI_AB", 1, "5.0000000");

    // Process operation 0 — intent should become underpaid (5/10 XLM).
    let r0 = reconcile_payment(&state, &op0)
        .await
        .expect("op0 reconciliation must not error");
    assert!(
        r0,
        "op0 must trigger a settlement transition (pending → underpaid)"
    );

    let after_op0 = db::get_payment(&pool, &payment_id)
        .await
        .unwrap()
        .expect("payment must exist");
    assert_eq!(
        after_op0.status, "underpaid",
        "after first operation the intent must be underpaid, not completed"
    );
    assert_eq!(
        after_op0.paid_amount.as_deref(),
        Some("5"),
        "paid_amount after op0 must be 5 XLM"
    );

    // Process operation 1 — intent must complete (5+5=10 XLM).
    let r1 = reconcile_payment(&state, &op1)
        .await
        .expect("op1 reconciliation must not error");
    assert!(
        r1,
        "op1 must trigger a settlement transition (underpaid → completed)"
    );

    let after_op1 = db::get_payment(&pool, &payment_id)
        .await
        .unwrap()
        .expect("payment must exist");
    assert_eq!(
        after_op1.status, "completed",
        "after both operations the intent must be completed"
    );
    assert_eq!(
        after_op1.paid_amount.as_deref(),
        Some("10"),
        "paid_amount after both ops must be the full 10 XLM"
    );

    // Exactly two webhook deliveries must have been recorded (one per
    // settlement transition: underpaid + completed).
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    let deliveries = db::list_webhook_deliveries(&pool, &payment_id)
        .await
        .unwrap();
    assert_eq!(
        deliveries.len(),
        2,
        "expected exactly 2 webhook delivery rows (underpaid + completed), got {}",
        deliveries.len()
    );

    // Verify the recorded `processed_transactions` sum directly.
    let total = db::sum_processed_stroops(&pool, &payment_id).await.unwrap();
    assert_eq!(
        total,
        100_000_000, // 10 XLM in stroops
        "processed_transactions must sum to 100_000_000 stroops (10 XLM)"
    );
}

/// Re-presenting the same operation (same `tx_hash` **and** same
/// `operation_index`) on a later poll cycle must be a no-op — no
/// double-credit and no duplicate webhook.
#[tokio::test]
async fn same_op_reprocessed_is_idempotent() {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/webhook"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&mock_server)
        .await;

    let webhook_url = format!("{}/webhook", mock_server.uri());
    let pool = memory_pool().await;
    let payment_id = seed_pending_payment(&pool, "5", Some(&webhook_url)).await;
    let state = make_state(pool.clone(), Some(webhook_url));

    let op = make_op("TX_IDEM", 0, "5.0000000");

    // First reconciliation — should complete the intent.
    let first = reconcile_payment(&state, &op)
        .await
        .expect("first reconciliation must not error");
    assert!(first, "first reconciliation must settle the intent");

    // Second reconciliation of the exact same operation — must be a no-op.
    let second = reconcile_payment(&state, &op)
        .await
        .expect("second reconciliation must not error");
    assert!(!second, "re-presenting the same operation must be a no-op");

    let payment = db::get_payment(&pool, &payment_id).await.unwrap().unwrap();
    assert_eq!(payment.status, "completed");

    // Only one webhook delivery row despite two reconcile calls.
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    let deliveries = db::list_webhook_deliveries(&pool, &payment_id)
        .await
        .unwrap();
    assert_eq!(
        deliveries.len(),
        1,
        "idempotent re-reconcile must not produce a second webhook delivery row"
    );

    // processed_transactions sum must not be doubled.
    let total = db::sum_processed_stroops(&pool, &payment_id).await.unwrap();
    assert_eq!(total, 50_000_000, "5 XLM = 50_000_000 stroops, not doubled");
}

/// A transaction with **three** payment operations all matching the same
/// intent must credit all three and leave the intent correctly completed.
#[tokio::test]
async fn three_ops_same_tx_all_credited() {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/webhook"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&mock_server)
        .await;

    let webhook_url = format!("{}/webhook", mock_server.uri());
    let pool = memory_pool().await;
    let payment_id = seed_pending_payment(&pool, "3", Some(&webhook_url)).await;
    let state = make_state(pool.clone(), Some(webhook_url));

    // Three 1 XLM operations in a single transaction.
    let ops: Vec<HorizonPayment> = (0..3)
        .map(|i| make_op("TX_THREE", i, "1.0000000"))
        .collect();

    for (i, op) in ops.iter().enumerate() {
        reconcile_payment(&state, op)
            .await
            .unwrap_or_else(|e| panic!("op {i} reconciliation failed: {e}"));
    }

    let payment = db::get_payment(&pool, &payment_id).await.unwrap().unwrap();
    assert_eq!(
        payment.status, "completed",
        "intent must be completed after three 1 XLM operations totalling 3 XLM"
    );
    assert_eq!(
        payment.paid_amount.as_deref(),
        Some("3"),
        "paid_amount must be 3 XLM"
    );

    let total = db::sum_processed_stroops(&pool, &payment_id).await.unwrap();
    assert_eq!(
        total,
        30_000_000, // 3 XLM in stroops
        "processed_transactions must sum to 30_000_000 stroops (3 XLM)"
    );
}
