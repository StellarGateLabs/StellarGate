use crate::{db, AppState};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use uuid::Uuid;

pub struct AppError(StatusCode, &'static str);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl From<anyhow::Error> for AppError {
    fn from(_: anyhow::Error) -> Self {
        AppError(StatusCode::INTERNAL_SERVER_ERROR, "internal server error")
    }
}

#[derive(Deserialize)]
pub struct CreatePaymentRequest {
    pub amount: String,
    #[serde(default = "default_asset")]
    pub asset: String,
    pub merchant_id: Option<String>,
    pub webhook_url: Option<String>,
}

fn default_asset() -> String { "XLM".into() }

pub async fn create(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreatePaymentRequest>,
) -> Result<(StatusCode, Json<Value>), AppError> {
    if !["XLM", "USDC"].contains(&body.asset.as_str()) {
        return Err(AppError(StatusCode::BAD_REQUEST, "unsupported asset"));
    }
    if body.amount.parse::<f64>().unwrap_or(0.0) <= 0.0 {
        return Err(AppError(StatusCode::BAD_REQUEST, "invalid amount"));
    }

    let memo = generate_unique_memo(&state.pool).await?;
    let id = Uuid::new_v4().to_string();

    let payment = db::create_payment(
        &state.pool,
        &id,
        body.merchant_id.as_deref().unwrap_or("anonymous"),
        &state.config.gateway_public,
        &memo,
        &body.amount,
        &body.asset,
        body.webhook_url.as_deref(),
    )
    .await?;

    Ok((StatusCode::CREATED, Json(to_json(&payment))))
}

pub async fn get_by_id(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    match db::get_payment(&state.pool, &id).await? {
        Some(p) => Ok(Json(to_json(&p))),
        None => Err(AppError(StatusCode::NOT_FOUND, "payment not found")),
    }
}

async fn generate_unique_memo(pool: &db::Db) -> Result<String, AppError> {
    for _ in 0..10 {
        let memo = Uuid::new_v4().to_string().replace('-', "")[..8].to_uppercase();
        if !db::memo_exists(pool, &memo).await? {
            return Ok(memo);
        }
    }
    Err(AppError(StatusCode::INTERNAL_SERVER_ERROR, "memo generation failed"))
}

fn to_json(p: &db::Payment) -> Value {
    json!({
        "id": p.id,
        "destination_address": p.destination_address,
        "memo": p.memo,
        "amount": p.amount,
        "asset": p.asset,
        "status": p.status,
        "created_at": p.created_at,
    })
}
