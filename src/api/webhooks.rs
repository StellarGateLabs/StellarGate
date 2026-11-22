//! `POST /v1/webhooks/test`: sends a signed `webhook.test` event to a merchant
//! URL and reports what the receiver answered (#1048).
//!
//! Uses the same SSRF guard, signing and pinned client as real deliveries. It
//! writes nothing to `webhook_deliveries` and records no delivery metrics, so
//! test traffic cannot skew real delivery counts.

use std::sync::Arc;
use std::time::Instant;

use axum::{Json, extract::State};
use serde::Deserialize;
use serde_json::{Value, json};

use super::payments::AppError;
use crate::{AppState, webhook};

#[derive(Deserialize)]
pub struct TestWebhookRequest {
    url: String,
}

/// Sends one `webhook.test` event to `url`.
///
/// A rejected URL is a `400`. Once the request is sent, the receiver's outcome
/// is returned in a `200` body even when it failed, so the merchant can see the
/// status code and latency. Error text from the transport is reduced to a fixed
/// code, so the response does not reveal internal addresses.
pub async fn send_test(
    State(state): State<Arc<AppState>>,
    Json(req): Json<TestWebhookRequest>,
) -> Result<Json<Value>, AppError> {
    let client = webhook::safe_client(&state, &req.url)
        .await
        .map_err(|_| AppError::bad_request("invalid_webhook_url", "webhook URL is not allowed"))?;

    let payload = json!({
        "event": "webhook.test",
        "sentAt": webhook::current_timestamp(),
    });
    let body = serde_json::to_vec(&payload).map_err(anyhow::Error::from)?;
    let timestamp = webhook::current_timestamp();
    let signature = webhook::sign(&state.config.webhook_secret, timestamp, &body);

    let started = Instant::now();
    let result = client
        .post(&req.url)
        .header("Content-Type", "application/json")
        .header("X-StellarGate-Signature", &signature)
        .header("X-StellarGate-Timestamp", timestamp.to_string())
        .header("X-StellarGate-Event", "webhook.test")
        .body(body)
        .send()
        .await;
    let latency_ms = started.elapsed().as_millis() as u64;

    let outcome = match result {
        Ok(resp) => json!({
            "delivered": resp.status().is_success(),
            "status": resp.status().as_u16(),
            "latencyMs": latency_ms,
            "error": null,
        }),
        Err(err) => json!({
            "delivered": false,
            "status": null,
            "latencyMs": latency_ms,
            "error": if err.is_timeout() { "timeout" } else { "request_failed" },
        }),
    };
    Ok(Json(outcome))
}
