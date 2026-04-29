use axum::http::StatusCode;
use axum_test::TestServer;
use serde_json::{json, Value};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::str::FromStr;
use std::sync::Arc;
use stellargate::{api, config::Config, db, AppState};

async fn test_server() -> TestServer {
    let cfg = Config {
        port: 0,
        database_url: "sqlite::memory:".into(),
        horizon_url: String::new(),
        gateway_public: "GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5".into(),
        usdc_issuer: String::new(),
        webhook_secret: String::new(),
        webhook_retry_attempts: 1,
        webhook_retry_delay_ms: 0,
    };
    let pool = SqlitePoolOptions::new()
        .connect_with(SqliteConnectOptions::from_str(&cfg.database_url).unwrap().create_if_missing(true))
        .await
        .unwrap();
    db::migrate(&pool).await.unwrap();
    TestServer::new(api::router(Arc::new(AppState { pool, config: cfg }))).unwrap()
}

#[tokio::test]
async fn test_health() {
    let res = test_server().await.get("/health").await;
    res.assert_status_ok();
}

#[tokio::test]
async fn test_create_payment() {
    let res = test_server().await
        .post("/payments")
        .json(&json!({ "amount": "10", "asset": "XLM" }))
        .await;
    res.assert_status(StatusCode::CREATED);
    let body: Value = res.json();
    assert_eq!(body["status"], "pending");
    assert_eq!(body["asset"], "XLM");
    assert_eq!(body["memo"].as_str().unwrap().len(), 8);
}

#[tokio::test]
async fn test_create_invalid_asset() {
    let res = test_server().await
        .post("/payments")
        .json(&json!({ "amount": "10", "asset": "BTC" }))
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_create_invalid_amount() {
    let res = test_server().await
        .post("/payments")
        .json(&json!({ "amount": "-1", "asset": "XLM" }))
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_get_by_id() {
    let server = test_server().await;
    let id = server.post("/payments")
        .json(&json!({ "amount": "5", "asset": "USDC" }))
        .await
        .json::<Value>()["id"].as_str().unwrap().to_string();

    let res = server.get(&format!("/payments/{id}")).await;
    res.assert_status_ok();
    assert_eq!(res.json::<Value>()["id"], id);
}

#[tokio::test]
async fn test_get_not_found() {
    let res = test_server().await.get("/payments/does-not-exist").await;
    res.assert_status(StatusCode::NOT_FOUND);
}
