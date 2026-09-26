//! Read-only audit: list payment intents under-credited by the multi-operation
//! bug (issue #617). Reads the same environment as the server and prints a JSON
//! report to stdout. It never writes to the database.

use anyhow::Result;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::str::FromStr;
use std::time::Duration;
use stellargate::{audit, config::Config};

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let cfg = Config::from_env()?;
    let opts = SqliteConnectOptions::from_str(&cfg.database_url)?.read_only(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await?;
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(concat!("StellarGate-audit/", env!("CARGO_PKG_VERSION")))
        .build()?;

    let found = audit::find_under_credited(&pool, &http, &cfg.horizon_url).await?;
    println!("{}", serde_json::to_string_pretty(&found)?);
    eprintln!("{} under-credited transaction(s) found", found.len());
    Ok(())
}
