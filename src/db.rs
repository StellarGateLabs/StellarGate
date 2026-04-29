use anyhow::Result;
use sqlx::{Pool, Row, Sqlite};

pub type Db = Pool<Sqlite>;

pub async fn migrate(pool: &Db) -> Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS payments (
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
            created_at TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at TEXT NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_payments_memo ON payments(memo)")
        .execute(pool)
        .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_payments_status ON payments(status)")
        .execute(pool)
        .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS webhook_deliveries (
            id TEXT PRIMARY KEY,
            payment_id TEXT NOT NULL,
            url TEXT NOT NULL,
            payload TEXT NOT NULL,
            status TEXT NOT NULL DEFAULT 'pending',
            attempts INTEGER NOT NULL DEFAULT 0,
            last_attempt TEXT,
            created_at TEXT NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;

    Ok(())
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Payment {
    pub id: String,
    pub merchant_id: String,
    pub destination_address: String,
    pub memo: String,
    pub amount: String,
    pub asset: String,
    pub status: String,
    pub webhook_url: Option<String>,
    pub tx_hash: Option<String>,
    pub paid_amount: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

fn row_to_payment(row: &sqlx::sqlite::SqliteRow) -> Payment {
    Payment {
        id: row.get("id"),
        merchant_id: row.get("merchant_id"),
        destination_address: row.get("destination_address"),
        memo: row.get("memo"),
        amount: row.get("amount"),
        asset: row.get("asset"),
        status: row.get("status"),
        webhook_url: row.get("webhook_url"),
        tx_hash: row.get("tx_hash"),
        paid_amount: row.get("paid_amount"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

pub async fn create_payment(
    pool: &Db,
    id: &str,
    merchant_id: &str,
    destination_address: &str,
    memo: &str,
    amount: &str,
    asset: &str,
    webhook_url: Option<&str>,
) -> Result<Payment> {
    sqlx::query(
        "INSERT INTO payments (id, merchant_id, destination_address, memo, amount, asset, webhook_url)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(id)
    .bind(merchant_id)
    .bind(destination_address)
    .bind(memo)
    .bind(amount)
    .bind(asset)
    .bind(webhook_url)
    .execute(pool)
    .await?;

    get_payment(pool, id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Payment not found after insert"))
}

pub async fn get_payment(pool: &Db, id: &str) -> Result<Option<Payment>> {
    let row = sqlx::query(
        "SELECT id, merchant_id, destination_address, memo, amount, asset, status,
                webhook_url, tx_hash, paid_amount, created_at, updated_at
         FROM payments WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    Ok(row.as_ref().map(row_to_payment))
}

pub async fn list_payments(
    pool: &Db,
    status: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<(Vec<Payment>, i64)> {
    let (rows, total) = if let Some(s) = status {
        let rows = sqlx::query(
            "SELECT id, merchant_id, destination_address, memo, amount, asset, status,
                    webhook_url, tx_hash, paid_amount, created_at, updated_at
             FROM payments WHERE status = ? ORDER BY created_at DESC LIMIT ? OFFSET ?",
        )
        .bind(s)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?;

        let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM payments WHERE status = ?")
            .bind(s)
            .fetch_one(pool)
            .await?;

        (rows, total)
    } else {
        let rows = sqlx::query(
            "SELECT id, merchant_id, destination_address, memo, amount, asset, status,
                    webhook_url, tx_hash, paid_amount, created_at, updated_at
             FROM payments ORDER BY created_at DESC LIMIT ? OFFSET ?",
        )
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?;

        let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM payments")
            .fetch_one(pool)
            .await?;

        (rows, total)
    };

    Ok((rows.iter().map(row_to_payment).collect(), total))
}

pub async fn find_pending_by_memo(pool: &Db, memo: &str) -> Result<Option<Payment>> {
    let row = sqlx::query(
        "SELECT id, merchant_id, destination_address, memo, amount, asset, status,
                webhook_url, tx_hash, paid_amount, created_at, updated_at
         FROM payments WHERE memo = ? AND status = 'pending'",
    )
    .bind(memo)
    .fetch_optional(pool)
    .await?;

    Ok(row.as_ref().map(row_to_payment))
}

pub async fn update_payment_status(
    pool: &Db,
    id: &str,
    status: &str,
    tx_hash: &str,
    paid_amount: &str,
) -> Result<()> {
    sqlx::query(
        "UPDATE payments SET status = ?, tx_hash = ?, paid_amount = ?, updated_at = datetime('now') WHERE id = ?",
    )
    .bind(status)
    .bind(tx_hash)
    .bind(paid_amount)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn memo_exists(pool: &Db, memo: &str) -> Result<bool> {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM payments WHERE memo = ?")
        .bind(memo)
        .fetch_one(pool)
        .await?;
    Ok(count > 0)
}

pub async fn save_webhook_delivery(
    pool: &Db,
    id: &str,
    payment_id: &str,
    url: &str,
    payload: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO webhook_deliveries (id, payment_id, url, payload) VALUES (?, ?, ?, ?)",
    )
    .bind(id)
    .bind(payment_id)
    .bind(url)
    .bind(payload)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn update_webhook_delivery(pool: &Db, id: &str, status: &str, attempts: i64) -> Result<()> {
    sqlx::query(
        "UPDATE webhook_deliveries SET status = ?, attempts = ?, last_attempt = datetime('now') WHERE id = ?",
    )
    .bind(status)
    .bind(attempts)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}
