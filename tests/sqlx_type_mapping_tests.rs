//! Pins the SQLite → Rust decode mappings `src/db.rs` relies on (issue #644).
//!
//! SQLite is dynamically typed, so every `query_scalar` / `query_as` /
//! `Row::get` in the service leans on sqlx's per-type `compatible()` table to
//! accept (or reject) what a column or expression actually returns. That table
//! has shifted between sqlx majors before, and a mismatch only surfaces at
//! runtime as a `ColumnDecode` error on the affected query.
//!
//! Audit result for 0.8.6 → 0.9.0: the `compatible()` table for every type
//! used here (`i64`, `bool`, `String`, `Option<_>` of those) is unchanged;
//! only the internal decode plumbing moved (`int64()`/`text()` became
//! fallible, `text()` split into `text_borrowed`/`text_owned`). So nothing
//! here tests a *changed* mapping — each test pins one shape of call site in
//! `db.rs` so a future sqlx bump that does change one fails here, by name,
//! rather than in whichever integration test happens to reach that query.

use sqlx::Row;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::str::FromStr;
use stellargate::db;

async fn pool() -> db::Db {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(SqliteConnectOptions::from_str("sqlite::memory:").unwrap())
        .await
        .unwrap();
    db::migrate(&pool).await.unwrap();
    pool
}

/// `COUNT(*)` → `i64`: the most common call site (list totals, `memo_exists`,
/// `merchant_exists`, `count_active_api_keys`, and the `pragma_table_info`
/// column probes in `migrate`).
#[tokio::test]
async fn count_star_decodes_as_i64() {
    let pool = pool().await;

    let zero: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM payments")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(zero, 0);

    let has_column: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pragma_table_info('payments') WHERE name = 'expires_at'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(has_column, 1);

    assert!(!db::memo_exists(&pool, "no-such-memo").await.unwrap());
    assert!(
        !db::merchant_exists(&pool, "no-such-merchant")
            .await
            .unwrap()
    );
    assert_eq!(db::count_active_api_keys(&pool, "m1").await.unwrap(), 0);
}

/// `SELECT 1` → `i64`: `db::ping`, behind `/health`.
#[tokio::test]
async fn literal_integer_decodes_as_i64() {
    let pool = pool().await;
    db::ping(&pool).await.unwrap();

    let one: i64 = sqlx::query_scalar("SELECT 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(one, 1);
}

/// `COALESCE(SUM(amount_stroops), 0)` → `i64`, both on an empty set (the
/// `COALESCE` fallback) and on a total above `i32::MAX`, so a narrowing to a
/// 32-bit decode would show up as an error rather than a wrapped amount.
#[tokio::test]
async fn coalesced_sum_decodes_as_i64_without_narrowing() {
    let pool = pool().await;
    assert_eq!(db::sum_processed_stroops(&pool, "p1").await.unwrap(), 0);

    let big: i64 = i64::from(i32::MAX) + 1;
    for (tx_hash, amount) in [("tx-a", big), ("tx-b", 5)] {
        sqlx::query(
            "INSERT INTO processed_transactions (payment_id, tx_hash, amount_stroops)
             VALUES ('p1', ?, ?)",
        )
        .bind(tx_hash)
        .bind(amount)
        .execute(&pool)
        .await
        .unwrap();
    }

    assert_eq!(
        db::sum_processed_stroops(&pool, "p1").await.unwrap(),
        big + 5
    );
}

/// `SELECT EXISTS(...)` → `bool`: SQLite returns an INTEGER 0/1, which the
/// last-active-key guard in `revoke_api_key` decodes straight to `bool`.
#[tokio::test]
async fn exists_integer_decodes_as_bool() {
    let pool = pool().await;
    sqlx::query("INSERT INTO kv_state (key, value) VALUES ('k', 'v')")
        .execute(&pool)
        .await
        .unwrap();

    let yes: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM kv_state WHERE key = 'k')")
        .fetch_one(&pool)
        .await
        .unwrap();
    let no: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM kv_state WHERE key = 'x')")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(yes);
    assert!(!no);
}

/// `fetch_optional` into `Option<String>`: a missing row is `None`, a present
/// TEXT value round-trips (`get_state` /
/// `find_payment_id_by_idempotency_key`).
#[tokio::test]
async fn optional_text_scalar_decodes_as_option_string() {
    let pool = pool().await;
    assert_eq!(db::get_state(&pool, "cursor").await.unwrap(), None);

    db::set_state(&pool, "cursor", "12345").await.unwrap();
    assert_eq!(
        db::get_state(&pool, "cursor").await.unwrap().as_deref(),
        Some("12345")
    );
}

/// `query_as` into a tuple mixing `String` and `Option<String>` with NULL and
/// non-NULL TEXT columns, the exact `KeyRow` shape `list_api_keys` uses, and
/// the `created_at` text timestamp the schema default writes.
#[tokio::test]
async fn text_and_nullable_text_columns_decode_in_query_as_tuples() {
    let pool = pool().await;
    sqlx::query(
        "INSERT INTO api_keys (id, merchant_id, key_hash, prefix, label, last_used_at)
         VALUES ('k1', 'm1', 'hash-1', 'sg_abcd', NULL, '2026-01-01T00:00:00Z')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let keys = db::list_api_keys(&pool, "m1").await.unwrap();
    assert_eq!(keys.len(), 1);
    let key = &keys[0];
    assert_eq!(key.id, "k1");
    assert_eq!(key.prefix, "sg_abcd");
    assert_eq!(key.label, None);
    assert_eq!(key.last_used_at.as_deref(), Some("2026-01-01T00:00:00Z"));
    assert_eq!(key.revoked_at, None);
    // Written by `strftime('%Y-%m-%dT%H:%M:%SZ','now')`: TEXT, not a number.
    assert_eq!(key.created_at.len(), "2026-01-01T00:00:00Z".len());
    assert!(key.created_at.ends_with('Z'));

    let pair: Option<(String, String)> =
        sqlx::query_as("SELECT id, merchant_id FROM api_keys WHERE key_hash = ?")
            .bind("hash-1")
            .fetch_optional(&pool)
            .await
            .unwrap();
    assert_eq!(pair, Some(("k1".to_string(), "m1".to_string())));
}

/// `Row::get` of INTEGER columns into `i64` (`attempts`, `manual_attempts` —
/// the latter added by `ALTER TABLE`) and of a legacy space-separated text
/// timestamp into `String`, which `row_to_webhook_delivery` then normalises.
#[tokio::test]
async fn row_get_decodes_integer_and_legacy_text_timestamp_columns() {
    let pool = pool().await;
    sqlx::query(
        "INSERT INTO webhook_deliveries
            (id, payment_id, url, payload, status, attempts, created_at)
         VALUES ('d1', 'p1', 'https://example.com/hook', '{}', 'failed', 3,
                 '2026-04-29 15:00:00')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let delivery = db::get_webhook_delivery(&pool, "d1")
        .await
        .unwrap()
        .expect("delivery row");
    assert_eq!(delivery.attempts, 3);
    assert_eq!(delivery.manual_attempts, 0);
    assert_eq!(delivery.event_type, None);
    assert_eq!(delivery.last_attempt, None);
    assert_eq!(delivery.created_at, "2026-04-29T15:00:00Z");

    let row = sqlx::query("SELECT attempts, created_at FROM webhook_deliveries WHERE id = 'd1'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.get::<i64, _>("attempts"), 3);
    assert_eq!(row.get::<String, _>("created_at"), "2026-04-29 15:00:00");
}

/// `EXPLAIN QUERY PLAN` rows → `(i64, i64, i64, String)`, as decoded by the
/// redrive-index test in `db.rs`.
#[tokio::test]
async fn explain_query_plan_rows_decode_as_integer_and_text_tuple() {
    let pool = pool().await;
    let rows: Vec<(i64, i64, i64, String)> =
        sqlx::query_as("EXPLAIN QUERY PLAN SELECT id FROM payments WHERE memo = 'x'")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert!(!rows.is_empty());
}

/// The mapping stays strict in the other direction: a TEXT value is not
/// silently coerced to `i64`. `amount` and `paid_amount` are TEXT decimals and
/// must go through `money::parse_stroops`, never a direct integer decode.
#[tokio::test]
async fn text_column_does_not_decode_as_i64() {
    let pool = pool().await;
    sqlx::query("INSERT INTO kv_state (key, value) VALUES ('n', '42')")
        .execute(&pool)
        .await
        .unwrap();

    let res: Result<i64, _> = sqlx::query_scalar("SELECT value FROM kv_state WHERE key = 'n'")
        .fetch_one(&pool)
        .await;
    assert!(
        matches!(res, Err(sqlx::Error::ColumnDecode { .. })),
        "TEXT must not decode as i64, got {res:?}"
    );
}
