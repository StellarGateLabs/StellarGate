use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::str::FromStr;
use stellargate::db;

#[tokio::test]
async fn migration_rolls_back_schema_changes_when_backfill_fails() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::from_str("sqlite::memory:")
                .unwrap()
                .create_if_missing(true),
        )
        .await
        .unwrap();

    sqlx::query(
        "CREATE TABLE payments (
            id TEXT PRIMARY KEY,
            merchant_id TEXT NOT NULL DEFAULT 'anonymous',
            destination_address TEXT NOT NULL,
            memo TEXT NOT NULL UNIQUE,
            amount TEXT NOT NULL,
            asset TEXT NOT NULL DEFAULT 'XLM',
            status TEXT NOT NULL DEFAULT 'pending',
            webhook_url TEXT,
            tx_hash TEXT,
            paid_amount TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        )",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO payments
            (id, destination_address, memo, amount, created_at, updated_at)
         VALUES ('payment-1', 'destination', 'memo-1', '10',
                 '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER fail_expiry_backfill
         BEFORE UPDATE ON payments
         WHEN NEW.created_at = OLD.created_at
         BEGIN
             SELECT RAISE(ABORT, 'injected migration failure');
         END",
    )
    .execute(&pool)
    .await
    .unwrap();

    assert!(db::migrate(&pool).await.is_err());

    let expires_at_columns: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pragma_table_info('payments') WHERE name = 'expires_at'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(expires_at_columns, 0);

    let committed_tables: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type = 'table' AND name IN
             ('webhook_deliveries', 'kv_state', 'merchants',
              'idempotency_keys', 'processed_transactions')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(committed_tables, 0);
}

/// Regression for issue #269: the offset path must order by (created_at DESC,
/// id DESC) so a full page-walk returns every row exactly once even when many
/// rows share the same whole-second timestamp.
///
/// Creates more payments than fit in one page, all with the same created_at,
/// walks pages until exhausted, and asserts every id appears exactly once.
#[tokio::test]
async fn offset_pagination_returns_each_row_exactly_once_within_one_second() {
    let pool = SqlitePoolOptions::new()
        .min_connections(1)
        .connect_with(
            SqliteConnectOptions::from_str("sqlite::memory:")
                .unwrap()
                .create_if_missing(true),
        )
        .await
        .unwrap();

    db::migrate(&pool).await.unwrap();

    let (raw_key, prefix) = db::generate_api_key();
    db::create_merchant(&pool, "m1", &raw_key, &prefix)
        .await
        .unwrap();

    // Insert 25 payments all stamped at the same second so every page boundary
    // falls inside a tie group — the most adversarial case for a missing tiebreaker.
    let ts = "2026-01-01T00:00:00Z";
    for i in 0..25u32 {
        let id = format!("pay-{i:03}");
        let memo = format!("MEMO-{i:03}");
        sqlx::query(
            "INSERT INTO payments
                (id, merchant_id, destination_address, memo, amount, asset, status,
                 created_at, updated_at, expires_at)
             VALUES (?, 'm1', 'GDEST', ?, '1', 'XLM', 'pending', ?, ?, ?)",
        )
        .bind(&id)
        .bind(&memo)
        .bind(ts)
        .bind(ts)
        .bind("2026-01-01T01:00:00Z")
        .execute(&pool)
        .await
        .unwrap();
    }

    // Walk all pages with limit=7 (not a divisor of 25 to catch the last partial page).
    let page_size = 7i64;
    let mut seen = std::collections::HashSet::new();
    let mut offset = 0i64;
    loop {
        let (page, _total) = db::list_payments(&pool, "m1", None, page_size, offset)
            .await
            .unwrap();
        if page.is_empty() {
            break;
        }
        for p in &page {
            assert!(
                seen.insert(p.id.clone()),
                "payment {} appeared more than once during offset walk (offset={})",
                p.id,
                offset
            );
        }
        offset += page.len() as i64;
    }

    assert_eq!(
        seen.len(),
        25,
        "offset walk must return all 25 payments exactly once"
    );
}
