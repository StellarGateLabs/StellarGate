use stellargate::{api, config::Config, db, AppState};
use anyhow::Result;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::str::FromStr;
use std::sync::Arc;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    dotenvy::dotenv().ok();

    let cfg = Config::from_env()?;

    let pool = SqlitePoolOptions::new()
        .connect_with(
            SqliteConnectOptions::from_str(&cfg.database_url)?.create_if_missing(true),
        )
        .await?;

    db::migrate(&pool).await?;

    let state = Arc::new(AppState { pool, config: cfg.clone() });
    let addr = format!("0.0.0.0:{}", cfg.port);
    info!("StellarGate API listening on {addr}");

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, api::router(state)).await?;

    Ok(())
}
