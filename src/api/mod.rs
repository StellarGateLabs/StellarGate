use crate::AppState;
use axum::{
    extract::ConnectInfo,
    http::StatusCode,
    middleware::{self, Next},
    response::IntoResponse,
    routing::{get, post},
    Json, Request,
};
use serde_json::json;
use std::net::SocketAddr;
use std::sync::Arc;
use tower_http::{
    cors::CorsLayer,
    limit::RequestBodyLimitLayer,
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
    trace::TraceLayer,
};

mod payments;

/// Reject request bodies larger than this (256 KiB) before they hit a handler.
const MAX_BODY_BYTES: usize = 256 * 1024;

static OPENAPI_SPEC: &str = include_str!("../../openapi.yaml");

async fn openapi() -> impl IntoResponse {
    // Parse once to validate and re-serialise as JSON; the YAML source is the
    // canonical spec so a single include_str keeps them in sync automatically.
    match serde_yaml::from_str::<serde_json::Value>(OPENAPI_SPEC) {
        Ok(v) => (StatusCode::OK, [(axum::http::header::CONTENT_TYPE, "application/json")], Json(v)).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("failed to parse openapi spec: {e}") })),
        ).into_response(),
    }
}

async fn swagger_ui() -> impl IntoResponse {
    let html = r#"<!DOCTYPE html>
<html>
<head>
  <title>StellarGate API</title>
  <meta charset="utf-8">
  <link rel="stylesheet" href="https://unpkg.com/swagger-ui-dist@5/swagger-ui.css">
</head>
<body>
  <div id="swagger-ui"></div>
  <script src="https://unpkg.com/swagger-ui-dist@5/swagger-ui-bundle.js"></script>
  <script>
    SwaggerUIBundle({ url: "/openapi.json", dom_id: '#swagger-ui', presets: [SwaggerUIBundle.presets.apis, SwaggerUIBundle.SwaggerUIStandalonePreset] });
  </script>
</body>
</html>"#;
    (StatusCode::OK, [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")], html)
}

pub fn router(state: Arc<AppState>) -> axum::Router {
    let cors = build_cors(&state.config);
    let rate_limit_rps = state.config.rate_limit_requests_per_sec;
    
    axum::Router::new()
        .route("/", get(|| async { "StellarGate API v0.1.0" }))
        .route("/health", get(health))
        .route("/openapi.json", get(openapi))
        .route("/docs", get(swagger_ui))
        .nest("/payments", {
            axum::Router::new()
                .route("/", post(payments::create).get(payments::list))
                .route("/:id", get(payments::get_by_id))
                .route("/:id/webhooks", get(payments::list_webhooks))
                .route("/:id/webhooks/:delivery_id/redeliver", post(payments::redeliver_webhook))
                .layer(middleware::from_fn_with_state(
                    rate_limit_rps,
                    rate_limit_middleware,
                ))
        })
        .fallback(not_found)
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(TraceLayer::new_for_http())
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
        .layer(RequestBodyLimitLayer::new(MAX_BODY_BYTES))
        .layer(cors)
        .with_state(state)
}

async fn rate_limit_middleware(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    rate_limit_rps: u32,
    req: Request,
    next: Next,
) -> axum::response::Response {
    static LIMITERS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, governor::RateLimiter>>> = std::sync::OnceLock::new();
    
    let limiters = LIMITERS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    let ip = addr.ip().to_string();
    
    let mut map = limiters.lock().unwrap();
    let limiter = map.entry(ip).or_insert_with(|| {
        governor::RateLimiter::direct(
            governor::Quota::per_second(
                std::num::NonZeroU32::new(rate_limit_rps).unwrap()
            )
        )
    });
    
    if limiter.check().is_err() {
        let retry_after = (1000 / rate_limit_rps).max(1);
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(
                axum::http::header::RETRY_AFTER,
                axum::http::HeaderValue::from_str(&retry_after.to_string()).unwrap(),
            )],
            axum::Json(json!({
                "error": "rate limit exceeded",
                "code": "rate_limit_exceeded"
            })),
        )
            .into_response();
    }
    
    next.run(req).await
}

fn build_cors(cfg: &crate::config::Config) -> CorsLayer {
    use axum::http::HeaderName;
    use tower_http::cors::AllowOrigin;

    let origins = &cfg.cors_allowed_origins;

    if origins.is_empty() {
        if cfg.network == "public" {
            tracing::warn!(
                "CORS_ALLOWED_ORIGINS is not set on a public-network deployment. \
                 All origins are allowed — set CORS_ALLOWED_ORIGINS in production."
            );
        }
        return CorsLayer::permissive();
    }

    let allow_origins: Vec<axum::http::HeaderValue> = origins
        .iter()
        .filter_map(|o| o.parse().ok())
        .collect();

    CorsLayer::new()
        .allow_origin(AllowOrigin::list(allow_origins))
        .allow_methods([
            axum::http::Method::GET,
            axum::http::Method::POST,
            axum::http::Method::OPTIONS,
        ])
        .allow_headers([
            HeaderName::from_static("content-type"),
            HeaderName::from_static("authorization"),
        ])
}

async fn health() -> impl IntoResponse {
    Json(json!({ "status": "ok" }))
}

async fn not_found() -> impl IntoResponse {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "not found", "code": "not_found" })),
    )
}
