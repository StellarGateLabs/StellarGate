use crate::AppState;
use axum::{routing::{get, post}, Router};
use std::sync::Arc;

mod payments;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(|| async { "StellarGate API v0.1.0" }))
        .route("/health", get(|| async { "ok" }))
        .route("/payments", post(payments::create))
        .route("/payments/:id", get(payments::get_by_id))
        .with_state(state)
}
