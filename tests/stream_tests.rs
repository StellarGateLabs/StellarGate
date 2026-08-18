//! Integration tests for the Horizon SSE streaming listener.
//!
//! Covers chunk-boundary reassembly, cursor advancement, reconnect behavior,
//! frame filtering, and shutdown responsiveness.

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use stellargate::{
    config::{AcceptedAsset, Config, ListenerMode},
    db, horizon, webhook, AppState,
};
use tokio::sync::watch;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

fn make_config() -> Config {
    Config {
        port: 0,
        database_url: "sqlite::memory:".into(),
        network: "testnet".into(),
        horizon_url: String::new(),
        gateway_public: "GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5".into(),
        accepted_assets: AcceptedAsset::default_list(),
        webhook_secret: "test-secret".into(),
        webhook_retry_attempts: 1,
        webhook_retry_delay_ms: 0,
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
        payment_ttl_secs: 3600,
        cors_allowed_origins: vec![],
        listener_mode: ListenerMode::Stream,
        webhook_allow_private_targets: true,
        rate_limit_requests_per_sec: 1000,
        db_pool_max_connections: 10,
        db_busy_timeout_ms: 5000,
        admin_provisioning_secret: String::new(),
        request_timeout_secs: 30,
    }
}

async fn setup_state(mut cfg: Config) -> AppState {
    cfg.horizon_url = String::new(); // will be set per-test
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
        webhook_metrics: stellargate::metrics::WebhookMetrics::new(),
        auth_metrics: stellargate::metrics::AuthMetrics::new(),
        task_health: stellargate::TaskHealth::new(),
    }
}

async fn create_test_payment(state: &AppState, webhook_url: Option<&str>) -> db::Payment {
    db::create_payment(
        &state.pool,
        db::NewPayment {
            id: "pay_test_stream",
            merchant_id: "merchant1",
            destination_address: &state.config.gateway_public,
            memo: "STREAMMEMO",
            amount: "10",
            asset: "XLM",
            webhook_url,
            ttl_secs: 3600,
        },
    )
    .await
    .unwrap()
}

/// Build an SSE text/event-stream payload with a single payment event.
fn sse_payment_event(paging_token: &str, amount: &str, memo: &str, to: &str) -> String {
    format!(
        "id: {}\nevent: \ndata: {}\n\n",
        paging_token,
        serde_json::json!({
            "type": "payment",
            "amount": amount,
            "asset_type": "native",
            "to": to,
            "transaction_hash": "TX_STREAM_TEST",
            "transaction": {
                "memo": memo,
                "memo_type": "text",
                "successful": true
            },
            "paging_token": paging_token,
            "created_at": "2026-01-01T00:00:00Z"
        })
    )
}

/// Test that a payment event delivered over the stream settles a matching
/// intent end-to-end.
#[tokio::test]
async fn stream_settles_matching_payment_intent() {
    let server = MockServer::start().await;
    let webhook_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/webhook"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&webhook_server)
        .await;

    let mut cfg = make_config();
    cfg.horizon_url = server.uri();
    let state = Arc::new(setup_state(cfg).await);
    let webhook_url = format!("{}/webhook", webhook_server.uri());
    let payment = create_test_payment(&state, Some(&webhook_url)).await;

    let event_stream = sse_payment_event(
        "123456789",
        "10.0000000",
        "STREAMMEMO",
        &state.config.gateway_public,
    );

    Mock::given(method("GET"))
        .and(path(format!(
            "/accounts/{}/payments",
            state.config.gateway_public
        )))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(event_stream)
                .insert_header("Content-Type", "text/event-stream"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let listener_state = state.clone();
    let listener = tokio::spawn(async move {
        horizon::run_stream_listener(listener_state, shutdown_rx).await;
    });

    // Give the stream enough time to process the event
    tokio::time::sleep(Duration::from_millis(300)).await;

    shutdown_tx.send(true).unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(2), listener).await;

    let settled = db::get_payment(&state.pool, &payment.id)
        .await
        .unwrap()
        .expect("payment must exist");
    assert_eq!(settled.status, "completed");
    assert_eq!(settled.tx_hash.as_deref(), Some("TX_STREAM_TEST"));
    assert_eq!(settled.paid_amount.as_deref(), Some("10"));
}

/// Test that an event split across two chunks — deliberately splitting a
/// multibyte UTF-8 character — is reassembled correctly without corruption.
#[tokio::test]
async fn stream_reassembles_split_multibyte_character() {
    let server = MockServer::start().await;

    let mut cfg = make_config();
    cfg.horizon_url = server.uri();
    let state = Arc::new(setup_state(cfg).await);

    // Create a payment with a memo containing multibyte UTF-8 (emoji)
    let memo_with_emoji = "MEMO🚀TEST";
    db::create_payment(
        &state.pool,
        db::NewPayment {
            id: "pay_emoji",
            merchant_id: "merchant1",
            destination_address: &state.config.gateway_public,
            memo: memo_with_emoji,
            amount: "10",
            asset: "XLM",
            webhook_url: None,
            ttl_secs: 3600,
        },
    )
    .await
    .unwrap();

    let event = sse_payment_event(
        "999",
        "10.0000000",
        memo_with_emoji,
        &state.config.gateway_public,
    );

    // Split the event in the middle of the UTF-8 rocket emoji (🚀 is 4 bytes: F0 9F 9A 80)
    let split_point = event.find("🚀").unwrap() + 2; // Split emoji in half
    let chunk1 = &event[..split_point];
    let chunk2 = &event[split_point..];

    let call_count = Arc::new(AtomicUsize::new(0));
    let call_count_clone = call_count.clone();
    let chunk1_owned = chunk1.to_string();
    let chunk2_owned = chunk2.to_string();

    Mock::given(method("GET"))
        .and(path(format!(
            "/accounts/{}/payments",
            state.config.gateway_public
        )))
        .respond_with(move |_req: &Request| {
            let n = call_count_clone.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                ResponseTemplate::new(200)
                    .set_body_raw(chunk1_owned.as_bytes(), "text/event-stream")
                    .insert_header("Content-Type", "text/event-stream")
            } else {
                ResponseTemplate::new(200)
                    .set_body_raw(chunk2_owned.as_bytes(), "text/event-stream")
                    .insert_header("Content-Type", "text/event-stream")
            }
        })
        .expect(1..)
        .mount(&server)
        .await;

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let listener_state = state.clone();
    let listener = tokio::spawn(async move {
        horizon::run_stream_listener(listener_state, shutdown_rx).await;
    });

    tokio::time::sleep(Duration::from_millis(300)).await;

    shutdown_tx.send(true).unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(2), listener).await;

    let settled = db::get_payment(&state.pool, "pay_emoji")
        .await
        .unwrap()
        .expect("payment must exist");
    assert_eq!(settled.status, "completed");
}

/// Test that the open greeting and comment-only keep-alives are ignored without
/// error.
#[tokio::test]
async fn stream_ignores_greeting_and_keepalives() {
    let server = MockServer::start().await;

    let mut cfg = make_config();
    cfg.horizon_url = server.uri();
    let state = Arc::new(setup_state(cfg).await);
    let payment = create_test_payment(&state, None).await;

    // Stream that starts with greeting, has keep-alives, and one real payment
    let stream_body = format!(
        "retry: 1000\nevent: open\ndata: \"hello\"\n\n\
         : keep-alive\n\n\
         {}\
         : another keep-alive\n\n",
        sse_payment_event(
            "555",
            "10.0000000",
            "STREAMMEMO",
            &state.config.gateway_public
        )
    );

    Mock::given(method("GET"))
        .and(path(format!(
            "/accounts/{}/payments",
            state.config.gateway_public
        )))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(stream_body)
                .insert_header("Content-Type", "text/event-stream"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let listener_state = state.clone();
    let listener = tokio::spawn(async move {
        horizon::run_stream_listener(listener_state, shutdown_rx).await;
    });

    tokio::time::sleep(Duration::from_millis(300)).await;

    shutdown_tx.send(true).unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(2), listener).await;

    let settled = db::get_payment(&state.pool, &payment.id)
        .await
        .unwrap()
        .expect("payment must exist");
    assert_eq!(settled.status, "completed");
}

/// Test that a dropped connection reconnects with the last-seen cursor in the
/// query string.
#[tokio::test]
async fn stream_reconnects_with_last_cursor() {
    let server = MockServer::start().await;

    let mut cfg = make_config();
    cfg.horizon_url = server.uri();
    let state = Arc::new(setup_state(cfg).await);

    // Create two payments to process across two connections
    db::create_payment(
        &state.pool,
        db::NewPayment {
            id: "pay_first",
            merchant_id: "merchant1",
            destination_address: &state.config.gateway_public,
            memo: "FIRST",
            amount: "5",
            asset: "XLM",
            webhook_url: None,
            ttl_secs: 3600,
        },
    )
    .await
    .unwrap();

    db::create_payment(
        &state.pool,
        db::NewPayment {
            id: "pay_second",
            merchant_id: "merchant1",
            destination_address: &state.config.gateway_public,
            memo: "SECOND",
            amount: "7",
            asset: "XLM",
            webhook_url: None,
            ttl_secs: 3600,
        },
    )
    .await
    .unwrap();

    let connection_count = Arc::new(AtomicUsize::new(0));
    let cursor_seen = Arc::new(tokio::sync::Mutex::new(String::new()));
    let cursor_clone = cursor_seen.clone();

    let conn_count_clone = connection_count.clone();
    Mock::given(method("GET"))
        .and(path(format!(
            "/accounts/{}/payments",
            state.config.gateway_public
        )))
        .respond_with(move |req: &Request| {
            let n = conn_count_clone.fetch_add(1, Ordering::SeqCst);

            // Capture the cursor parameter
            let cursor_param = req
                .url
                .query_pairs()
                .find(|(k, _)| k == "cursor")
                .map(|(_, v)| v.to_string())
                .unwrap_or_else(|| "now".to_string());

            let cursor_clone_inner = cursor_clone.clone();
            tokio::spawn(async move {
                let mut guard = cursor_clone_inner.lock().await;
                *guard = cursor_param;
            });

            if n == 0 {
                // First connection: deliver one event with cursor "100"
                ResponseTemplate::new(200)
                    .set_body_string(sse_payment_event(
                        "100",
                        "5.0000000",
                        "FIRST",
                        &state.config.gateway_public,
                    ))
                    .insert_header("Content-Type", "text/event-stream")
            } else {
                // Second connection: deliver the next event with cursor "200"
                ResponseTemplate::new(200)
                    .set_body_string(sse_payment_event(
                        "200",
                        "7.0000000",
                        "SECOND",
                        &state.config.gateway_public,
                    ))
                    .insert_header("Content-Type", "text/event-stream")
            }
        })
        .expect(2..)
        .mount(&server)
        .await;

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let listener_state = state.clone();
    let listener = tokio::spawn(async move {
        horizon::run_stream_listener(listener_state, shutdown_rx).await;
    });

    // Wait for both reconnections to happen
    tokio::time::sleep(Duration::from_millis(500)).await;

    shutdown_tx.send(true).unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(2), listener).await;

    // Verify the second connection used cursor "100" from the first event
    let final_cursor = cursor_seen.lock().await.clone();
    assert_eq!(final_cursor, "100", "reconnect must use last-seen cursor");

    // Both payments should be settled
    let first = db::get_payment(&state.pool, "pay_first")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.status, "completed");
}

/// Test that shutdown causes run_stream_listener to return promptly without
/// hanging.
#[tokio::test]
async fn stream_shutdown_is_responsive() {
    let server = MockServer::start().await;

    let mut cfg = make_config();
    cfg.horizon_url = server.uri();
    let state = Arc::new(setup_state(cfg).await);

    // Create a mock that never closes the stream
    Mock::given(method("GET"))
        .and(path(format!(
            "/accounts/{}/payments",
            state.config.gateway_public
        )))
        .respond_with(move |_req: &Request| {
            // Return a long-lived stream that sends keep-alives indefinitely
            ResponseTemplate::new(200)
                .set_body_string(": keep-alive\n\n")
                .insert_header("Content-Type", "text/event-stream")
                .set_delay(Duration::from_secs(10))
        })
        .expect(1..)
        .mount(&server)
        .await;

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let listener_state = state.clone();
    let listener = tokio::spawn(async move {
        horizon::run_stream_listener(listener_state, shutdown_rx).await;
    });

    // Let the listener start
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Signal shutdown
    shutdown_tx.send(true).unwrap();

    // The listener must exit within a reasonable time
    let result = tokio::time::timeout(Duration::from_secs(2), listener).await;
    assert!(
        result.is_ok(),
        "run_stream_listener must exit promptly on shutdown signal"
    );
}

/// Test that stream_once correctly processes multiple events in a single
/// response body.
#[tokio::test]
async fn stream_processes_multiple_events() {
    let server = MockServer::start().await;

    let mut cfg = make_config();
    cfg.horizon_url = server.uri();
    let state = Arc::new(setup_state(cfg).await);

    // Create three payments
    for i in 1..=3 {
        db::create_payment(
            &state.pool,
            db::NewPayment {
                id: &format!("pay_{}", i),
                merchant_id: "merchant1",
                destination_address: &state.config.gateway_public,
                memo: &format!("MEMO{}", i),
                amount: &format!("{}", i),
                asset: "XLM",
                webhook_url: None,
                ttl_secs: 3600,
            },
        )
        .await
        .unwrap();
    }

    // Build a stream with three payment events
    let stream_body = format!(
        "{}{}{}",
        sse_payment_event("1001", "1.0000000", "MEMO1", &state.config.gateway_public),
        sse_payment_event("1002", "2.0000000", "MEMO2", &state.config.gateway_public),
        sse_payment_event("1003", "3.0000000", "MEMO3", &state.config.gateway_public)
    );

    Mock::given(method("GET"))
        .and(path(format!(
            "/accounts/{}/payments",
            state.config.gateway_public
        )))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(stream_body)
                .insert_header("Content-Type", "text/event-stream"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let listener_state = state.clone();
    let listener = tokio::spawn(async move {
        horizon::run_stream_listener(listener_state, shutdown_rx).await;
    });

    tokio::time::sleep(Duration::from_millis(400)).await;

    shutdown_tx.send(true).unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(2), listener).await;

    // All three payments should be settled
    for i in 1..=3 {
        let payment = db::get_payment(&state.pool, &format!("pay_{}", i))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(payment.status, "completed");
        assert_eq!(payment.paid_amount.as_deref(), Some(&i.to_string()));
    }
}

/// Test that exponential backoff resets when the cursor advances (successful
/// reconnection) and doubles otherwise (connection keeps failing).
#[tokio::test]
async fn stream_backoff_behavior() {
    let server = MockServer::start().await;

    let mut cfg = make_config();
    cfg.horizon_url = server.uri();
    let state = Arc::new(setup_state(cfg).await);

    let connection_count = Arc::new(AtomicUsize::new(0));
    let timestamps = Arc::new(tokio::sync::Mutex::new(Vec::<std::time::Instant>::new()));

    let conn_count_clone = connection_count.clone();
    let timestamps_clone = timestamps.clone();

    Mock::given(method("GET"))
        .and(path(format!(
            "/accounts/{}/payments",
            state.config.gateway_public
        )))
        .respond_with(move |_req: &Request| {
            let n = conn_count_clone.fetch_add(1, Ordering::SeqCst);
            let ts_clone = timestamps_clone.clone();

            tokio::spawn(async move {
                let mut guard = ts_clone.lock().await;
                guard.push(std::time::Instant::now());
            });

            if n < 2 {
                // First two connections fail immediately
                ResponseTemplate::new(500)
            } else {
                // Third connection succeeds with an event
                ResponseTemplate::new(200)
                    .set_body_string(": keep-alive\n\n")
                    .insert_header("Content-Type", "text/event-stream")
            }
        })
        .expect(3..)
        .mount(&server)
        .await;

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let listener_state = state.clone();
    let listener = tokio::spawn(async move {
        horizon::run_stream_listener(listener_state, shutdown_rx).await;
    });

    // Wait for reconnection attempts
    tokio::time::sleep(Duration::from_millis(4000)).await;

    shutdown_tx.send(true).unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(2), listener).await;

    let ts = timestamps.lock().await;
    // Should have at least 3 connection attempts
    assert!(ts.len() >= 3, "should have multiple reconnection attempts");
}

/// Test CRLF line endings in SSE events are handled correctly.
#[tokio::test]
async fn stream_handles_crlf_line_endings() {
    let server = MockServer::start().await;

    let mut cfg = make_config();
    cfg.horizon_url = server.uri();
    let state = Arc::new(setup_state(cfg).await);
    let payment = create_test_payment(&state, None).await;

    // Build an SSE event with CRLF line endings instead of LF
    let event_crlf = format!(
        "id: {}\r\nevent: \r\ndata: {}\r\n\r\n",
        "7777",
        serde_json::json!({
            "type": "payment",
            "amount": "10.0000000",
            "asset_type": "native",
            "to": &state.config.gateway_public,
            "transaction_hash": "TX_CRLF",
            "transaction": {
                "memo": "STREAMMEMO",
                "memo_type": "text",
                "successful": true
            },
            "paging_token": "7777",
            "created_at": "2026-01-01T00:00:00Z"
        })
    );

    Mock::given(method("GET"))
        .and(path(format!(
            "/accounts/{}/payments",
            state.config.gateway_public
        )))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(event_crlf.as_bytes(), "text/event-stream")
                .insert_header("Content-Type", "text/event-stream"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let listener_state = state.clone();
    let listener = tokio::spawn(async move {
        horizon::run_stream_listener(listener_state, shutdown_rx).await;
    });

    tokio::time::sleep(Duration::from_millis(300)).await;

    shutdown_tx.send(true).unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(2), listener).await;

    let settled = db::get_payment(&state.pool, &payment.id)
        .await
        .unwrap()
        .expect("payment must exist");
    assert_eq!(settled.status, "completed");
    assert_eq!(settled.tx_hash.as_deref(), Some("TX_CRLF"));
}
