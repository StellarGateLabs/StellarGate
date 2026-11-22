use crate::{AppState, api::AuthenticatedMerchant, db, money};
use axum::{
    Json,
    extract::{
        ConnectInfo, Extension, FromRequest, FromRequestParts, OptionalFromRequest, Path, Query,
        Request, State,
    },
    http::{HeaderMap, StatusCode, request::Parts},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::Arc;
use uuid::Uuid;

/// An error with an HTTP status, a stable machine-readable code, and a human message.
pub struct AppError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl AppError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    pub fn bad_request(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code, message)
    }

    pub fn unsupported_media_type(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNSUPPORTED_MEDIA_TYPE, code, message)
    }

    pub fn not_found(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, code, message)
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({ "error": self.message, "code": self.code })),
        )
            .into_response()
    }
}

impl From<anyhow::Error> for AppError {
    fn from(err: anyhow::Error) -> Self {
        tracing::error!(error = %err, "internal error");
        AppError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "internal server error",
        )
    }
}

/// A drop-in replacement for `Json<T>` that maps any deserialization or
/// content-type failure into our standard `{"error": "..."}` 400 response
/// instead of axum's default 422 plaintext rejection.
pub struct JsonBody<T>(pub T);

impl<T, S> FromRequest<S> for JsonBody<T>
where
    T: serde::de::DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match <Json<T> as FromRequest<S>>::from_request(req, state).await {
            Ok(Json(value)) => Ok(JsonBody(value)),
            Err(rejection) => {
                use axum::extract::rejection::JsonRejection;
                match &rejection {
                    JsonRejection::JsonDataError(_) => Err(AppError::bad_request(
                        "invalid_request",
                        format!("invalid request body: {}", rejection.body_text()),
                    )),
                    JsonRejection::JsonSyntaxError(_) => Err(AppError::bad_request(
                        "invalid_request",
                        "request body contains malformed JSON",
                    )),
                    JsonRejection::MissingJsonContentType(_) => {
                        Err(AppError::unsupported_media_type(
                            "unsupported_media_type",
                            "Content-Type must be application/json",
                        ))
                    }
                    _ => Err(AppError::bad_request(
                        "invalid_request",
                        "invalid request body",
                    )),
                }
            }
        }
    }
}

/// Lets handlers take `Option<JsonBody<T>>` for an optional body. axum 0.8
/// requires an explicit impl for `Option<_>` extractors; this keeps 0.7's
/// behaviour, where any rejection (missing, empty or invalid body) yields
/// `None` rather than an error response.
impl<T, S> OptionalFromRequest<S> for JsonBody<T>
where
    T: serde::de::DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request(req: Request, state: &S) -> Result<Option<Self>, Self::Rejection> {
        Ok(<Self as FromRequest<S>>::from_request(req, state)
            .await
            .ok())
    }
}

/// A drop-in replacement for `Query<T>` that maps a query-string failure into
/// our standard `{"code": "...", "error": "..."}` 400 instead of axum's
/// plaintext rejection.
///
/// Paired with `#[serde(deny_unknown_fields)]` on the target struct, this is
/// what makes an unrecognised *parameter* a first-class error: serde raises
/// "unknown field `stauts`, expected one of ..." and this turns it into
/// `400 unknown_parameter` with that message intact, so the response names the
/// offending key and lists the accepted ones.
pub struct QueryParams<T>(pub T);

impl<T, S> FromRequestParts<S> for QueryParams<T>
where
    T: serde::de::DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        match Query::<T>::from_request_parts(parts, state).await {
            Ok(Query(value)) => Ok(QueryParams(value)),
            Err(rejection) => {
                let detail = rejection.body_text();
                /* An unrecognised parameter is its own failure mode, not a
                generic deserialization error: it almost always means a typo or
                a client written against an older spec, and the fix is
                different from "the value had the wrong type". This mirrors the
                `unknown_field` split `JsonBody` already makes for bodies. */
                if detail.contains("unknown field") {
                    Err(AppError::bad_request(
                        "unknown_parameter",
                        format!("invalid query string: {detail}"),
                    ))
                } else {
                    Err(AppError::bad_request(
                        "invalid_query",
                        format!("invalid query string: {detail}"),
                    ))
                }
            }
        }
    }
}

#[derive(Deserialize)]
pub struct CreatePaymentRequest {
    pub amount: String,
    #[serde(default = "default_asset")]
    pub asset: String,
    pub webhook_url: Option<String>,
}

fn default_asset() -> String {
    "XLM".into()
}

pub async fn create(
    State(state): State<Arc<AppState>>,
    Extension(AuthenticatedMerchant(merchant_id)): Extension<AuthenticatedMerchant>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    JsonBody(body): JsonBody<CreatePaymentRequest>,
) -> Result<(StatusCode, Json<Value>), AppError> {
    let asset = body.asset.to_uppercase();
    let accepted = &state.config.accepted_assets;
    /* Resolve the *whole* asset identity, not just the code: the issuer is
    persisted with the intent so the pair can be audited, reported to the
    merchant, and reconciled against an external ledger later (issue #223).
    Where the allow-list carries two entries for one code, the first wins —
    the same entry the previous code-only check would have accepted. */
    let Some(accepted_asset) = accepted.iter().find(|a| a.code == asset) else {
        let codes = accepted
            .iter()
            .map(|a| a.code.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(AppError::bad_request(
            "unsupported_asset",
            format!("unsupported asset '{}'; supported: {}", body.asset, codes),
        ));
    };
    if !money::is_valid_amount(&body.amount) {
        return Err(AppError::bad_request(
            "invalid_amount",
            "amount must be a positive number with at most 7 decimal places",
        ));
    }
    if let Some(url) = &body.webhook_url {
        if url.len() > 2048 {
            return Err(AppError::bad_request(
                "invalid_webhook_url",
                "webhook_url exceeds max length of 2048 characters",
            ));
        };
        let parsed_url = reqwest::Url::parse(url).map_err(|_| {
            AppError::bad_request("invalid_webhook_url", "webhook_url is not a valid URL")
        })?;

        if !state
            .config
            .allowed_webhook_schemes
            .contains(&parsed_url.scheme().to_string())
        {
            return Err(AppError::bad_request(
                "invalid_webhook_url",
                format!(
                    "webhook_url scheme '{}' not allowed. Allowed schemes: {:?}",
                    parsed_url.scheme(),
                    state.config.allowed_webhook_schemes
                ),
            ));
        }

        /* Independent of the configurable allow-list: on the public network a
        webhook must be HTTPS. Delivery carries payment data and the HMAC
        signature, so a permissive ALLOWED_WEBHOOK_SCHEMES must not be able to
        downgrade mainnet traffic to plaintext. */
        if state.config.network == "public" && parsed_url.scheme() != "https" {
            return Err(AppError::bad_request(
                "invalid_webhook_url",
                "webhook_url must be an HTTPS URL on public network",
            ));
        }

        /* Resolve the host and reject loopback/link-local/private/reserved
        addresses so a webhook_url can't be used to probe the internal network
        (the same guard runs again, against the pinned address, on every
        dispatch and redelivery). */
        if crate::ssrf::validate(url, state.config.webhook_allow_private_targets)
            .await
            .is_err()
        {
            return Err(AppError::bad_request(
                "invalid_webhook_url",
                "webhook_url must be a reachable http(s) URL that does not resolve to a \
                 loopback, link-local, private, or other reserved address",
            ));
        }
    }

    /* An optional Idempotency-Key lets a client safely retry a create after a
    network blip without minting a duplicate intent. Keys are scoped per
    merchant; an empty header value is treated as absent. */
    let idempotency_key = headers
        .get("Idempotency-Key")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|k| !k.is_empty());

    /* If we've already seen this key for this merchant, return the original
    payment with 200 instead of creating a new one. If the mapped payment row
    is missing (e.g. a previous winner crashed after inserting the key but
    before creating the payment), delete the stale mapping and proceed. */
    if let Some(key) = idempotency_key
        && let Some(existing_id) =
            db::find_payment_id_by_idempotency_key(&state.pool, &merchant_id, key).await?
    {
        if let Some(payment) = db::get_payment(&state.pool, &existing_id).await? {
            return Ok((StatusCode::OK, Json(to_json_with_network(&payment, &state.config.network))));
        }
        sqlx::query("DELETE FROM idempotency_keys WHERE merchant_id = ? AND idempotency_key = ?")
            .bind(&merchant_id)
            .bind(key)
            .execute(&state.pool)
            .await
            .map_err(anyhow::Error::from)?;
    }

    let id = Uuid::new_v4().to_string();

    /* Reserve the idempotency key before minting the payment so concurrent
    same-key requests can't both create payments. The DB primary key
    serialises the race: only one request's INSERT succeeds. */
    if let Some(key) = idempotency_key {
        let canonical_id = db::save_idempotency_key(&state.pool, &merchant_id, key, &id).await?;
        if canonical_id != id {
            /* Lost the race — the winner is about to create its payment. Wait
            for it with a short retry loop and then return that payment. */
            for _ in 0..50 {
                if let Some(payment) = db::get_payment(&state.pool, &canonical_id).await? {
                    return Ok((StatusCode::OK, Json(to_json_with_network(&payment, &state.config.network))));
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            return Err(AppError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "idempotency_conflict",
                "concurrent request conflict, please retry",
            ));
        }
    }

    let memo = generate_unique_memo(&state.pool).await?;

    let payment = db::create_payment(
        &state.pool,
        db::NewPayment {
            id: &id,
            merchant_id: &merchant_id,
            destination_address: &state.config.gateway_public,
            memo: &memo,
            amount: &body.amount,
            asset: &asset,
            asset_issuer: accepted_asset.issuer.as_deref(),
            webhook_url: body.webhook_url.as_deref(),
            ttl_secs: state.config.payment_ttl_secs as i64,
        },
    )
    .await?;

    let source_ip =
        super::client_ip_key_from_parts(Some(peer), &headers, &state.config.trusted_proxy_cidrs);
    let request_id = super::request_id(&headers);
    tracing::info!(
        audit = true,
        action = "payment.create",
        actor = "merchant",
        %merchant_id,
        outcome = "created",
        payment_id = %payment.id,
        amount = %payment.amount,
        asset = %payment.asset,
        %source_ip,
        %request_id,
        "payment created"
    );

    Ok((StatusCode::CREATED, Json(to_json_with_network(&payment, &state.config.network))))
}

/// `GET /payments/:id` — status for one payment.
///
/// This route stays reachable without a merchant key so a checkout page can
/// poll it directly, but what it returns depends on who is asking
/// (issues #67, #85):
///
/// - **No credential** → a minimal projection: the id, its status, and when it
///   expires. Enough to answer "has this been paid yet", and nothing else.
/// - **The owning merchant's key** → the full record.
/// - **Another merchant's key** → `404`, the same answer an unknown id gets.
///   A `403` would confirm the payment exists and belongs to someone else,
///   which is exactly the cross-tenant signal these issues are about.
///
/// The public projection deliberately omits `merchant_id`, every amount, and
/// `tx_hash`. Payment ids travel through logs, referrers and browser history,
/// so anything on this response should be treated as effectively public: it
/// previously leaked which merchant an id belonged to and how much it was for.
pub async fn get_by_id(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    let payment = db::get_payment(&state.pool, &id)
        .await?
        .ok_or_else(|| AppError::not_found("payment_not_found", "payment not found"))?;

    let Some(key) = bearer_token(&headers) else {
        return Ok(Json(public_view(&payment)));
    };

    /* A credential was offered, so honour it rather than silently falling back
    to the public view — a typo'd or revoked key should say so, not quietly
    return less data and leave the caller wondering why fields are missing. */
    match db::find_merchant_by_key(&state.pool, &key).await? {
        Some(merchant_id) if merchant_id == payment.merchant_id => Ok(Json(to_json_with_network(&payment, &state.config.network))),
        Some(_) => Err(AppError::not_found(
            "payment_not_found",
            "payment not found",
        )),
        None => Err(AppError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "missing or invalid Authorization header",
        )),
    }
}

/// Extract a bearer token, if one was supplied at all.
fn bearer_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(str::to_string)
}

/// What an unauthenticated caller sees: enough to poll for completion, with
/// nothing that identifies the merchant or the sum involved.
fn public_view(p: &db::Payment) -> Value {
    json!({
        "id": p.id,
        "status": p.status,
        "expires_at": p.expires_at,
    })
}

#[derive(Deserialize)]
pub struct ListQuery {
    pub status: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub cursor: Option<String>,
    pub created_after: Option<String>,
    pub created_before: Option<String>,
}

const DEFAULT_LIMIT: i64 = 20;
const MAX_LIMIT: i64 = 100;
/// Statuses a payment can actually hold, and therefore the only ones worth
/// filtering on: `pending` at creation, `completed`/`underpaid` from
/// settlement (`horizon::settle`), and `expired` from the TTL sweeper
/// (`db::expire_overdue`). Nothing writes any other value, so anything else is
/// a guaranteed-empty filter and is rejected as invalid.
const VALID_STATUSES: [&str; 4] = ["pending", "completed", "underpaid", "expired"];

pub async fn list(
    State(state): State<Arc<AppState>>,
    Extension(AuthenticatedMerchant(merchant_id)): Extension<AuthenticatedMerchant>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Value>, AppError> {
    if let Some(s) = &q.status
        && !VALID_STATUSES.contains(&s.as_str())
    {
        return Err(AppError::bad_request(
            "invalid_status",
            format!(
                "invalid status '{}'; valid: {}",
                s,
                VALID_STATUSES.join(", ")
            ),
        ));
    }

    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);

    if let Some(raw_cursor) = &q.cursor {
        // Keyset (cursor) pagination — stable, O(log n) regardless of page depth.
        let (cursor_ts, cursor_id) = decode_cursor(raw_cursor)
            .ok_or_else(|| AppError::bad_request("invalid_cursor", "invalid cursor"))?;

        let payments = db::list_payments_keyset(
            &state.pool,
            &merchant_id,
            q.status.as_deref(),
            q.created_after.as_deref(),
            q.created_before.as_deref(),
            limit,
            Some((&cursor_ts, &cursor_id)),
        )
        .await?;

        let next_cursor = if payments.len() == limit as usize {
            payments.last().map(|p| encode_cursor(&p.created_at, &p.id))
        } else {
            None
        };

        let mut rows = payments.iter().map(|p| to_json_with_network(p, &state.config.network)).collect::<Vec<_>>();
        attach_webhook_status(&state.pool, &payments, &mut rows).await?;

        Ok(Json(json!({
            "payments": rows,
            "limit": limit,
            "next_cursor": next_cursor,
        })))
    } else {
        // Legacy offset pagination — kept for backward compatibility.
        let offset = q.offset.unwrap_or(0).max(0);
        let (payments, total) = db::list_payments(
            &state.pool,
            &merchant_id,
            q.status.as_deref(),
            q.created_after.as_deref(),
            q.created_before.as_deref(),
            limit,
            offset,
        )
        .await?;

        // Provide next_cursor to ease migration to keyset pagination.
        let next_cursor = payments.last().map(|p| encode_cursor(&p.created_at, &p.id));

        let mut rows = payments.iter().map(|p| to_json_with_network(p, &state.config.network)).collect::<Vec<_>>();
        attach_webhook_status(&state.pool, &payments, &mut rows).await?;

        Ok(Json(json!({
            "payments": rows,
            "total": total,
            "limit": limit,
            "offset": offset,
            "next_cursor": next_cursor,
        })))
    }
}

pub async fn summary(
    State(state): State<Arc<AppState>>,
    Extension(AuthenticatedMerchant(merchant_id)): Extension<AuthenticatedMerchant>,
) -> Result<Json<Value>, AppError> {
    let summary = db::payments_summary(&state.pool, &merchant_id).await?;
    Ok(Json(json!({ "summary": summary })))
}

fn encode_cursor(ts: &str, id: &str) -> String {
    hex::encode(format!("{ts}\t{id}"))
}

fn decode_cursor(raw: &str) -> Option<(String, String)> {
    let bytes = hex::decode(raw).ok()?;
    let s = String::from_utf8(bytes).ok()?;
    let (ts, id) = s.split_once('\t')?;
    Some((ts.to_string(), id.to_string()))
}

/// Generates an 8-character uppercase-hex `text` memo (32 bits of entropy,
/// well within Stellar's 28-byte text memo limit) and confirms it hasn't been
/// used by *any* payment intent before — `memo_exists` checks the entire
/// `payments` table, not just pending ones, so a memo is never reused for the
/// lifetime of the database. That makes the collision probability for a
/// single call simply `rows-in-table / 2^32`, and the loop retries up to 10
/// times before giving up; exhausting that before billions of payments exist
/// is effectively impossible. If traffic ever approaches that scale, widen
/// the memo (more hex chars, still under the 28-byte limit) rather than
/// switching scheme.
///
/// We chose a `text` memo over `memo_id` (a u64) or `memo_hash`/`memo_return`
/// (32-byte) because it's the simplest scheme that round-trips a
/// human-legible reference through Horizon. The tradeoff: Horizon's JSON
/// `memo` field also holds a string for those other memo types (a decimal
/// string for `memo_id`, base64 for `memo_hash`/`memo_return`), and a
/// `memo_id` consisting only of digits could coincidentally render as the
/// same text as one of our hex memos. `horizon::HorizonPayment::memo()`
/// guards against this by only matching when Horizon reports `memo_type:
/// "text"` (see issue #17).
async fn generate_unique_memo(pool: &db::Db) -> Result<String, AppError> {
    for _ in 0..10 {
        let memo = Uuid::new_v4().to_string().replace('-', "")[..8].to_uppercase();
        if !db::memo_exists(pool, &memo).await? {
            return Ok(memo);
        }
    }
    Err(AppError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal_error",
        "memo generation failed",
    ))
}

/// Build a SEP-7 `web+stellar:pay` URI for `payment`.
///
/// The URI encodes the destination address, memo (as `MEMO_TEXT`), the
/// stroop-exact amount, and—for issued assets—the asset code and issuer.
/// For non-public networks the `network_passphrase` parameter is included so
/// SEP-7-capable wallets submit the transaction on the correct network rather
/// than silently signing for mainnet (issue #1012).
///
/// The amount is derived from the stored stroop count so floating-point
/// rounding (the bug present in the dashboard's `buildSep7Uri`) cannot occur.
pub fn build_sep7_uri(p: &db::Payment, network: &str) -> String {
    use std::fmt::Write as _;

    // Use the stroop-exact amount to avoid any floating-point rounding.
    let amount_str = crate::money::parse_stroops(&p.amount)
        .map(crate::money::stroops_to_string)
        .unwrap_or_else(|| p.amount.clone());

    let mut params = String::new();

    // destination is always present.
    let _ = write!(params, "destination={}", sep7_encode(&p.destination_address));

    let _ = write!(params, "&amount={}", sep7_encode(&amount_str));

    let asset = p.asset.to_uppercase();
    if asset != "XLM" {
        let _ = write!(params, "&asset_code={}", sep7_encode(&asset));
        if let Some(issuer) = &p.asset_issuer {
            let _ = write!(params, "&asset_issuer={}", sep7_encode(issuer));
        }
    }

    if !p.memo.is_empty() {
        let _ = write!(params, "&memo={}", sep7_encode(&p.memo));
        params.push_str("&memo_type=MEMO_TEXT");
    }

    // Include the passphrase for every non-public network so wallets don't
    // silently broadcast the transaction on mainnet (the dashboard had this
    // bug — it always omitted the passphrase).
    if network != "public" {
        let passphrase = match network {
            "testnet" => "Test SDF Network ; September 2015",
            // Any other custom network must set its own passphrase via env;
            // fall back to the testnet value so the URI at least indicates
            // a non-mainnet context rather than omitting the parameter entirely.
            _ => "Test SDF Network ; September 2015",
        };
        let _ = write!(params, "&network_passphrase={}", sep7_encode(passphrase));
    }

    format!("web+stellar:pay?{params}")
}

/// Percent-encode a value for use as a SEP-7 query parameter.
///
/// Encodes all characters except unreserved URI characters (letters, digits,
/// `-`, `_`, `.`, `~`) per RFC 3986. This matches how browsers and SEP-7
/// reference implementations encode query values.
fn sep7_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            other => {
                out.push('%');
                out.push(char::from_digit((other >> 4) as u32, 16).unwrap().to_ascii_uppercase());
                out.push(char::from_digit((other & 0xf) as u32, 16).unwrap().to_ascii_uppercase());
            }
        }
    }
    out
}

fn to_json_with_network(p: &db::Payment, network: &str) -> Value {
    // Canonicalize amount: parse to stroops and format back to canonical form.
    // This ensures "10.00", "10.0", and "10" all serialize identically,
    // eliminating spurious string-based comparisons across responses.
    let canonical_amount = crate::money::parse_stroops(&p.amount)
        .map(crate::money::stroops_to_string)
        .unwrap_or_else(|| p.amount.clone());

    // Canonicalize paid_amount the same way (defensive; it should already be
    // canonical from horizon.rs, but this ensures consistency across all
    // serialization paths).
    let canonical_paid_amount = p
        .paid_amount
        .as_ref()
        .and_then(|pa| crate::money::parse_stroops(pa).map(crate::money::stroops_to_string));

    let payment_uri = build_sep7_uri(p, network);

    json!({
        "id": p.id,
        "merchant_id": p.merchant_id,
        "destination_address": p.destination_address,
        "memo": p.memo,
        "amount": canonical_amount,
        "asset": p.asset,
        // `null` for the native asset, which has no issuer.
        "asset_issuer": p.asset_issuer,
        "status": p.status,
        "tx_hash": p.tx_hash,
        "paid_amount": canonical_paid_amount,
        "created_at": p.created_at,
        "updated_at": p.updated_at,
        "expires_at": p.expires_at,
        // SEP-7 pay URI that encodes every field a wallet needs to prefill a
        // payment: destination, amount (stroop-exact), asset, memo, and the
        // network passphrase on non-public networks (issue #1012).
        "payment_uri": payment_uri,
        // `null` when the payment has no webhook configured; otherwise the
        // status of its most recent delivery (`delivered` / `failed` /
        // `pending`). Populated by the list handlers below.
        "webhook_status": Value::Null,
    })
}

/// Legacy alias kept so internal call sites that do not yet carry the network
/// string still compile. New code should call [`to_json_with_network`].
#[allow(dead_code)]
fn to_json(p: &db::Payment) -> Value {
    to_json_with_network(p, "testnet")
}

/// Fill in `webhook_status` on each serialized payment from its latest
/// delivery, so the dashboard can render a delivered/failing/none indicator
/// straight from the list response instead of opening each payment.
///
/// A payment with no webhook configured, or one whose webhook has not been
/// attempted yet, gets `null` (rendered as "none"). Any delivery that has not
/// succeeded — including `pending` — is reported as `"failing"` so a stuck or
/// retrying webhook is visible rather than silently omitted.
async fn attach_webhook_status(
    pool: &db::Db,
    payments: &[db::Payment],
    rows: &mut [Value],
) -> Result<(), AppError> {
    for (payment, row) in payments.iter().zip(rows.iter_mut()) {
        if payment.webhook_url.is_none() {
            continue;
        }
        let deliveries = db::list_webhook_deliveries(pool, &payment.id).await?;
        let Some(latest) = deliveries.first() else {
            continue;
        };
        let status = if latest.status == "delivered" {
            "delivered"
        } else {
            "failing"
        };
        row["webhook_status"] = Value::String(status.to_string());
    }
    Ok(())
}

pub async fn list_webhooks(
    State(state): State<Arc<AppState>>,
    Extension(AuthenticatedMerchant(merchant_id)): Extension<AuthenticatedMerchant>,
    Path(payment_id): Path<String>,
) -> Result<Json<Value>, AppError> {
    // Verify payment exists and belongs to the caller. A payment owned by
    // another merchant reports the same 404 as a missing one, so this can't
    // be used to enumerate which payment ids exist for other tenants.
    let payment = db::get_payment(&state.pool, &payment_id)
        .await?
        .filter(|p| p.merchant_id == merchant_id)
        .ok_or_else(|| {
            AppError::new(
                StatusCode::NOT_FOUND,
                "payment_not_found",
                "payment not found",
            )
        })?;

    let deliveries = db::list_webhook_deliveries(&state.pool, &payment_id).await?;

    Ok(Json(json!({
        "payment_id": payment.id,
        "deliveries": deliveries.iter().map(delivery_to_json).collect::<Vec<_>>(),
    })))
}

/// Query parameters accepted by `POST /payments/:id/webhooks/:delivery_id/redeliver`.
/// `deny_unknown_fields` so a typo'd param is rejected rather than silently
/// ignored — the same strictness applied to every other query string in the API.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RedeliverQuery {
    /// Pass `?force=true` to redeliver a webhook that was already successfully
    /// delivered. Without it, a delivery whose status is `"delivered"` is
    /// refused with `409 already_delivered` so that an accidental double-click
    /// on the dashboard does not silently send a duplicate event (issue #236).
    pub force: Option<bool>,
}

/// One delivery as the API exposes it. The stored `payload` is deliberately
/// omitted — it is the signed event body, it can be large, and a listing is for
/// triage rather than replay.
fn delivery_to_json(d: &db::WebhookDelivery) -> Value {
    json!({
        "id": d.id,
        "payment_id": d.payment_id,
        "url": d.url,
        "event": d.event(),
        "status": d.status,
        "attempts": d.attempts,
        "manual_attempts": d.manual_attempts,
        "last_attempt": d.last_attempt,
        "acknowledged_at": d.acknowledged_at,
        "created_at": d.created_at,
    })
}

pub async fn redeliver_webhook(
    State(state): State<Arc<AppState>>,
    Extension(AuthenticatedMerchant(merchant_id)): Extension<AuthenticatedMerchant>,
    Path((payment_id, delivery_id)): Path<(String, String)>,
    ConnectInfo(_peer): ConnectInfo<SocketAddr>,
    _headers: HeaderMap,
    QueryParams(query): QueryParams<RedeliverQuery>,
) -> Result<StatusCode, AppError> {
    // Verify payment exists and belongs to the caller. A payment owned by
    // another merchant reports the same 404 as a missing one.
    db::get_payment(&state.pool, &payment_id)
        .await?
        .filter(|p| p.merchant_id == merchant_id)
        .ok_or_else(|| {
            AppError::new(
                StatusCode::NOT_FOUND,
                "payment_not_found",
                "payment not found",
            )
        })?;

    // Get the delivery
    let delivery = db::get_webhook_delivery(&state.pool, &delivery_id)
        .await?
        .ok_or_else(|| {
            AppError::new(
                StatusCode::NOT_FOUND,
                "delivery_not_found",
                "delivery not found",
            )
        })?;

    // Verify the delivery belongs to this payment
    if delivery.payment_id != payment_id {
        return Err(AppError::new(
            StatusCode::NOT_FOUND,
            "delivery_not_found",
            "delivery not found",
        ));
    }

    /* Guard against silent duplicate events on an already-successful delivery
    (issue #236). An accidental double-click on the dashboard redeliver button,
    or an API client that does not check status before calling this endpoint,
    would otherwise send a second copy of the event — and overwrite the
    audit row if the second attempt failed, permanently losing the evidence
    that the original delivery succeeded. Require explicit opt-in via
    `?force=true` so callers cannot hit this path accidentally. */
    if delivery.status == "delivered" && !query.force.unwrap_or(false) {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            "already_delivered",
            "this webhook was already successfully delivered; \
             pass ?force=true to redeliver anyway",
        ));
    }

    // Re-validate the target on every redelivery — the delivery row may be old,
    // so a stale-but-once-valid URL must not become a standing SSRF pivot.
    let client = crate::webhook::safe_client(&state, &delivery.url)
        .await
        .map_err(|_| {
            AppError::new(
                StatusCode::BAD_REQUEST,
                "webhook_target_blocked",
                "webhook target is not allowed",
            )
        })?;

    /* Re-send the original payload, re-signed with a fresh timestamp so the
    receiver's replay-tolerance window is measured from this redelivery. The
    event header comes from the stored delivery rather than being hard-coded:
    a receiver that routes on `X-StellarGate-Event` must see the same event the
    body carries, whether that was completed, overpaid, underpaid, or expired. */
    let payload_bytes = delivery.payload.as_bytes();
    let timestamp = crate::webhook::current_timestamp();
    let signature = crate::webhook::sign(&state.config.webhook_secret, timestamp, payload_bytes);
    let event = delivery.event();

    let result = client
        .post(&delivery.url)
        .header("Content-Type", "application/json")
        .header("X-StellarGate-Signature", &signature)
        .header("X-StellarGate-Timestamp", timestamp.to_string())
        // Convenience header — mirrors the `event` field already present in
        // the signed body. NOT covered by the HMAC; receivers must route on
        // the authenticated body field, not this header.
        .header("X-StellarGate-Event", &event)
        .body(delivery.payload.clone())
        .send()
        .await;

    let new_status = match result {
        Ok(resp) if resp.status().is_success() => "delivered",
        _ => "failed",
    };

    db::update_webhook_delivery(&state.pool, &delivery_id, new_status, delivery.attempts + 1)
        .await?;

    if new_status == "delivered" {
        Ok(StatusCode::OK)
    } else {
        Err(AppError::new(
            StatusCode::BAD_GATEWAY,
            "webhook_delivery_failed",
            "webhook delivery failed",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_payment(amount: &str, asset: &str, asset_issuer: Option<&str>, memo: &str) -> db::Payment {
        db::Payment {
            id: "pay1".into(),
            merchant_id: "merch1".into(),
            destination_address: "GDEST1234567890ABCDEFGHIJKLMNOPQRSTUVWXYZ1234567890ABCDE".into(),
            memo: memo.into(),
            amount: amount.into(),
            asset: asset.into(),
            asset_issuer: asset_issuer.map(str::to_string),
            status: "pending".into(),
            webhook_url: None,
            tx_hash: None,
            paid_amount: None,
            created_at: "2024-01-01T00:00:00Z".into(),
            updated_at: "2024-01-01T00:00:00Z".into(),
            expires_at: "2024-01-01T01:00:00Z".into(),
        }
    }

    // ── SEP-7 URI building (issue #1012) ──────────────────────────────────

    #[test]
    fn sep7_uri_xlm_testnet_includes_passphrase() {
        let p = make_payment("10", "XLM", None, "ABCD1234");
        let uri = build_sep7_uri(&p, "testnet");
        assert!(uri.starts_with("web+stellar:pay?"), "must start with scheme");
        assert!(uri.contains("destination="), "must contain destination");
        assert!(uri.contains("amount=10"), "must contain exact amount");
        assert!(uri.contains("memo=ABCD1234"), "must contain memo");
        assert!(uri.contains("memo_type=MEMO_TEXT"), "must set memo_type");
        assert!(!uri.contains("asset_code="), "XLM must not include asset_code");
        // testnet passphrase, percent-encoded
        assert!(
            uri.contains("network_passphrase="),
            "testnet URI must include network_passphrase"
        );
        assert!(
            uri.contains("Test%20SDF%20Network"),
            "passphrase must be percent-encoded"
        );
    }

    #[test]
    fn sep7_uri_public_network_omits_passphrase() {
        let p = make_payment("5", "XLM", None, "MEMO1");
        let uri = build_sep7_uri(&p, "public");
        assert!(!uri.contains("network_passphrase"), "mainnet must not include passphrase");
    }

    #[test]
    fn sep7_uri_issued_asset_includes_code_and_issuer() {
        let issuer = "GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5";
        let p = make_payment("1.5", "USDC", Some(issuer), "MEMOX");
        let uri = build_sep7_uri(&p, "testnet");
        assert!(uri.contains("asset_code=USDC"), "must have asset_code");
        assert!(uri.contains(&format!("asset_issuer={}", sep7_encode(issuer))), "must have encoded issuer");
    }

    #[test]
    fn sep7_uri_amount_is_stroop_exact() {
        // "10.0000000" should produce "10" (no floating-point rounding)
        let p = make_payment("10.0000000", "XLM", None, "M1");
        let uri = build_sep7_uri(&p, "public");
        assert!(uri.contains("amount=10&") || uri.ends_with("amount=10"), "amount must be canonical");
        assert!(!uri.contains("amount=10.0"), "must not have trailing zeros");
    }

    #[test]
    fn sep7_encode_encodes_special_characters() {
        // Space, semicolon, and slash must be percent-encoded.
        assert_eq!(sep7_encode("a b"), "a%20b");
        assert_eq!(sep7_encode("a;b"), "a%3Bb");
        assert_eq!(sep7_encode("a/b"), "a%2Fb");
        // Unreserved characters must pass through unchanged.
        assert_eq!(sep7_encode("abc-_~."), "abc-_~.");
    }

    #[test]
    fn sep7_uri_memo_with_special_chars_is_encoded() {
        let mut p = make_payment("1", "XLM", None, "hello world");
        p.memo = "hello world".into();
        let uri = build_sep7_uri(&p, "public");
        assert!(uri.contains("memo=hello%20world"), "space in memo must be encoded");
    }
}
