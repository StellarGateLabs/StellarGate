use crate::AppState;
use axum::{
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json,
};
use serde_json::json;
use std::sync::Arc;
use tower_http::{
    cors::CorsLayer, limit::RequestBodyLimitLayer, trace::TraceLayer,
};

mod payments;

/// Reject request bodies larger than this (256 KiB) before they hit a handler.
const MAX_BODY_BYTES: usize = 256 * 1024;

pub fn router(state: Arc<AppState>) -> axum::Router {
    axum::Router::new()
        .route("/", get(|| async { "StellarGate API v0.1.0" }))
        .route("/health", get(health))
        .route("/payments", post(payments::create).get(payments::list))
        .route("/payments/:id", get(payments::get_by_id))
        .route("/payments/:id/webhooks", get(payments::list_webhooks))
        .route("/payments/:id/webhooks/:delivery_id/redeliver", post(payments::redeliver_webhook))
        .fallback(not_found)
        .layer(TraceLayer::new_for_http())
        .layer(RequestBodyLimitLayer::new(MAX_BODY_BYTES))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

async fn health() -> impl IntoResponse {
    Json(json!({ "status": "ok" }))
}

async fn not_found() -> impl IntoResponse {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "not found" })),
    )
}
