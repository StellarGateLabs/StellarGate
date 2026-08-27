use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::str::FromStr;
use stellargate::db;

#[tokio::main]
async fn main() {
    let pool = SqlitePoolOptions::new()
        .connect_with(SqliteConnectOptions::from_str("sqlite::memory:").unwrap())
        .await
        .unwrap();
    db::migrate(&pool).await.unwrap();
    let rows: Vec<(String,)> =
        sqlx::query_as("SELECT sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY type, name")
            .fetch_all(&pool)
            .await
            .unwrap();
    for (sql,) in rows {
        println!("{sql}");
        println!(";");
    }
}
