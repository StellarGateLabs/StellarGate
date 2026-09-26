use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::str::FromStr;
use std::time::Duration;
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

/// API keys must never be persisted in a recoverable form (issue #462): the
/// only representation stored in either `merchants.api_key_hash` (legacy) or
/// `api_keys.key_hash` (current) is a SHA-256 digest of the raw key.
#[tokio::test]
async fn api_keys_are_stored_hashed_not_plaintext() {
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

    let expected_digest = {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(raw_key.as_bytes()))
    };

    let merchant_hash: String =
        sqlx::query_scalar("SELECT api_key_hash FROM merchants WHERE id = 'm1'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_ne!(
        merchant_hash, raw_key,
        "merchants.api_key_hash must not store the raw key"
    );
    assert_eq!(merchant_hash, expected_digest);

    let key_hash: String =
        sqlx::query_scalar("SELECT key_hash FROM api_keys WHERE merchant_id = 'm1'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_ne!(
        key_hash, raw_key,
        "api_keys.key_hash must not store the raw key"
    );
    assert_eq!(key_hash, expected_digest);
}

// ── production pool settings (issue #642) ────────────────────────────────────

/// A throwaway on-disk database URL. WAL needs a real file: an in-memory
/// database silently keeps `journal_mode = memory` whatever is requested.
fn temp_db_url() -> (std::path::PathBuf, String) {
    let dir = std::env::temp_dir().join(format!("stellargate-pool-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let url = format!("sqlite://{}", dir.join("pool.db").display());
    (dir, url)
}

/// `db::open_pool` must leave every pooled connection in WAL mode with
/// `synchronous = NORMAL`, the configured busy timeout and foreign keys on.
/// Two connections are held at once so the PRAGMAs are checked on more than
/// the first connection the pool happens to open: all but `journal_mode` are
/// per-connection settings.
#[tokio::test]
async fn open_pool_applies_production_pragmas_to_every_connection() {
    let (dir, url) = temp_db_url();
    let pool = db::open_pool(&url, 2, Duration::from_millis(1234))
        .await
        .unwrap();

    let mut conns = vec![pool.acquire().await.unwrap(), pool.acquire().await.unwrap()];
    for conn in &mut conns {
        let journal_mode: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(&mut **conn)
            .await
            .unwrap();
        assert_eq!(journal_mode, "wal");

        // 0 = OFF, 1 = NORMAL, 2 = FULL, 3 = EXTRA.
        let synchronous: i64 = sqlx::query_scalar("PRAGMA synchronous")
            .fetch_one(&mut **conn)
            .await
            .unwrap();
        assert_eq!(synchronous, 1, "synchronous must be NORMAL");

        let busy_timeout: i64 = sqlx::query_scalar("PRAGMA busy_timeout")
            .fetch_one(&mut **conn)
            .await
            .unwrap();
        assert_eq!(busy_timeout, 1234);

        let foreign_keys: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
            .fetch_one(&mut **conn)
            .await
            .unwrap();
        assert_eq!(foreign_keys, 1, "sqlx enables foreign keys by default");
    }

    drop(conns);
    pool.close().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// `create_if_missing` must still be honoured: a fresh deployment points
/// `DATABASE_URL` at a file that does not exist yet.
#[tokio::test]
async fn open_pool_creates_a_missing_database_file() {
    let (dir, url) = temp_db_url();
    let path = dir.join("pool.db");
    assert!(!path.exists());

    let pool = db::open_pool(&url, 1, Duration::from_millis(5000))
        .await
        .unwrap();
    db::migrate(&pool).await.unwrap();
    assert!(path.exists());

    pool.close().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// Two writers contending for SQLite's single write lock must wait out the
/// busy timeout rather than fail with `SQLITE_BUSY` the moment the lock is
/// taken. One connection holds an open write transaction while the other
/// tries to write, then the first commits well inside the timeout.
#[tokio::test]
async fn open_pool_busy_timeout_lets_a_contending_writer_wait() {
    let (dir, url) = temp_db_url();
    let pool = db::open_pool(&url, 2, Duration::from_millis(5000))
        .await
        .unwrap();
    db::migrate(&pool).await.unwrap();

    let mut holder = pool.acquire().await.unwrap();
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut *holder)
        .await
        .unwrap();
    sqlx::query("INSERT INTO kv_state (key, value) VALUES ('a', '1')")
        .execute(&mut *holder)
        .await
        .unwrap();

    let waiter = {
        let pool = pool.clone();
        tokio::spawn(async move {
            sqlx::query("INSERT INTO kv_state (key, value) VALUES ('b', '2')")
                .execute(&pool)
                .await
        })
    };

    tokio::time::sleep(Duration::from_millis(200)).await;
    sqlx::query("COMMIT").execute(&mut *holder).await.unwrap();
    drop(holder);

    waiter
        .await
        .unwrap()
        .expect("the second writer must wait for the lock, not fail with SQLITE_BUSY");

    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM kv_state WHERE key IN ('a', 'b')")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 2);

    pool.close().await;
    let _ = std::fs::remove_dir_all(&dir);
}
