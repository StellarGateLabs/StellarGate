//! Status CHECK constraints on `payments` and `webhook_deliveries` (#1049).
//!
//! Written, not run as part of this change.

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::str::FromStr;
use stellargate::db;

async fn memory_pool() -> sqlx::SqlitePool {
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(SqliteConnectOptions::from_str("sqlite::memory:").unwrap())
        .await
        .unwrap()
}

/// Inserts a payment with the given status. Only the columns without defaults are set.
async fn insert_payment(pool: &sqlx::SqlitePool, id: &str, status: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO payments (id, destination_address, memo, amount, status)
         VALUES (?, 'GDEST', ?, '1', ?)",
    )
    .bind(id)
    .bind(format!("memo-{id}"))
    .bind(status)
    .execute(pool)
    .await
    .map(|_| ())
}

#[tokio::test]
async fn fresh_database_rejects_an_unknown_payment_status() {
    let pool = memory_pool().await;
    db::migrate(&pool).await.unwrap();

    assert!(insert_payment(&pool, "p1", "complete").await.is_err());
    assert!(insert_payment(&pool, "p2", "completed").await.is_ok());
}

#[tokio::test]
async fn startup_check_reports_rows_outside_the_allowed_set() {
    let pool = memory_pool().await;
    // A legacy table: created without the CHECK, holding a typo'd status.
    sqlx::query(
        "CREATE TABLE payments (
            id TEXT PRIMARY KEY,
            merchant_id TEXT NOT NULL DEFAULT 'anonymous',
            destination_address TEXT NOT NULL,
            memo TEXT NOT NULL UNIQUE,
            amount TEXT NOT NULL,
            asset TEXT NOT NULL DEFAULT 'XLM',
            asset_issuer TEXT,
            status TEXT NOT NULL DEFAULT 'pending',
            webhook_url TEXT,
            tx_hash TEXT,
            paid_amount TEXT,
            created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
            updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
            expires_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now','+1 hour'))
        )",
    )
    .execute(&pool)
    .await
    .unwrap();
    insert_payment(&pool, "p1", "complete").await.unwrap();

    let out_of_set = db::out_of_set_statuses(&pool).await.unwrap();
    assert_eq!(out_of_set, vec![("payments".to_string(), "complete".to_string(), 1)]);

    // The constraint is withheld while the bad row exists, so the typo stays visible.
    db::enforce_status_checks(&pool).await.unwrap();
    assert!(insert_payment(&pool, "p2", "pending").await.is_ok());
}

#[tokio::test]
async fn legacy_table_gains_the_constraint_when_every_row_is_valid() {
    let pool = memory_pool().await;
    sqlx::query(
        "CREATE TABLE payments (
            id TEXT PRIMARY KEY,
            merchant_id TEXT NOT NULL DEFAULT 'anonymous',
            destination_address TEXT NOT NULL,
            memo TEXT NOT NULL UNIQUE,
            amount TEXT NOT NULL,
            asset TEXT NOT NULL DEFAULT 'XLM',
            asset_issuer TEXT,
            status TEXT NOT NULL DEFAULT 'pending',
            webhook_url TEXT,
            tx_hash TEXT,
            paid_amount TEXT,
            created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
            updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
            expires_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now','+1 hour'))
        )",
    )
    .execute(&pool)
    .await
    .unwrap();
    insert_payment(&pool, "p1", "pending").await.unwrap();

    db::enforce_status_checks(&pool).await.unwrap();

    assert!(insert_payment(&pool, "p2", "complete").await.is_err());
    // The existing row survives the rebuild.
    let kept: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM payments WHERE id = 'p1'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(kept, 1);
}
