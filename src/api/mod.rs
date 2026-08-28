use crate::api::payments::{AppError, JsonBody};
use crate::{db, AppState};
use axum::{
    extract::{ConnectInfo, Path, Request, State},
    http::{header, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::IntoResponse,
    routing::{get, post},
    Json,
};
use moka::sync::Cache;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;
use tower_http::{
    cors::CorsLayer,
    limit::RequestBodyLimitLayer,
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
    timeout::TimeoutLayer,
    trace::TraceLayer,
};

mod payments;

/// Reject request bodies larger than this (256 KiB) before they hit a handler.
const MAX_BODY_BYTES: usize = 256 * 1024;

/// The authenticated merchant ID injected by the auth middleware.
#[derive(Clone)]
pub struct AuthenticatedMerchant(pub String);

/// Maximum number of distinct IP+bucket keys tracked at once.
/// Once this is reached, moka evicts the least-recently-used entry,
/// bounding resident memory regardless of key cardinality.
const RATE_LIMITER_MAX_KEYS: u64 = 10_000;

/// How long a per-key limiter is retained after its last access.
/// Keys for IPs that go quiet are automatically reclaimed.
const RATE_LIMITER_IDLE_TTL: Duration = Duration::from_secs(60);

#[derive(Clone)]
struct RateLimitState {
    requests_per_sec: u32,
    /// Bounded, TTL-evicting cache of per-(bucket, IP) rate limiters.
    ///
    /// Replaces the previous `Mutex<HashMap<...>>`:
    /// - Capacity is capped at `RATE_LIMITER_MAX_KEYS` entries (moka evicts
    ///   via a W-TinyLFU policy when the cap is hit).
    /// - Each entry expires `RATE_LIMITER_IDLE_TTL` after its last access,
    ///   so limiter state for quiet IPs is automatically reclaimed.
    /// - moka uses internal sharding, eliminating the single global lock that
    ///   the old `Mutex` imposed.
    limiters: Cache<String, Arc<governor::DefaultDirectRateLimiter>>,
}

impl RateLimitState {
    fn new(requests_per_sec: u32) -> Self {
        let limiters = Cache::builder()
            .max_capacity(RATE_LIMITER_MAX_KEYS)
            .time_to_idle(RATE_LIMITER_IDLE_TTL)
            .build();
        Self {
            requests_per_sec: requests_per_sec.max(1),
            limiters,
        }
    }
}

pub fn router(state: Arc<AppState>) -> axum::Router {
    let cors = build_cors(&state.config);
    let rate_limit = RateLimitState::new(state.config.rate_limit_requests_per_sec);
    let request_timeout = Duration::from_secs(state.config.request_timeout_secs);

    axum::Router::new()
        .route("/", get(root))
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/metrics", get(metrics_handler))
        /* Operator dashboard. The assets are static and carry no data, so they
        are served unauthenticated — every figure they display is fetched by
        the browser from the same authenticated endpoints a merchant would
        call directly, using an API key the operator supplies. */
        .route("/dashboard", get(dashboard_html))
        .route("/dashboard/app.css", get(dashboard_css))
        .route("/dashboard/app.js", get(dashboard_js))
        /* The versioned API surface, mounted twice.
        `/v1` is canonical. The same routes stay mounted unprefixed so every
        existing integrator keeps working — shipping versioning by breaking all
        current callers at once would be precisely the failure versioning
        exists to prevent (issue #121). Legacy responses carry `Deprecation`
        and `Link` headers pointing at their `/v1` equivalent, so a client can
        discover the move from a response it already parses. */
        .nest("/v1", api_v1(&state))
        .merge(api_v1(&state).layer(middleware::from_fn(mark_deprecated)))
        .fallback(not_found)
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(TraceLayer::new_for_http())
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
        .layer(RequestBodyLimitLayer::new(MAX_BODY_BYTES))
        .layer(middleware::from_fn_with_state(
            rate_limit,
            rate_limit_middleware,
        ))
        .layer(cors)
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            request_timeout,
        ))
        .with_state(state)
}

/// The versioned API surface: everything that forms the public contract.
///
/// Operational endpoints (`/health`, `/ready`, `/metrics`, `/dashboard`, `/`)
/// are deliberately excluded. They are infrastructure rather than contract —
/// a probe URL that moved with every API revision would break liveness checks
/// and scrape configs for no benefit.
fn api_v1(state: &Arc<AppState>) -> axum::Router<Arc<AppState>> {
    /* Merchant provisioning and API key lifecycle. All admin-gated behind
    ADMIN_PROVISIONING_SECRET: this service has no self-service signup, and
    minting or revoking a credential is an operator action. */
    let merchants = axum::Router::new()
        .route("/", post(provision_merchant))
        .route("/:id/keys", post(issue_api_key).get(list_api_keys))
        .route("/:id/keys/:key_id", axum::routing::delete(revoke_api_key))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_admin_secret,
        ));

    /* Auth middleware on the write + list routes, the webhook listing, and
    redelivery (it triggers a merchant-scoped outbound request). The
    per-payment status endpoint handles credentials itself, because it serves
    both authenticated and anonymous callers — see `payments::get_by_id`. */
    let payments_authed = axum::Router::new()
        .route("/", post(payments::create).get(payments::list))
        .route("/:id/webhooks", get(payments::list_webhooks))
        .route(
            "/:id/webhooks/:delivery_id/redeliver",
            post(payments::redeliver_webhook),
        )
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));

    axum::Router::new().nest("/merchants", merchants).nest(
        "/payments",
        axum::Router::new()
            .merge(payments_authed)
            .route("/:id", get(payments::get_by_id)),
    )
}

/// Tag a response served from an unversioned path as deprecated.
///
/// `Deprecation` and `Link: rel="successor-version"` are the RFC 8594 /
/// RFC 8288 way of saying this, so a client can find the replacement in a
/// response it is already parsing rather than by reading release notes.
///
/// No `Sunset` header is emitted. That header is a commitment to a removal
/// date, and inventing one here would announce a promise nobody has made —
/// see the deprecation policy in the README.
async fn mark_deprecated(req: Request, next: Next) -> axum::response::Response {
    let successor = format!("/v1{}", req.uri().path());
    let mut res = next.run(req).await;

    let headers = res.headers_mut();
    headers.insert(
        axum::http::HeaderName::from_static("deprecation"),
        HeaderValue::from_static("true"),
    );
    if let Ok(link) = HeaderValue::from_str(&format!("<{successor}>; rel=\"successor-version\"")) {
        headers.insert(header::LINK, link);
    }
    res
}

/// Authenticates via the `Authorization: Bearer <key>` header, injecting
/// [`AuthenticatedMerchant`] into request extensions on success.
///
/// Every outcome is both logged (`source_ip` + `reason`, at a level matched
/// to severity — failures visible by default, success at `debug` to avoid
/// flooding logs) and counted in `AuthMetrics` (issue #139), so
/// credential-stuffing or a misconfigured client shows up in logs/metrics
/// instead of silently returning 401s.
async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    mut req: Request,
    next: Next,
) -> axum::response::Response {
    let source_ip = client_ip_key(&req);

    let raw_key = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(str::to_string);

    let Some(key) = raw_key else {
        tracing::warn!(
            %source_ip,
            reason = "missing_key",
            "auth denied: missing or malformed Authorization header"
        );
        state.auth_metrics.record_failure_missing_key();
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "missing or invalid Authorization header", "code": "unauthorized" })),
        )
            .into_response();
    };

    match db::find_merchant_by_key(&state.pool, &key).await {
        Ok(Some(merchant_id)) => {
            tracing::debug!(%source_ip, %merchant_id, "auth succeeded");
            state.auth_metrics.record_success();
            req.extensions_mut()
                .insert(AuthenticatedMerchant(merchant_id));
            next.run(req).await
        }
        Ok(None) => {
            tracing::warn!(
                %source_ip,
                reason = "invalid_key",
                "auth denied: API key did not match any merchant"
            );
            state.auth_metrics.record_failure_invalid_key();
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "invalid API key", "code": "unauthorized" })),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!(%source_ip, error = %e, "auth errored: merchant key lookup failed");
            state.auth_metrics.record_failure_internal_error();
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "internal server error", "code": "internal_error" })),
            )
                .into_response()
        }
    }
}

/// Guards `POST /merchants` with a shared admin secret sent via the
/// `X-Admin-Secret` header. An unset `ADMIN_PROVISIONING_SECRET` disables
/// provisioning entirely rather than leaving the endpoint open.
async fn require_admin_secret(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> axum::response::Response {
    let configured = &state.config.admin_provisioning_secret;

    // An empty configured secret means provisioning is disabled — reject
    // immediately without touching any caller-supplied value (issue #244).
    if configured.is_empty() {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "missing or invalid admin secret", "code": "unauthorized" })),
        )
            .into_response();
    }

    // Reject a missing or non-UTF-8 header before any comparison.
    let Some(provided) = req
        .headers()
        .get("x-admin-secret")
        .and_then(|v| v.to_str().ok())
    else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "missing or invalid admin secret", "code": "unauthorized" })),
        )
            .into_response();
    };

    // Compare SHA-256 digests in constant time (issue #244).
    //
    // `str` equality short-circuits on the first differing byte, leaking a
    // prefix-match oracle. Hashing both sides with SHA-256 normalises them to
    // fixed-length byte arrays; `==` on `[u8; 32]` uses a fixed-time
    // comparison in the standard library, removing the oracle. Salting is
    // unnecessary here — we are comparing a caller-supplied value against a
    // configured secret, not storing a password.
    use sha2::{Digest, Sha256};
    let provided_digest = Sha256::digest(provided.as_bytes());
    let configured_digest = Sha256::digest(configured.as_bytes());

    if provided_digest != configured_digest {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "missing or invalid admin secret", "code": "unauthorized" })),
        )
            .into_response();
    }

    next.run(req).await
}

/// `POST /merchants` — provision a new merchant and return its API key once.
/// Requires the `X-Admin-Secret` header (see `require_admin_secret`).
async fn provision_merchant(
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let merchant_id = uuid::Uuid::new_v4().to_string();
    let (raw_key, prefix) = db::generate_api_key();

    let key_id = db::create_merchant(&state.pool, &merchant_id, &raw_key, &prefix)
        .await
        .map_err(|e| {
            // Issue #125: swallowing this left an operator with a 500 and no
            // way to tell a disk error from a UNIQUE collision.
            tracing::error!(error = %e, "failed to provision merchant");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "internal server error", "code": "internal_error" })),
            )
        })?;

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "merchant_id": merchant_id,
            "api_key": raw_key,
            "key_id": key_id,
        })),
    ))
}

/// `POST /merchants/:id/keys` — issue an additional API key.
///
/// Rotation is issue-then-revoke rather than replace-in-place: the new key is
/// live immediately while the old one keeps working, so a merchant can deploy
/// the new credential before retiring the old one and never has a window with
/// no valid key. Admin-gated, like provisioning.
async fn issue_api_key(
    State(state): State<Arc<AppState>>,
    Path(merchant_id): Path<String>,
    body: Option<JsonBody<IssueKeyRequest>>,
) -> Result<impl IntoResponse, AppError> {
    if !db::merchant_exists(&state.pool, &merchant_id).await? {
        return Err(AppError::not_found(
            "merchant_not_found",
            "merchant not found",
        ));
    }

    let label = body.and_then(|JsonBody(b)| b.label);
    if let Some(l) = &label {
        if l.len() > 100 {
            return Err(AppError::bad_request(
                "invalid_label",
                "label exceeds max length of 100 characters",
            ));
        }
    }

    let (raw_key, prefix) = db::generate_api_key();
    let key_id = db::create_api_key(
        &state.pool,
        &merchant_id,
        &raw_key,
        &prefix,
        label.as_deref(),
    )
    .await?;

    tracing::info!(%merchant_id, %key_id, "api key issued");

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "key_id": key_id,
            "api_key": raw_key,
            "prefix": prefix,
            "label": label,
        })),
    ))
}

/// `GET /merchants/:id/keys` — list a merchant's keys.
///
/// Returns metadata only. The secret is unrecoverable by design, so this can
/// never leak a usable credential; `prefix` exists so an operator can identify
/// which key to revoke.
async fn list_api_keys(
    State(state): State<Arc<AppState>>,
    Path(merchant_id): Path<String>,
) -> Result<Json<Value>, AppError> {
    if !db::merchant_exists(&state.pool, &merchant_id).await? {
        return Err(AppError::not_found(
            "merchant_not_found",
            "merchant not found",
        ));
    }

    let keys = db::list_api_keys(&state.pool, &merchant_id).await?;
    Ok(Json(json!({
        "merchant_id": merchant_id,
        "keys": keys.iter().map(|k| json!({
            "key_id": k.id,
            "prefix": k.prefix,
            "label": k.label,
            "created_at": k.created_at,
            "last_used_at": k.last_used_at,
            "revoked_at": k.revoked_at,
            "active": k.revoked_at.is_none(),
        })).collect::<Vec<_>>(),
    })))
}

/// `DELETE /merchants/:id/keys/:key_id` — revoke a key immediately.
///
/// Refuses to revoke a merchant's last active key. Doing so would lock them
/// out of an API that has no self-service recovery, turning a routine
/// revocation into an incident; issue a replacement first.
async fn revoke_api_key(
    State(state): State<Arc<AppState>>,
    Path((merchant_id, key_id)): Path<(String, String)>,
) -> Result<impl IntoResponse, AppError> {
    if !db::merchant_exists(&state.pool, &merchant_id).await? {
        return Err(AppError::not_found(
            "merchant_not_found",
            "merchant not found",
        ));
    }

    // The guard against revoking the last active key is now enforced atomically
    // inside the UPDATE via a subquery (issue #247). The previous
    // check-then-act split (count_active_api_keys → revoke_api_key) could be
    // bypassed by two concurrent revocations of a merchant's two remaining
    // keys: both would read a count of 2, both pass the guard, and both
    // succeed, locking the merchant out. RevokeKeyOutcome::LastActiveKey is
    // returned when the atomic guard fires.
    match db::revoke_api_key(&state.pool, &merchant_id, &key_id).await? {
        db::RevokeKeyOutcome::Revoked => {
            let source_ip = client_ip_key_from_parts(
                Some(peer),
                &headers,
                &state.config.trusted_proxy_cidrs,
            );
            tracing::warn!(
                audit = true,
                action = "api_key.revoke",
                actor = "admin",
                outcome = "revoked",
                %merchant_id,
                %key_id,
                source_ip = %source_ip,
                request_id = %request_id(&headers),
                "api key revoked"
            );
            Ok((
                StatusCode::OK,
                Json(json!({ "key_id": key_id, "revoked": true })),
            ))
        }
        db::RevokeKeyOutcome::LastActiveKey => Err(AppError::bad_request(
            "last_active_key",
            "cannot revoke a merchant's only active key — issue a replacement first",
        )),
        db::RevokeKeyOutcome::NotFound => Err(AppError::not_found(
            "key_not_found",
            "no active key with that id for this merchant",
        )),
    }
}

#[derive(serde::Deserialize)]
struct IssueKeyRequest {
    label: Option<String>,
}

async fn rate_limit_middleware(
    State(rate_limit): State<RateLimitState>,
    req: Request,
    next: Next,
) -> axum::response::Response {
    if let Some(bucket) = rate_limited_bucket(&req) {
        let key = rate_limit_key(bucket, &req);
        let base_rps = rate_limit.requests_per_sec;
        let effective_rps = base_rps
            .saturating_mul(bucket_rate_multiplier(bucket))
            .max(1);
        /* `get_with` clones the `Arc` out of the cache rather than handing back a
        guard, so nothing borrowed from the cache is held across the `.await`
        below. */
        let limiter = rate_limit.limiters.get_with(key, || {
            Arc::new(governor::RateLimiter::direct(governor::Quota::per_second(
                NonZeroU32::new(effective_rps)
                    .expect("effective_rps is clamped to at least 1"),
            )))
        });

        if limiter.check().is_err() {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                [(header::RETRY_AFTER, HeaderValue::from_static("1"))],
                Json(json!({
                    "error": "rate limit exceeded",
                    "code": "rate_limit_exceeded"
                })),
            )
                .into_response();
        }
    }

    next.run(req).await
}

/// Identifies which rate-limit bucket a request falls into.
///
/// Every request is assigned a bucket so all routes are protected by default.
/// Write and sensitive routes use named buckets that receive the base quota
/// (`requests_per_sec × 1`). Read-only routes fall into the `"default"` bucket
/// which receives a more generous quota (`requests_per_sec × 5`) to avoid
/// throttling normal polling.
///
/// Redelivery is bucketed by shape rather than by path: the URL carries a
/// payment and delivery id, and keying on those would let every id mint its
/// own limiter entry — both an unbounded map and a trivially bypassed limit.
fn rate_limited_bucket(req: &Request) -> Option<&'static str> {
    let path = req.uri().path();
    if req.method() == axum::http::Method::POST {
        return match path {
            "/payments" => Some("payments"),
            "/merchants" => Some("merchants"),
            _ if path.starts_with("/payments/") && path.ends_with("/redeliver") => {
                Some("redeliver")
            }
            // Key issuance: POST /merchants/:id/keys — credential lifecycle
            // belongs in the "merchants" write bucket, not the default read
            // bucket that gives it 5× the quota (issue #243).
            _ if path.starts_with("/merchants/") => Some("merchants"),
            _ => Some("default"),
        };
    }
    // DELETE is a write/destructive operation. Key revocation
    // (DELETE /merchants/:id/keys/:key_id) must be treated as a write like
    // POST /merchants, not as cheap read-only traffic (issue #243).
    if req.method() == axum::http::Method::DELETE
        && path.starts_with("/merchants/")
        && path.contains("/keys/")
    {
        return Some("merchants");
    }
    // All other non-POST requests (GET, etc.) fall into the default bucket so
    // that payment enumeration and webhook listing are covered by a baseline
    // limit.
    Some("default")
}

/// Returns the rate multiplier for a bucket.
///
/// Write/sensitive buckets get the base rate (× 1). Read-only traffic gets a
/// higher allowance (× 5) so normal API consumers aren't throttled by polling.
fn bucket_rate_multiplier(bucket: &str) -> u32 {
    match bucket {
        "payments" | "merchants" | "redeliver" => 1,
        _ => 5,
    }
}

/// Keyed by bucket + client so each bucket is rate-limited independently —
/// provisioning a merchant should never eat into a client's payment quota (or
/// vice versa).
fn rate_limit_key(bucket: &str, req: &Request) -> String {
    format!("{bucket}:{}", client_ip_key(req))
}

fn client_ip_key(req: &Request) -> String {
    if let Some(ConnectInfo(addr)) = req.extensions().get::<ConnectInfo<SocketAddr>>() {
        return addr.ip().to_string();
    }

    for name in ["x-forwarded-for", "x-real-ip"] {
        if let Some(value) = req.headers().get(name).and_then(|v| v.to_str().ok()) {
            if let Some(first) = value.split(',').map(str::trim).find(|s| !s.is_empty()) {
                return first.to_string();
            }
        }
    }

    "local".to_string()
}

fn build_cors(cfg: &crate::config::Config) -> CorsLayer {
    use axum::http::HeaderName;
    use tower_http::cors::AllowOrigin;

    let origins = &cfg.cors_allowed_origins;

    if origins.is_empty() {
        if cfg.network == "public" {
            tracing::error!(
                "CORS_ALLOWED_ORIGINS is not set on a public-network deployment. \
                 Denying all origins by default."
            );
            return CorsLayer::new();
        }
        tracing::warn!(
            "CORS_ALLOWED_ORIGINS is not set on a testnet deployment. \
             Falling back to permissive CORS for development and test environments."
        );
        return CorsLayer::permissive();
    }

    let allow_origins: Vec<axum::http::HeaderValue> = origins
        .iter()
        .map(|o| {
            o.parse().unwrap_or_else(|e| {
                // Origins are validated in Config::from_env, so this branch is
                // unreachable in production. Treat it as a programming error.
                panic!("BUG: unparseable CORS origin {o:?} reached build_cors: {e}")
            })
        })
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

/// Service root.
///
/// A browser gets sent to the dashboard; anything else gets the version string.
/// Hosting platforms hand you the root URL ("open in browser"), so a bare line
/// of text there reads as "nothing is running" and gives no hint that a UI
/// exists one path away.
///
/// The split is on `Accept`, not User-Agent: `fetch()` and `curl` send `*/*`
/// and keep the plaintext response, so the dashboard's own version lookup and
/// any uptime check are unaffected.
async fn root(headers: axum::http::HeaderMap) -> impl IntoResponse {
    let wants_html = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|accept| accept.contains("text/html"));

    if wants_html {
        axum::response::Redirect::temporary("/dashboard").into_response()
    } else {
        (
            StatusCode::OK,
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; charset=utf-8"),
            )],
            concat!("StellarGate API v", env!("CARGO_PKG_VERSION")),
        )
            .into_response()
    }
}

async fn health() -> impl IntoResponse {
    Json(json!({ "status": "ok" }))
}

/// Readiness probe — returns 200 only when both the database AND Horizon are
/// reachable. A pod that cannot reach Horizon cannot detect on-chain payments;
/// routing traffic to it is worse than routing it elsewhere (issue #172).
///
/// Uses a 3-second timeout on the Horizon check so a slow node never hangs
/// the probe. The check is skipped when no gateway is configured
/// (STELLAR_GATEWAY_PUBLIC=UNCONFIGURED) since without a gateway there is no
/// on-chain work to do.
async fn ready(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    // 1. Database must respond.
    if db::ping(&state.pool).await.is_err() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "status": "unavailable", "reason": "database unreachable" })),
        )
            .into_response();
    }

    // 2. Horizon must respond (only when a gateway wallet is configured).
    if state.config.gateway_configured() {
        if let Err(reason) = check_horizon_ready(&state).await {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "status": "unavailable", "reason": reason })),
            )
                .into_response();
        }
    }

    (StatusCode::OK, Json(json!({ "status": "ok" }))).into_response()
}

/// Probe Horizon with a hard 3-second timeout.
/// Returns Ok(()) when reachable (any non-5xx response), or an error string.
async fn check_horizon_ready(state: &Arc<AppState>) -> Result<(), String> {
    let url = state.config.horizon_url.trim_end_matches('/').to_string();
    let result = tokio::time::timeout(
        Duration::from_millis(3_000),
        state
            .http
            .get(&url)
            .header("Accept", "application/json")
            .send(),
    )
    .await;
    match result {
        Ok(Ok(resp)) if resp.status().as_u16() < 500 => Ok(()),
        Ok(Ok(resp)) => Err(format!("Horizon returned {}", resp.status())),
        Ok(Err(e)) => Err(format!("Horizon unreachable: {e}")),
        Err(_) => Err("Horizon health check timed out".to_string()),
    }
}

/// `GET /metrics` — Prometheus-compatible plain-text metrics snapshot.
async fn metrics_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let body = crate::metrics::render(
        &state.webhook_metrics,
        &state.auth_metrics,
        &state.horizon_metrics,
    );
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
        )],
        body,
    )
}

/* The dashboard is compiled into the binary rather than read from disk, so a
deployment stays a single artifact with no asset path to configure and no way
for the two to drift apart. */
const DASHBOARD_HTML: &str = include_str!("../../static/dashboard.html");
const DASHBOARD_CSS: &str = include_str!("../../static/dashboard.css");
const DASHBOARD_JS: &str = include_str!("../../static/dashboard.js");

/// Locks the dashboard to its own origin: no third-party script, style, frame
/// or connection. The page ships no inline script or style, so this needs no
/// `unsafe-inline` escape hatch.
const DASHBOARD_CSP: &str = "default-src 'none'; \
     script-src 'self'; \
     style-src 'self'; \
     img-src 'self' data:; \
     connect-src 'self'; \
     form-action 'none'; \
     frame-ancestors 'none'; \
     base-uri 'none'";

fn dashboard_asset(body: &'static str, content_type: &'static str) -> impl IntoResponse {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, HeaderValue::from_static(content_type)),
            (
                header::CONTENT_SECURITY_POLICY,
                HeaderValue::from_static(DASHBOARD_CSP),
            ),
            (
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            ),
        ],
        body,
    )
}

async fn dashboard_html() -> impl IntoResponse {
    dashboard_asset(DASHBOARD_HTML, "text/html; charset=utf-8")
}

async fn dashboard_css() -> impl IntoResponse {
    dashboard_asset(DASHBOARD_CSS, "text/css; charset=utf-8")
}

async fn dashboard_js() -> impl IntoResponse {
    dashboard_asset(DASHBOARD_JS, "text/javascript; charset=utf-8")
}

async fn not_found() -> impl IntoResponse {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "not found", "code": "not_found" })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum_test::TestServer;
    use tower_http::timeout::TimeoutLayer;

    /// Exercises the exact `TimeoutLayer` construction used in `router()`,
    /// against a router small enough to run with millisecond durations —
    /// `request_timeout_secs` itself is whole seconds, too coarse for a fast test.
    fn timeout_test_router(timeout: Duration) -> axum::Router {
        axum::Router::new()
            .route(
                "/slow",
                get(|| async {
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                }),
            )
            .route("/fast", get(|| async { "ok" }))
            .layer(TimeoutLayer::with_status_code(
                StatusCode::REQUEST_TIMEOUT,
                timeout,
            ))
    }

    #[tokio::test]
    async fn slow_handler_is_aborted_with_408() {
        let server = TestServer::new(timeout_test_router(Duration::from_millis(20)))
            .expect("timeout test router should build");
        let response = server.get("/slow").await;
        response.assert_status(StatusCode::REQUEST_TIMEOUT);
    }

    #[tokio::test]
    async fn fast_handler_is_unaffected() {
        let server = TestServer::new(timeout_test_router(Duration::from_millis(200))).unwrap();
        let response = server.get("/fast").await;
        response.assert_status_ok();
    }

    // ── client_ip_key (issue #330) ───────────────────────────────────────────

    /// Build a bare request with an optional peer IP and headers, the inputs
    /// `client_ip_key` actually reads (extensions + headers).
    fn req_with(peer: Option<&str>, headers: &[(&str, &str)]) -> Request<axum::body::Body> {
        let mut req = Request::builder().body(axum::body::Body::empty()).unwrap();
        if let Some(ip) = peer {
            let addr = SocketAddr::new(ip.parse().unwrap(), 0);
            req.extensions_mut().insert(ConnectInfo(addr));
        }
        for (name, value) in headers {
            req.headers_mut().insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }
        req
    }

    fn cidrs(list: &[&str]) -> Vec<IpNet> {
        list.iter().map(|c| c.parse().unwrap()).collect()
    }

    /// TEST-NET-3 (RFC 5737) — documentation IPs, safe to use as fake clients.
    const PEER: &str = "203.0.113.9";
    const FAKE_CLIENT: &str = "198.51.100.7";

    #[test]
    fn untrusted_peer_ignores_spoofed_forwarding_headers() {
        let req = req_with(
            Some(PEER),
            &[
                ("x-forwarded-for", FAKE_CLIENT),
                ("x-real-ip", "198.51.100.8"),
            ],
        );
        assert_eq!(
            client_ip_key(&req, &cidrs(&["10.0.0.0/8"])),
            PEER,
            "an untrusted peer must be attributed by its own address"
        );
    }

    #[test]
    fn headers_ignored_when_no_trusted_proxies_configured() {
        // The acceptance criterion: with TRUSTED_PROXY_CIDRS unset, forwarding
        // headers are ignored regardless of ConnectInfo.
        let req = req_with(Some(PEER), &[("x-forwarded-for", FAKE_CLIENT)]);
        assert_eq!(client_ip_key(&req, &[]), PEER);
    }

    #[test]
    fn trusted_proxy_chain_takes_rightmost_non_trusted_hop() {
        // Peer is a trusted proxy; the chain was appended by trusted proxies
        // (rightmost first), so the leftmost client survives.
        let req = req_with(
            Some("10.0.0.1"),
            &[("x-forwarded-for", "198.51.100.7, 10.0.0.5, 10.0.0.1")],
        );
        assert_eq!(client_ip_key(&req, &cidrs(&["10.0.0.0/8"])), FAKE_CLIENT);
    }

    #[test]
    fn trusted_proxy_chain_of_only_trusted_hops_falls_back_to_peer() {
        let req = req_with(
            Some("10.0.0.1"),
            &[("x-forwarded-for", "10.0.0.9, 10.0.0.1")],
        );
        assert_eq!(client_ip_key(&req, &cidrs(&["10.0.0.0/8"])), "10.0.0.1");
    }

    #[test]
    fn trusted_proxy_honors_x_real_ip_when_no_forwarded_for() {
        let req = req_with(Some("10.0.0.1"), &[("x-real-ip", FAKE_CLIENT)]);
        assert_eq!(client_ip_key(&req, &cidrs(&["10.0.0.0/8"])), FAKE_CLIENT);
    }

    #[test]
    fn missing_connect_info_fails_closed_to_shared_key() {
        // Router served without connect info: headers must never be trusted,
        // and every request shares one key rather than minting per-header ones.
        let req = req_with(None, &[("x-forwarded-for", FAKE_CLIENT)]);
        assert_eq!(
            client_ip_key(&req, &cidrs(&["10.0.0.0/8"])),
            CLIENT_IP_UNKNOWN
        );
    }

    #[test]
    fn plain_peer_with_no_headers_uses_peer_address() {
        let req = req_with(Some(PEER), &[]);
        assert_eq!(client_ip_key(&req, &cidrs(&["10.0.0.0/8"])), PEER);
    }

    // ── reset_and_retry_after (issue #327) ───────────────────────────────────
    //
    // These are where the "derived, not fabricated" claim is actually pinned.
    // The integration tests cannot do it: every quota the service builds is a
    // `Quota::per_second(n)`, under which one cell replenishes in `1/n` seconds,
    // so the wait is always sub-second and `Retry-After` — an integer per
    // RFC 9110 — rounds to `1` at every configured rate. The hard-coded `1` was
    // numerically indistinguishable from the truth for every reachable
    // configuration, which is precisely why it survived review.
    //
    // A slow quota is where the two diverge, so that is what these use. They
    // also mean the derivation stays correct if the quota shape ever changes
    // (a per-minute window, a burst allowance) without anyone remembering that
    // a constant elsewhere encodes an assumption about it.

    /// A ten-minute replenish interval: the derived answer is 600, a fabricated
    /// one is 1.
    #[test]
    fn retry_after_follows_a_slow_quota_instead_of_a_constant() {
        assert_eq!(retry_after_secs(Duration::from_secs(600)), 600);
    }

    #[test]
    fn retry_after_rounds_up_rather_than_truncating() {
        // 1.4s truncated to 1 would retry into another rejection.
        assert_eq!(retry_after_secs(Duration::from_millis(1_400)), 2);
    }

    #[test]
    fn retry_after_never_tells_a_client_to_retry_immediately() {
        assert_eq!(retry_after_secs(Duration::ZERO), 1);
        assert_eq!(retry_after_secs(Duration::from_millis(1)), 1);
    }

    fn slow_quota(period: Duration, burst: u32) -> governor::Quota {
        governor::Quota::with_period(period)
            .unwrap()
            .allow_burst(NonZeroU32::new(burst).unwrap())
    }

    #[test]
    fn reset_is_zero_when_the_bucket_is_untouched() {
        let quota = slow_quota(Duration::from_secs(60), 5);
        assert_eq!(reset_secs(quota, 5, Duration::ZERO), 0);
    }

    #[test]
    fn reset_counts_every_missing_cell_not_just_the_next_one() {
        // 3 of 5 cells spent, one minute each to replenish → 180s to full.
        let quota = slow_quota(Duration::from_secs(60), 5);
        assert_eq!(reset_secs(quota, 2, Duration::ZERO), 180);
    }

    #[test]
    fn reset_on_a_drained_bucket_covers_the_whole_refill() {
        // Empty, with the next cell 60s out: still 5 × 60s to full capacity.
        let quota = slow_quota(Duration::from_secs(60), 5);
        assert_eq!(reset_secs(quota, 0, Duration::from_secs(60)), 300);
    }

    /// `Reset` is time-to-full and `Retry-After` is time-to-one-cell, so the
    /// former can never be the smaller of the two. A client that waited
    /// `Reset` and still got throttled would have no way to make progress.
    #[test]
    fn reset_is_never_shorter_than_retry_after() {
        let quota = slow_quota(Duration::from_secs(600), 1);
        let wait = Duration::from_secs(600);
        assert!(reset_secs(quota, 0, wait) >= retry_after_secs(wait));
    }

    // ── rate_limited_bucket — probe exemption ────────────────────────────────

    fn method_req(method: axum::http::Method, path: &str) -> Request<axum::body::Body> {
        Request::builder()
            .method(method)
            .uri(path)
            .body(axum::body::Body::empty())
            .unwrap()
    }

    #[test]
    fn probe_endpoints_bypass_the_rate_limiter() {
        for path in ["/health", "/ready", "/metrics"] {
            assert_eq!(
                rate_limited_bucket(&method_req(axum::http::Method::GET, path)),
                None,
                "{path} must return None so rate_limit_middleware skips it entirely"
            );
        }
    }

    /// The exemption is scoped to exactly the three probe paths — it must not
    /// widen into "every GET is exempt", which would silently undo the
    /// per-IP protection on payment enumeration and webhook listing.
    #[test]
    fn other_get_routes_still_fall_into_the_default_bucket() {
        assert_eq!(
            rate_limited_bucket(&method_req(axum::http::Method::GET, "/payments/abc")),
            Some("default")
        );
    }

    #[test]
    fn test_rate_limit_bucket_assignment_all_routes() {
        use axum::http::Method;
        let cases = [
            (Method::POST, "/payments", Some("payments")),
            (Method::POST, "/v1/payments", Some("payments")),
            (Method::POST, "/merchants", Some("merchants")),
            (Method::POST, "/v1/merchants", Some("merchants")),
            (
                Method::POST,
                "/payments/x/webhooks/y/redeliver",
                Some("redeliver"),
            ),
            (
                Method::POST,
                "/v1/payments/x/webhooks/y/redeliver",
                Some("redeliver"),
            ),
            // Key issuance: POST to /merchants/:id/keys must be in the
            // write "merchants" bucket, not the 5× read default (issue #243).
            (Method::POST, "/merchants/abc/keys", Some("merchants")),
            (Method::POST, "/v1/merchants/abc/keys", Some("merchants")),
            // Key revocation: DELETE is a destructive write, same bucket (issue #243).
            (
                Method::DELETE,
                "/merchants/abc/keys/key-id",
                Some("merchants"),
            ),
            (
                Method::DELETE,
                "/v1/merchants/abc/keys/key-id",
                Some("merchants"),
            ),
            (Method::GET, "/health", None),
            (Method::GET, "/v1/health", None),
        ];

        for (method, path, expected_bucket) in cases {
            let req = method_req(method.clone(), path);
            assert_eq!(
                rate_limited_bucket(&req),
                expected_bucket,
                "{method} {path}"
            );
        }
    }

    #[test]
    fn test_bucket_rate_multiplier() {
        assert_eq!(bucket_rate_multiplier("payments"), 1);
        assert_eq!(bucket_rate_multiplier("merchants"), 1);
        assert_eq!(bucket_rate_multiplier("redeliver"), 1);
        assert_eq!(bucket_rate_multiplier("default"), 5);
        assert_eq!(bucket_rate_multiplier("unknown"), 5);
    }

    // ── baseline security headers (issue #251) ──────────────────────────────

    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;

    /// Build a full `Config` for a header test. Mostly mirrors `expiry`'s
    /// helper; the only field that varies here is `network`.
    fn header_test_config(network: &str) -> crate::config::Config {
        use crate::config::{AcceptedAsset, ListenerMode, WebhookPayloadDetail};
        crate::config::Config {
            port: 0,
            database_url: "sqlite::memory:".into(),
            network: network.into(),
            horizon_url: "https://horizon.invalid".parse().unwrap(),
            gateway_public: "GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5".into(),
            accepted_assets: AcceptedAsset::default_list(),
            webhook_secret: "a-very-long-and-secure-webhook-signing-secret-32-chars".into(),
            webhook_retry_attempts: 1,
            webhook_retry_delay_ms: 0,
            webhook_retry_max_delay_ms: 60_000,
            allowed_webhook_schemes: vec!["https".into()],
            webhook_payload_detail: WebhookPayloadDetail::Minimal,
            webhook_timeout_secs: 10,
            webhook_redrive_interval_secs: 30,
            webhook_redrive_concurrency: 4,
            webhook_redrive_max_attempts: 8,
            webhook_redrive_grace_secs: 60,
            webhook_redrive_backoff_initial_secs: 0,
            webhook_redrive_backoff_max_secs: 0,
            webhook_redrive_jitter_secs: 0,
            retention_interval_secs: 3600,
            webhook_delivery_retention_days: 30,
            idempotency_retention_days: 7,
            poll_interval_secs: 10,
            cursor_staleness_multiple: 3,
            payment_ttl_secs: 3600,
            expiry_batch_size: 500,
            rate_limit_requests_per_sec: 1000,
            db_pool_max_connections: 5,
            db_busy_timeout_ms: 5000,
            cors_allowed_origins: vec![],
            listener_mode: ListenerMode::Poll,
            webhook_allow_private_targets: false,
            admin_provisioning_secret: String::new(),
            request_timeout_secs: 30,
            stream_idle_timeout_secs: 30,
            trusted_proxy_cidrs: vec![],
            max_payment_amount: Default::default(),
            min_payment_amount: Default::default(),
            max_body_bytes: 256 * 1024,
            rate_limiter_max_keys: 10_000,
            rate_limiter_idle_ttl_secs: 60,
            pagination_default_limit: 20,
            pagination_max_limit: 100,
            shutdown_grace_secs: 30,
            horizon_page_limit: 200,
            db_prune_batch_size: 500,
            retention_max_rows_per_cycle: 50_000,
            horizon_timeout_secs: 10,
            sqlite_wal_autocheckpoint: 1000,
            sqlite_journal_size_limit: 67_108_864,
            sqlite_cache_size: -2000,
            require_gateway_account: false,
        }
    }

    /// The exact `AppState` construction `router()` expects, backed by an
    /// in-memory SQLite pool so routing — and the header layers — run for real.
    async fn header_test_state(network: &str) -> Arc<AppState> {
        let cfg = header_test_config(network);
        let pool = SqlitePoolOptions::new()
            .connect_with(
                SqliteConnectOptions::from_str(&cfg.database_url)
                    .unwrap()
                    .create_if_missing(true),
            )
            .await
            .unwrap();
        crate::db::migrate(&pool).await.unwrap();
        Arc::new(AppState {
            pool,
            config: cfg,
            http: reqwest::Client::new(),
            webhook_http: reqwest::Client::new(),
            webhook_metrics: crate::metrics::WebhookMetrics::new(),
            auth_metrics: crate::metrics::AuthMetrics::new(),
            horizon_metrics: crate::metrics::HorizonMetrics::new(),
            trustline_metrics: crate::metrics::TrustlineMetrics::new(),
            http_metrics: crate::metrics::HttpMetrics::new(),
            payment_metrics: crate::metrics::PaymentMetrics::new(),
            task_health: crate::TaskHealth::new(),
        })
    }

    /// Representatives of the three acceptance criteria:
    /// 1. nosniff + Referrer-Policy + Cache-Control on an API response;
    /// 2. HSTS emitted only on public-network deployments;
    /// 3. the dashboard's stricter CSP preserved, not overwritten.
    #[tokio::test]
    async fn api_responses_carry_baseline_security_headers() {
        let state = header_test_state("testnet").await;
        let server = TestServer::new(router(state)).unwrap();

        // A representative API response: the 404 envelope is generated by the
        // router's own fallback, so it is a pure API response with no handler
        // that could set these headers itself.
        let res = server.get("/payments/does-not-exist").await;
        res.assert_status(StatusCode::NOT_FOUND);
        assert_eq!(res.header("x-content-type-options"), "nosniff");
        assert_eq!(res.header("referrer-policy"), "no-referrer");
        assert_eq!(res.header("cache-control"), "no-store");
    }

    #[tokio::test]
    async fn hsts_is_emitted_only_on_public_network() {
        let public = TestServer::new(router(header_test_state("public").await)).unwrap();
        let testnet = TestServer::new(router(header_test_state("testnet").await)).unwrap();

        let public_ok = public.get("/health").await;
        public_ok.assert_status_ok();
        assert!(public_ok
            .header("strict-transport-security")
            .to_str()
            .unwrap_or("")
            .contains("max-age="));

        let testnet_ok = testnet.get("/health").await;
        testnet_ok.assert_status_ok();
        assert!(!testnet_ok.contains_header("strict-transport-security"));
    }

    #[tokio::test]
    async fn dashboard_csp_survives_the_global_header_layers() {
        let state = header_test_state("testnet").await;
        let server = TestServer::new(router(state)).unwrap();

        let res = server.get("/dashboard").await;
        res.assert_status_ok();
        // The dashboard's own, stricter CSP must not be clobbered.
        assert_eq!(res.header("content-security-policy"), DASHBOARD_CSP);
        // And it still gets the baseline headers like everything else.
        assert_eq!(res.header("x-content-type-options"), "nosniff");
    }
}
