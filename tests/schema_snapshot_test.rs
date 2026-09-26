//! Asserts a freshly `db::migrate`d database matches the checked-in schema
//! snapshot at `tests/schema_snapshot.sql` (issue #308).
//!
//! `db::migrate` (in `src/db.rs`) is the only schema definition that
//! actually runs — the `migrations/*.sql` directory that used to sit
//! alongside it was never read at runtime and had drifted to the point of
//! missing whole tables the running code depends on. Rather than maintain a
//! second, hand-synchronised definition, this test makes the *running*
//! schema self-verifying: any change to `db::migrate` that isn't reflected
//! in `tests/schema_snapshot.sql` fails CI here instead of drifting
//! silently, the same failure mode `migrations/` had.
//!
//! To update the snapshot after an intentional schema change, run this test
//! with `--nocapture` — on a mismatch it prints the freshly generated
//! snapshot text so it can be pasted directly into `tests/schema_snapshot.sql`.

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::str::FromStr;
use stellargate::db;

const SNAPSHOT: &str = include_str!("schema_snapshot.sql");

/// Every `CREATE TABLE` / `CREATE INDEX` statement SQLite stores for the
/// schema `db::migrate` produces on a fresh database, in the same
/// `(type, name)` order the snapshot file uses. SQLite strips `IF NOT
/// EXISTS` when it stores DDL, so this is the schema's true, canonical text
/// — comparing it directly is what makes this test self-verifying rather
/// than another hand-maintained copy that can itself drift.
async fn current_schema_statements() -> Vec<String> {
    let pool = SqlitePoolOptions::new()
        .connect_with(SqliteConnectOptions::from_str("sqlite::memory:").unwrap())
        .await
        .unwrap();
    db::migrate(&pool).await.unwrap();

    schema_statements(&pool).await
}

/// The stored DDL of every table and index in `pool`, ordered as the
/// snapshot file is.
async fn schema_statements(pool: &sqlx::SqlitePool) -> Vec<String> {
    let rows: Vec<(String,)> =
        sqlx::query_as("SELECT sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY type, name")
            .fetch_all(pool)
            .await
            .unwrap();

    rows.into_iter().map(|(sql,)| sql).collect()
}

/// A database file under the system temp dir, deleted (with its WAL and
/// shared-memory side files) on drop.
struct TempDbFile(std::path::PathBuf);

impl TempDbFile {
    fn new(label: &str) -> Self {
        Self(std::env::temp_dir().join(format!("stellargate-{label}-{}.db", uuid::Uuid::new_v4())))
    }

    async fn pool(&self) -> sqlx::SqlitePool {
        SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&self.0)
                    .create_if_missing(true),
            )
            .await
            .unwrap()
    }
}

impl Drop for TempDbFile {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let mut path = self.0.clone().into_os_string();
            path.push(suffix);
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Parse the checked-in snapshot file into the same shape: one entry per
/// statement, split on a line containing only `;`. Leading `--`-comment
/// lines (the file's header) are dropped.
fn snapshot_statements() -> Vec<String> {
    let mut body: String = SNAPSHOT
        .lines()
        .skip_while(|line| line.starts_with("--") || line.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    // Guarantee a trailing "\n;\n" so the final statement splits the same
    // way as every other one — `.lines()` above already stripped the file's
    // trailing newline, so without this the last entry would keep its `;`
    // glued on instead of being consumed as a separator.
    body.push('\n');
    body.split("\n;\n")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

#[tokio::test]
async fn migrated_schema_matches_the_checked_in_snapshot() {
    let current = current_schema_statements().await;
    let expected = snapshot_statements();

    if current == expected {
        return;
    }

    let mut message = String::from(
        "The schema db::migrate produces no longer matches tests/schema_snapshot.sql.\n\
         If this change was intentional, replace the snapshot file's statements with \
         the freshly generated ones below (each already ends with a lone `;` line):\n\n",
    );
    for stmt in &current {
        message.push_str(stmt);
        message.push_str("\n;\n");
    }

    let missing: Vec<_> = expected.iter().filter(|s| !current.contains(s)).collect();
    let added: Vec<_> = current.iter().filter(|s| !expected.contains(s)).collect();
    if !missing.is_empty() {
        message.push_str(&format!(
            "\nIn the snapshot but NOT in the live schema ({} statement(s)):\n",
            missing.len()
        ));
        for stmt in missing {
            message.push_str(&format!("  - {}\n", stmt.lines().next().unwrap_or(stmt)));
        }
    }
    if !added.is_empty() {
        message.push_str(&format!(
            "\nIn the live schema but NOT in the snapshot ({} statement(s)):\n",
            added.len()
        ));
        for stmt in added {
            message.push_str(&format!("  + {}\n", stmt.lines().next().unwrap_or(stmt)));
        }
    }

    panic!("{message}");
}

/// A database file written by the current release (whose schema is exactly
/// the checked-in snapshot) must come through `db::migrate` on the current
/// sqlx unchanged: same DDL, same rows.
#[tokio::test]
async fn database_file_from_current_release_upgrades_cleanly() {
    let file = TempDbFile::new("upgrade");

    /* Build the file from the snapshot text rather than `db::migrate`, so it
    stands in for a database the previous build created. */
    {
        let pool = file.pool().await;
        for stmt in snapshot_statements() {
            sqlx::query(sqlx::AssertSqlSafe(stmt))
                .execute(&pool)
                .await
                .unwrap();
        }
        sqlx::query(
            "INSERT INTO payments
                (id, destination_address, memo, amount, asset_issuer,
                 created_at, updated_at, expires_at)
             VALUES ('payment-1', 'destination', 'memo-1', '10', NULL,
                     '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z',
                     '2026-01-01T01:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;
    }

    let pool = file.pool().await;
    db::migrate(&pool).await.unwrap();

    assert_eq!(schema_statements(&pool).await, snapshot_statements());

    let (created_at, expires_at, asset_issuer): (String, String, Option<String>) = sqlx::query_as(
        "SELECT created_at, expires_at, asset_issuer FROM payments WHERE id = 'payment-1'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(created_at, "2026-01-01T00:00:00Z");
    assert_eq!(expires_at, "2026-01-01T01:00:00Z");
    assert_eq!(asset_issuer, None);

    // Idempotent: a second startup against the same file changes nothing.
    db::migrate(&pool).await.unwrap();
    assert_eq!(schema_statements(&pool).await, snapshot_statements());
    pool.close().await;
}

/// A file from before `expires_at` and `asset_issuer` existed, with the old
/// `datetime('now')` timestamp format, gets both columns added and its rows
/// back-filled and normalised.
#[tokio::test]
async fn legacy_database_file_gets_expires_at_and_asset_issuer_backfilled() {
    let file = TempDbFile::new("legacy");

    {
        let pool = file.pool().await;
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
                     '2026-01-01 00:00:00', '2026-01-01 00:00:00')",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;
    }

    let pool = file.pool().await;
    db::migrate(&pool).await.unwrap();

    for column in ["expires_at", "asset_issuer"] {
        let present: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM pragma_table_info('payments') WHERE name = ?")
                .bind(column)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(present, 1, "migrate did not add payments.{column}");
    }

    let (created_at, updated_at, expires_at, asset_issuer): (
        String,
        String,
        String,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT created_at, updated_at, expires_at, asset_issuer FROM payments WHERE id = 'payment-1'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(created_at, "2026-01-01T00:00:00Z");
    assert_eq!(updated_at, "2026-01-01T00:00:00Z");
    assert_eq!(expires_at, "2026-01-01T01:00:00Z");
    assert_eq!(asset_issuer, None);

    /* Every table and index the snapshot names must exist. The payments DDL
    itself legitimately differs (columns added by ALTER land at the end), so
    compare names rather than text. */
    let names: Vec<(String,)> =
        sqlx::query_as("SELECT name FROM sqlite_master WHERE sql IS NOT NULL")
            .fetch_all(&pool)
            .await
            .unwrap();
    let names: Vec<String> = names.into_iter().map(|(n,)| n).collect();
    for stmt in snapshot_statements() {
        let name = stmt
            .split_whitespace()
            .nth(2)
            .unwrap()
            .split('(')
            .next()
            .unwrap()
            .to_string();
        assert!(names.contains(&name), "legacy upgrade is missing {name}");
    }
    pool.close().await;
}
