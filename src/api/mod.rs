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
        .route("/ready", axum::routing::get(ready))
        .route("/payments", post(payments::create).get(payments::list))
        .route("/payments/:id", get(payments::get_by_id))
        .fallback(not_found)
        .layer(TraceLayer::new_for_http())
        .layer(RequestBodyLimitLayer::new(MAX_BODY_BYTES))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

async fn health() -> impl IntoResponse {
    Json(json!({ "status": "ok" }))
}

use axum::extract::State;

async fn ready(
    State(state): State<Arc<AppState>>,
) -> (axum::http::StatusCode, &'static str) {
    match sqlx::query("SELECT 1")
        .fetch_one(&state.db)
        .await
    {
        Ok(_) => (axum::http::StatusCode::OK, "ready"),
        Err(_) => (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "db down",
        ),
    }
}

async fn not_found() -> impl IntoResponse {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "not found" })),
    )
}
