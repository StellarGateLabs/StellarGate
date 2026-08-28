use anyhow::Result;
use sqlx::{Pool, Row, Sqlite};

pub type Db = Pool<Sqlite>;

/// Normalize a raw SQLite timestamp to strict RFC 3339 UTC with a Z suffix.
///
/// Handles both legacy rows (`"2026-04-29 15:00:00"` / `"2026-04-29T15:00:00"`)
/// and already-correct rows (`"2026-04-29T15:00:00Z"`). Any value that doesn't
/// look like a 19-character datetime is returned unchanged so we never silently
/// corrupt unexpected data.
fn normalize_ts(raw: &str) -> String {
    let s = raw.trim();
    // Already has an explicit offset/Z — nothing to do.
    if s.ends_with('Z') || s.contains('+') {
        return s.to_string();
    }
    // Replace the space separator with T if present, then append Z.
    if s.len() == 19 {
        let with_t = s.replacen(' ', "T", 1);
        return format!("{with_t}Z");
    }
    s.to_string()
}

pub async fn migrate(pool: &Db) -> Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS payments (
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
    .execute(pool)
    .await?;

    /* Bring pre-existing payment tables up to schema. New databases already have
    `expires_at` from the CREATE TABLE above; older ones need it added in
    place. SQLite rejects a non-constant DEFAULT on ALTER ... ADD COLUMN, so we
    add it nullable and backfill below. */
    let has_expires_at: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pragma_table_info('payments') WHERE name = 'expires_at'",
    )
    .fetch_one(pool)
    .await?;
    if has_expires_at == 0 {
        sqlx::query("ALTER TABLE payments ADD COLUMN expires_at TEXT")
            .execute(pool)
            .await?;
    }
    /* Backfill any row without an expiry (legacy rows, or rows inserted in the
    brief window before the column existed). `created_at + 1h` mirrors the
    default TTL; SQLite's date functions accept the stored RFC 3339 `Z` form. */
    sqlx::query(
        "UPDATE payments
            SET expires_at = strftime('%Y-%m-%dT%H:%M:%SZ', created_at, '+1 hour')
          WHERE expires_at IS NULL",
    )
    .execute(pool)
    .await?;

    /* `asset_issuer` completes the asset identity of an intent. Only the code
    used to be stored, so which USDC (say) a historical row referred to lived in
    process configuration and changed whenever `ACCEPTED_ASSETS` was edited
    (issue #223). Existing databases get the column added in place; it is
    nullable by design — NULL means the native asset, which has no issuer.
    Rows created before this migration are backfilled, best-effort, from the
    configured allow-list by [`backfill_asset_issuers`]. */
    let has_asset_issuer: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pragma_table_info('payments') WHERE name = 'asset_issuer'",
    )
    .fetch_one(&mut *tx)
    .await?;
    if has_asset_issuer == 0 {
        sqlx::query("ALTER TABLE payments ADD COLUMN asset_issuer TEXT")
            .execute(&mut *tx)
            .await?;
    }

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_payments_memo ON payments(memo)")
        .execute(pool)
        .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_payments_status ON payments(status)")
        .execute(pool)
        .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_payments_created_id ON payments(created_at DESC, id DESC)",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS webhook_deliveries (
            id TEXT PRIMARY KEY,
            payment_id TEXT NOT NULL,
            url TEXT NOT NULL,
            payload TEXT NOT NULL,
            event_type TEXT,
            status TEXT NOT NULL DEFAULT 'pending',
            attempts INTEGER NOT NULL DEFAULT 0,
            last_attempt TEXT,
            created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now'))
        )",
    )
    .execute(pool)
    .await?;

    /* Bring pre-existing delivery tables up to schema. `event_type` records
    which event the payload represents so a redelivery can echo the original
    `X-StellarGate-Event` header instead of guessing. Rows written before this
    column existed stay NULL; readers fall back to the `event` field inside the
    stored payload. */
    let has_event_type: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pragma_table_info('webhook_deliveries') WHERE name = 'event_type'",
    )
    .fetch_one(pool)
    .await?;
    if has_event_type == 0 {
        sqlx::query("ALTER TABLE webhook_deliveries ADD COLUMN event_type TEXT")
            .execute(&mut *tx)
            .await?;
    }

    /* Back-fill `event_type` for legacy rows whose column is NULL but whose
    stored payload carries an `event` field (issue #237). This makes the
    FALLBACK_EVENT path in `WebhookDelivery::event` genuinely unreachable for
    rows this gateway wrote — the fallback is only needed for payloads that
    could not be parsed at all (corruption, manual edits), which should never
    happen for rows we inserted ourselves.
    The JSON path expression `json_extract(payload, '$.event')` returns NULL
    when the field is absent, keeping those rows NULL (they remain for the
    fallback). This is a no-op for rows that already have event_type set. */
    sqlx::query(
        "UPDATE webhook_deliveries
            SET event_type = json_extract(payload, '$.event')
          WHERE event_type IS NULL
            AND json_extract(payload, '$.event') IS NOT NULL",
    )
    .execute(&mut *tx)
    .await?;

    /* `acknowledged_at` records that somebody has seen a terminal failure and
    acted on it — set by the bulk requeue/acknowledge endpoint. It exists so
    retention can distinguish "this failure was dealt with" from "nobody has
    looked at this yet", and refuse to delete the latter (issue #319). Rows
    that predate the column are NULL, i.e. unacknowledged, which is the safe
    reading: we do not know that anyone saw them. */
    let has_acknowledged_at: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pragma_table_info('webhook_deliveries') WHERE name = 'acknowledged_at'",
    )
    .fetch_one(&mut *tx)
    .await?;
    if has_acknowledged_at == 0 {
        sqlx::query("ALTER TABLE webhook_deliveries ADD COLUMN acknowledged_at TEXT")
            .execute(&mut *tx)
            .await?;
    }

    /* Durable key/value state — used by the Horizon poller to persist its
    paging cursor so it resumes exactly where it left off across restarts. */
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS kv_state (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL,
            updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now'))
        )",
    )
    .execute(pool)
    .await?;

    /* Merchants are provisioned via POST /merchants. The raw API key is never
    stored; only its SHA-256 hex digest is persisted so a DB breach does not
    expose live credentials. */
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS merchants (
            id TEXT PRIMARY KEY,
            api_key_hash TEXT NOT NULL UNIQUE,
            created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now'))
        )",
    )
    .execute(pool)
    .await?;

    /* API keys, one row per credential rather than one per merchant, so a key
    can be rotated (issue a second, revoke the first) and revoked individually
    without disturbing the merchant record.

    Only the SHA-256 digest is stored; `prefix` keeps the first few characters
    of the raw key so an operator can tell two keys apart in a list without the
    secret being recoverable. `revoked_at` is a tombstone rather than a delete
    so an audit trail survives revocation. */
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS api_keys (
            id TEXT PRIMARY KEY,
            merchant_id TEXT NOT NULL,
            key_hash TEXT NOT NULL UNIQUE,
            prefix TEXT NOT NULL,
            label TEXT,
            created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
            last_used_at TEXT,
            revoked_at TEXT
        )",
    )
    .execute(pool)
    .await?;

    /* Authentication looks a key up by hash on every request, so this index is
    load-bearing rather than an optimisation. */
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_api_keys_hash ON api_keys(key_hash)")
        .execute(&mut *tx)
        .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_api_keys_merchant ON api_keys(merchant_id)")
        .execute(&mut *tx)
        .await?;

    /* Carry pre-existing single-key merchants across. Their raw key is not
    recoverable, but the hash is all authentication needs, so keys issued
    before this table existed keep working. The prefix is unknown for those
    rows — mark them rather than inventing one. */
    sqlx::query(
        "INSERT OR IGNORE INTO api_keys (id, merchant_id, key_hash, prefix, label, created_at)
         SELECT lower(hex(randomblob(16))), id, api_key_hash, 'legacy', 'migrated', created_at
           FROM merchants
          WHERE api_key_hash IS NOT NULL AND api_key_hash <> ''",
    )
    .execute(&mut *tx)
    .await?;

    /* `webhook_deliveries` is queried by payment_id on every delivery listing
    and by the redrive worker; without this it is a full scan (issue #112). */
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_webhook_deliveries_payment
         ON webhook_deliveries(payment_id)",
    )
    .execute(pool)
    .await?;

    /* Partial index covering the redrive worker's per-tick query (issue #239).
    The query filters on `status IN ('pending', 'failed')` plus `attempts <
    max_attempts`, then applies date arithmetic over `last_attempt` /
    `created_at`. Two properties matter here:

    1. The `WHERE status IN ('pending', 'failed')` partial clause keeps the
       index tiny: in steady state almost every row is `delivered` and therefore
       immediately excluded — a full table index would grow without bound and
       still need to visit the status check first.
    2. Including `attempts`, `last_attempt`, and `created_at` in the index
       covers the remaining predicates as much as SQLite's limited expression
       indexing allows; the date-arithmetic expression is not sargable, but
       narrowing the candidate set to the handful of non-delivered, under-cap
       rows first is where the dominant win is.

    Verified with EXPLAIN QUERY PLAN: `list_redrivable_deliveries` now shows
    "SEARCH webhook_deliveries USING INDEX idx_webhook_deliveries_redrive"
    instead of a full table scan (see `redrive_index_is_used` test). */
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_webhook_deliveries_redrive
         ON webhook_deliveries(status, attempts, last_attempt, created_at)
         WHERE status IN ('pending', 'failed')",
    )
    .execute(pool)
    .await?;

    /* Idempotency keys for payment creation. A key is unique per merchant and
    maps to the payment id minted for the first request that used it, so a
    client retrying after a network blip gets the original payment back
    instead of a duplicate intent. */
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS idempotency_keys (
            merchant_id TEXT NOT NULL,
            idempotency_key TEXT NOT NULL,
            payment_id TEXT NOT NULL,
            created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
            PRIMARY KEY (merchant_id, idempotency_key)
        )",
    )
    .execute(pool)
    .await?;

    /* Every on-chain transaction we credit to an intent, one row per
    (payment_id, tx_hash). The cumulative received amount for an intent is the
    SUM of `amount_stroops` over its rows, so re-seeing a transaction (on a
    later poll cycle, over the stream, or from a concurrent reconciler) is an
    idempotent no-op instead of a double-credit. `amount_stroops` is the
    integer stroop value so SUM is exact. */
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS processed_transactions (
            payment_id TEXT NOT NULL,
            /* The transaction hash is half the dedup key, so an empty value
            would make every unhashed record collide on one row and silently
            discard all but the first (issue #224). Reject it in the schema as
            well as at the write path. */
            tx_hash TEXT NOT NULL CHECK (tx_hash <> ''),
            amount_stroops INTEGER NOT NULL,
            created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
            PRIMARY KEY (payment_id, tx_hash)
        )",
    )
    .execute(pool)
    .await?;

    /* Backfill from legacy rows that recorded only the most-recent `tx_hash`
    and a cumulative `paid_amount`, so upgrading preserves the received-amount
    ledger for intents that are still in flight. Idempotent via ON CONFLICT, so
    it is safe to run on every startup. */
    let legacy = sqlx::query(
        "SELECT id, tx_hash, paid_amount FROM payments
         WHERE tx_hash IS NOT NULL AND tx_hash <> '' AND paid_amount IS NOT NULL",
    )
    .fetch_all(pool)
    .await?;
    for row in &legacy {
        let id: String = row.get("id");
        let tx_hash: String = row.get("tx_hash");
        let paid_amount: String = row.get("paid_amount");
        if let Some(stroops) = crate::money::parse_stroops(&paid_amount) {
            sqlx::query(
                "INSERT INTO processed_transactions (payment_id, tx_hash, amount_stroops)
                 VALUES (?, ?, ?)
                 ON CONFLICT(payment_id, tx_hash) DO NOTHING",
            )
            .bind(&id)
            .bind(&tx_hash)
            .bind(stroops)
            .execute(pool)
            .await?;
        }
    }

    /* Rows written before issue #224 was fixed may carry an empty `tx_hash`,
    where two distinct unhashed records collapsed onto one primary key. We do
    not delete them — the amount they carry was really received, and dropping
    the row would silently reduce an intent's paid total — but we surface them
    so an operator can reconcile against the ledger by hand. */
    let unhashed: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM processed_transactions WHERE tx_hash = ''")
            .fetch_one(pool)
            .await?;
    if unhashed > 0 {
        tracing::warn!(
            rows = unhashed,
            "processed_transactions contains rows with an empty tx_hash, written before \
             unhashed Horizon records were rejected; these may under-count an intent's \
             received amount and should be reconciled against Horizon by hand"
        );
    }

    /* Normalise legacy rows that were written by the old datetime('now') default,
    which produced "YYYY-MM-DD HH:MM:SS" (space, no Z). Safe to run on every
    startup — the WHERE clause skips rows that are already RFC 3339. */
    for tbl_col in [
        ("payments", "created_at"),
        ("payments", "updated_at"),
        ("webhook_deliveries", "created_at"),
    ] {
        let sql = format!(
            "UPDATE {} SET {col} = replace({col}, ' ', 'T') || 'Z' WHERE {col} NOT LIKE '%T%'",
            tbl_col.0,
            col = tbl_col.1
        );
        sqlx::query(&sql).execute(pool).await?;
    }

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
    /// Issuer of the asset this intent was priced in, as an `G…` account id.
    /// `None` means the native asset (XLM), which has no issuer.
    ///
    /// Persisted at creation time from the accepted-asset allow-list so that
    /// editing `ACCEPTED_ASSETS` later cannot retroactively change which asset
    /// a historical intent refers to (issue #223).
    pub asset_issuer: Option<String>,
    pub status: String,
    pub webhook_url: Option<String>,
    pub tx_hash: Option<String>,
    pub paid_amount: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    /// When this intent stops being `pending` and is swept to `expired`.
    pub expires_at: String,
}

fn row_to_payment(row: &sqlx::sqlite::SqliteRow) -> Payment {
    Payment {
        id: row.get("id"),
        merchant_id: row.get("merchant_id"),
        destination_address: row.get("destination_address"),
        memo: row.get("memo"),
        amount: row.get("amount"),
        asset: row.get("asset"),
        asset_issuer: row.get("asset_issuer"),
        status: row.get("status"),
        webhook_url: row.get("webhook_url"),
        tx_hash: row.get("tx_hash"),
        paid_amount: row.get("paid_amount"),
        created_at: normalize_ts(&row.get::<String, _>("created_at")),
        updated_at: normalize_ts(&row.get::<String, _>("updated_at")),
        expires_at: normalize_ts(&row.get::<String, _>("expires_at")),
    }
}

/// Fields needed to insert a new payment intent.
pub struct NewPayment<'a> {
    pub id: &'a str,
    pub merchant_id: &'a str,
    pub destination_address: &'a str,
    pub memo: &'a str,
    pub amount: &'a str,
    /// Issuer of `asset`, or `None` for the native asset. Resolved from the
    /// accepted-asset allow-list by the caller and stored alongside the code so
    /// the pair is a complete asset identity (issue #223).
    pub asset_issuer: Option<&'a str>,
    pub asset: &'a str,
    pub webhook_url: Option<&'a str>,
    /// Seconds from now until the intent expires. The expiry timestamp is
    /// computed by SQLite at insert time as `now + ttl_secs`.
    pub ttl_secs: i64,
}

pub async fn create_payment(pool: &Db, new: NewPayment<'_>) -> Result<Payment> {
    /* Canonicalize the amount: parse to stroops, then convert back to the
    canonical string representation. This ensures "10.00", "10.0", and "10"
    all serialize identically, eliminating spurious string-based comparisons
    across create/get/webhook responses. */
    let stroops =
        crate::money::parse_stroops(new.amount).ok_or_else(|| anyhow::anyhow!("Invalid amount"))?;
    let canonical_amount = crate::money::stroops_to_string(stroops);

    /* Compute the expiry as `now + ttl_secs` in SQLite so it shares the exact
    clock and RFC 3339 format as created_at. */
    let ttl_modifier = format!("{:+} seconds", new.ttl_secs);
    sqlx::query(
        "INSERT INTO payments (id, merchant_id, destination_address, memo, amount, asset, asset_issuer, webhook_url, expires_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, strftime('%Y-%m-%dT%H:%M:%SZ','now',?))",
    )
    .bind(new.id)
    .bind(new.merchant_id)
    .bind(new.destination_address)
    .bind(new.memo)
    .bind(&canonical_amount)
    .bind(new.asset)
    .bind(new.asset_issuer)
    .bind(new.webhook_url)
    .bind(&ttl_modifier)
    .execute(pool)
    .await?;

    get_payment(pool, new.id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Payment not found after insert"))
}

/// Look up the payment id previously minted for `(merchant_id, key)`, if any.
pub async fn find_payment_id_by_idempotency_key(
    pool: &Db,
    merchant_id: &str,
    key: &str,
) -> Result<Option<String>> {
    let id: Option<String> = sqlx::query_scalar(
        "SELECT payment_id FROM idempotency_keys WHERE merchant_id = ? AND idempotency_key = ?",
    )
    .bind(merchant_id)
    .bind(key)
    .fetch_optional(pool)
    .await?;
    Ok(id)
}

/// Record the payment id minted for `(merchant_id, key)`. If the key already
/// exists (e.g. a concurrent request won the race), the existing mapping is left
/// untouched and the winning payment id is returned; otherwise `payment_id` is
/// stored and returned.
pub async fn save_idempotency_key(
    pool: &Db,
    merchant_id: &str,
    key: &str,
    payment_id: &str,
) -> Result<String> {
    sqlx::query(
        "INSERT INTO idempotency_keys (merchant_id, idempotency_key, payment_id)
         VALUES (?, ?, ?)
         ON CONFLICT(merchant_id, idempotency_key) DO NOTHING",
    )
    .bind(merchant_id)
    .bind(key)
    .bind(payment_id)
    .execute(pool)
    .await?;

    // Re-read so a concurrent insert that won the race returns the canonical id.
    let stored = find_payment_id_by_idempotency_key(pool, merchant_id, key)
        .await?
        .unwrap_or_else(|| payment_id.to_string());
    Ok(stored)
}

pub async fn get_payment(pool: &Db, id: &str) -> Result<Option<Payment>> {
    let row = sqlx::query(
        "SELECT id, merchant_id, destination_address, memo, amount, asset, asset_issuer, status,
                webhook_url, tx_hash, paid_amount, created_at, updated_at, expires_at
         FROM payments WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    Ok(row.as_ref().map(row_to_payment))
}

pub async fn list_payments(
    pool: &Db,
    merchant_id: &str,
    status: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<(Vec<Payment>, i64)> {
    let (rows, total) = if let Some(s) = status {
        let rows = sqlx::query(
            "SELECT id, merchant_id, destination_address, memo, amount, asset, asset_issuer, status,
                    webhook_url, tx_hash, paid_amount, created_at, updated_at, expires_at
             FROM payments WHERE merchant_id = ? AND status = ? ORDER BY created_at DESC LIMIT ? OFFSET ?",
        )
        .bind(merchant_id)
        .bind(s)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?;

        let total: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM payments WHERE merchant_id = ? AND status = ?",
        )
        .bind(merchant_id)
        .bind(s)
        .fetch_one(pool)
        .await?;

        (rows, total)
    } else {
        let rows = sqlx::query(
            "SELECT id, merchant_id, destination_address, memo, amount, asset, asset_issuer, status,
                    webhook_url, tx_hash, paid_amount, created_at, updated_at, expires_at
             FROM payments WHERE merchant_id = ? ORDER BY created_at DESC LIMIT ? OFFSET ?",
        )
        .bind(merchant_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?;

        let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM payments WHERE merchant_id = ?")
            .bind(merchant_id)
            .fetch_one(pool)
            .await?;

        (rows, total)
    };

    Ok((rows.iter().map(row_to_payment).collect(), total))
}

pub async fn list_payments_keyset(
    pool: &Db,
    merchant_id: &str,
    status: Option<&str>,
    limit: i64,
    cursor: Option<(&str, &str)>,
) -> Result<Vec<Payment>> {
    let rows = match (status, cursor) {
        (None, None) => {
            sqlx::query(
                "SELECT id, merchant_id, destination_address, memo, amount, asset, asset_issuer, status,
                    webhook_url, tx_hash, paid_amount, created_at, updated_at, expires_at
             FROM payments WHERE merchant_id = ? ORDER BY created_at DESC, id DESC LIMIT ?",
            )
            .bind(merchant_id)
            .bind(limit)
            .fetch_all(pool)
            .await?
        }

        (None, Some((ts, cid))) => {
            sqlx::query(
                "SELECT id, merchant_id, destination_address, memo, amount, asset, asset_issuer, status,
                    webhook_url, tx_hash, paid_amount, created_at, updated_at, expires_at
             FROM payments
             WHERE merchant_id = ? AND (created_at < ? OR (created_at = ? AND id < ?))
             ORDER BY created_at DESC, id DESC LIMIT ?",
            )
            .bind(merchant_id)
            .bind(ts)
            .bind(ts)
            .bind(cid)
            .bind(limit)
            .fetch_all(pool)
            .await?
        }

        (Some(s), None) => {
            sqlx::query(
                "SELECT id, merchant_id, destination_address, memo, amount, asset, asset_issuer, status,
                    webhook_url, tx_hash, paid_amount, created_at, updated_at, expires_at
             FROM payments WHERE merchant_id = ? AND status = ? ORDER BY created_at DESC, id DESC LIMIT ?",
            )
            .bind(merchant_id)
            .bind(s)
            .bind(limit)
            .fetch_all(pool)
            .await?
        }

        (Some(s), Some((ts, cid))) => {
            sqlx::query(
                "SELECT id, merchant_id, destination_address, memo, amount, asset, asset_issuer, status,
                    webhook_url, tx_hash, paid_amount, created_at, updated_at, expires_at
             FROM payments
             WHERE merchant_id = ? AND status = ? AND (created_at < ? OR (created_at = ? AND id < ?))
             ORDER BY created_at DESC, id DESC LIMIT ?",
            )
            .bind(merchant_id)
            .bind(s)
            .bind(ts)
            .bind(ts)
            .bind(cid)
            .bind(limit)
            .fetch_all(pool)
            .await?
        }
    };

    Ok(rows.iter().map(row_to_payment).collect())
}

/// All payments still awaiting confirmation or top-up, oldest first. Rows whose
/// TTL has elapsed are excluded even if the sweeper hasn't transitioned them
/// yet, so an overdue intent is never polled.
pub async fn list_pending(pool: &Db) -> Result<Vec<Payment>> {
    let rows = sqlx::query(
        "SELECT id, merchant_id, destination_address, memo, amount, asset, asset_issuer, status,
                webhook_url, tx_hash, paid_amount, created_at, updated_at, expires_at
         FROM payments
         WHERE status IN ('pending', 'underpaid')
           AND expires_at > strftime('%Y-%m-%dT%H:%M:%SZ','now')
         ORDER BY created_at ASC",
    )
    .fetch_all(pool)
    .await?;

    Ok(rows.iter().map(row_to_payment).collect())
}

/// Transition every watchable payment whose TTL has elapsed to `expired`,
/// returning the rows that were swept so the caller can fire `payment.expired`
/// webhooks. Each row is updated with a guard on a watchable status so a payment
/// that settles concurrently is left untouched and not double-reported.
pub async fn expire_overdue(pool: &Db) -> Result<Vec<Payment>> {
    let overdue = sqlx::query(
        "SELECT id, merchant_id, destination_address, memo, amount, asset, asset_issuer, status,
                webhook_url, tx_hash, paid_amount, created_at, updated_at, expires_at
         FROM payments
         WHERE status IN ('pending', 'underpaid')
           AND expires_at <= strftime('%Y-%m-%dT%H:%M:%SZ','now')
         ORDER BY created_at ASC",
    )
    .fetch_all(pool)
    .await?;

    let mut expired = Vec::new();
    for row in &overdue {
        let mut payment = row_to_payment(row);
        let result = sqlx::query(
            "UPDATE payments
                SET status = 'expired',
                    updated_at = strftime('%Y-%m-%dT%H:%M:%SZ','now')
              WHERE id = ? AND status IN ('pending', 'underpaid')",
        )
        .bind(&payment.id)
        .execute(pool)
        .await?;

        /* Only report rows we actually transitioned; a concurrent settlement
        may have flipped the status out from under us. */
        if result.rows_affected() == 1 {
            payment.status = "expired".to_string();
            expired.push(payment);
        }
    }

    Ok(expired)
}

pub async fn find_pending_by_memo(pool: &Db, memo: &str) -> Result<Option<Payment>> {
    let row = sqlx::query(
        "SELECT id, merchant_id, destination_address, memo, amount, asset, asset_issuer, status,
                webhook_url, tx_hash, paid_amount, created_at, updated_at, expires_at
         FROM payments
         WHERE memo = ?
           AND status IN ('pending', 'underpaid')
           AND expires_at > strftime('%Y-%m-%dT%H:%M:%SZ','now')",
    )
    .bind(memo)
    .fetch_optional(pool)
    .await?;

    Ok(row.as_ref().map(row_to_payment))
}

/// Like [`find_pending_by_memo`] but matches any status — used to detect
/// payments arriving after an intent has already been settled or expired.
/// Such payments must still be recorded and reported to the merchant (issue
/// #232), even though the intent's terminal status must not change.
pub async fn find_by_memo_any_status(pool: &Db, memo: &str) -> Result<Option<Payment>> {
    let row = sqlx::query(
        "SELECT id, merchant_id, destination_address, memo, amount, asset, asset_issuer, status,
                webhook_url, tx_hash, paid_amount, created_at, updated_at, expires_at
         FROM payments
         WHERE memo = ?
         ORDER BY created_at DESC
         LIMIT 1",
    )
    .bind(memo)
    .fetch_optional(pool)
    .await?;

    Ok(row.as_ref().map(row_to_payment))
}

/// Transition a payment to a new status, returning `true` when the row was
/// actually updated.
///
/// The `WHERE … AND status IN ('pending', 'underpaid')` guard is the key to
/// single-settlement under concurrent reconciliation (issue #155): SQLite's
/// serialized write path ensures that exactly one of two racing UPDATE
/// statements will match a row still in a watchable state. The loser sees
/// `rows_affected() == 0` and knows it must skip the webhook.
pub async fn update_payment_status(
    pool: &Db,
    id: &str,
    status: &str,
    tx_hash: &str,
    paid_amount: &str,
) -> Result<bool> {
    let result = sqlx::query(
        "UPDATE payments
            SET status = ?, tx_hash = ?, paid_amount = ?,
                updated_at = strftime('%Y-%m-%dT%H:%M:%SZ','now')
          WHERE id = ?
            AND status IN ('pending', 'underpaid')",
    )
    .bind(status)
    .bind(tx_hash)
    .bind(paid_amount)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Record that transaction `tx_hash`, worth `amount_stroops`, has been credited
/// to intent `payment_id`. Returns `true` when this is the first time the
/// transaction was recorded for the intent, and `false` when it was already
/// present (a re-seen record on a later poll cycle, over the stream, or from a
/// concurrent reconciler).
///
/// The `(payment_id, tx_hash)` primary key plus `ON CONFLICT DO NOTHING` makes
/// this the atomic dedup point: SQLite serialises writers, so exactly one of
/// two racing inserts for the same transaction observes `rows_affected() == 1`.
pub async fn record_processed_tx(
    pool: &Db,
    payment_id: &str,
    tx_hash: &str,
    amount_stroops: i64,
) -> Result<bool> {
    /* An empty hash is not a hash: it would make every unhashed record share
    one primary key, so the first would be credited and every later one
    silently dropped as "already processed" (issue #224). Callers must skip
    such records; reaching here with one is a bug, so it is an error rather
    than a quiet `false` (which reads as "already recorded"). */
    if tx_hash.is_empty() {
        anyhow::bail!("refusing to record a processed transaction with an empty tx_hash");
    }

    let result = sqlx::query(
        "INSERT INTO processed_transactions (payment_id, tx_hash, amount_stroops)
         VALUES (?, ?, ?)
         ON CONFLICT(payment_id, tx_hash) DO NOTHING",
    )
    .bind(payment_id)
    .bind(tx_hash)
    .bind(amount_stroops)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Sum of every transaction recorded against `payment_id`, in stroops. This is
/// the authoritative received-amount ledger for an intent — independent of how
/// many transactions arrived, or the order they were seen in.
pub async fn sum_processed_stroops(pool: &Db, payment_id: &str) -> Result<i64> {
    let total: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(amount_stroops), 0) FROM processed_transactions WHERE payment_id = ?",
    )
    .bind(payment_id)
    .fetch_one(pool)
    .await?;
    Ok(total)
}

/// Read a value from the durable key/value state table, if present.
pub async fn get_state(pool: &Db, key: &str) -> Result<Option<String>> {
    let value: Option<String> = sqlx::query_scalar("SELECT value FROM kv_state WHERE key = ?")
        .bind(key)
        .fetch_optional(pool)
        .await?;
    Ok(value)
}

/// Key recording that the one-off `asset_issuer` backfill has already run, so
/// it is never applied twice.
const ASSET_ISSUER_BACKFILL_KEY: &str = "schema_backfill_asset_issuer_v1";

/// Backfill `payments.asset_issuer` for rows created before the column existed,
/// using the currently-configured accepted-asset allow-list.
///
/// This runs exactly once per database (guarded by a marker in `kv_state`),
/// because it is a *best-effort* reconstruction: the issuer a historical intent
/// was actually priced in was never recorded, so all we can do is assume it was
/// the one configured for that asset code today. Running it repeatedly would
/// let a later `ACCEPTED_ASSETS` edit rewrite history a second time, which is
/// exactly the problem the column exists to prevent (issue #223).
///
/// Native assets are left NULL — they have no issuer. Rows whose asset code is
/// no longer in the allow-list are also left NULL, since there is nothing left
/// to reconstruct from.
pub async fn backfill_asset_issuers(
    pool: &Db,
    accepted_assets: &[crate::config::AcceptedAsset],
) -> Result<()> {
    if get_state(pool, ASSET_ISSUER_BACKFILL_KEY).await?.is_some() {
        return Ok(());
    }

    let mut filled = 0u64;
    for asset in accepted_assets {
        let Some(issuer) = asset.issuer.as_deref() else {
            continue;
        };
        let result = sqlx::query(
            "UPDATE payments SET asset_issuer = ? WHERE asset = ? AND asset_issuer IS NULL",
        )
        .bind(issuer)
        .bind(&asset.code)
        .execute(pool)
        .await?;
        filled += result.rows_affected();
    }

    set_state(pool, ASSET_ISSUER_BACKFILL_KEY, "done").await?;
    if filled > 0 {
        tracing::info!(
            rows = filled,
            "backfilled payments.asset_issuer from the configured accepted assets; \
             rows created before this migration are best-effort"
        );
    }
    Ok(())
}

/// Insert or update a value in the durable key/value state table.
pub async fn set_state(pool: &Db, key: &str, value: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO kv_state (key, value, updated_at)
         VALUES (?, ?, strftime('%Y-%m-%dT%H:%M:%SZ','now'))
         ON CONFLICT(key) DO UPDATE SET
            value = excluded.value,
            updated_at = excluded.updated_at",
    )
    .bind(key)
    .bind(value)
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

/// Record an outbound webhook delivery. `event_type` is the event name the
/// payload carries (e.g. `payment.underpaid`); it is persisted so a later
/// redelivery can reproduce the original `X-StellarGate-Event` header.
pub async fn save_webhook_delivery(
    pool: &Db,
    id: &str,
    payment_id: &str,
    url: &str,
    payload: &str,
    event_type: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO webhook_deliveries (id, payment_id, url, payload, event_type) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(id)
    .bind(payment_id)
    .bind(url)
    .bind(payload)
    .bind(event_type)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn update_webhook_delivery(
    pool: &Db,
    id: &str,
    status: &str,
    attempts: i64,
) -> Result<()> {
    sqlx::query(
        "UPDATE webhook_deliveries SET status = ?, attempts = ?, last_attempt = strftime('%Y-%m-%dT%H:%M:%SZ','now') WHERE id = ?",
    )
    .bind(status)
    .bind(attempts)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct WebhookDelivery {
    pub id: String,
    pub payment_id: String,
    pub url: String,
    pub payload: String,
    /// The event this payload represents. `None` only for rows written before
    /// the column existed — use [`WebhookDelivery::event`] to read it.
    pub event_type: Option<String>,
    pub status: String,
    pub attempts: i64,
    pub last_attempt: Option<String>,
    pub created_at: String,
}

/// Event name used when a legacy row has no `event_type` and its stored payload
/// cannot be parsed. This sentinel is intentionally non-actionable: a receiver
/// that routes on `payment.completed` (the old fallback) could fulfil an order
/// because a payload failed to parse — which is precisely the risk #237 closes.
/// Receivers must treat `payment.unknown` as an opaque signal to look the
/// payment up via `GET /v1/payments/:id` rather than acting on the event name
/// directly.
const FALLBACK_EVENT: &str = "payment.unknown";

impl WebhookDelivery {
    /// The event name to report for this delivery, falling back to the `event`
    /// field of the stored payload for rows written before `event_type`
    /// existed (and for which the migration backfill could not extract the
    /// field — e.g. corrupted or externally written rows). If neither source
    /// yields a value, returns [`FALLBACK_EVENT`] (`"payment.unknown"`), which
    /// is intentionally non-actionable: a caller receiving that value must
    /// fetch the full record via `GET /v1/payments/:id` rather than acting on
    /// the event name directly (issue #237).
    pub fn event(&self) -> String {
        if let Some(event) = &self.event_type {
            return event.clone();
        }
        // Reach here only for rows whose payload could not be used by the
        // migration backfill (or rows written after the column existed but
        // somehow NULL — not possible through normal code paths). The backfill
        // already tried json_extract; we try a full parse here as a last resort
        // before falling back to the sentinel.
        serde_json::from_str::<serde_json::Value>(&self.payload)
            .ok()
            .and_then(|v| v.get("event")?.as_str().map(str::to_string))
            .unwrap_or_else(|| FALLBACK_EVENT.to_string())
    }
}

fn row_to_webhook_delivery(row: &sqlx::sqlite::SqliteRow) -> WebhookDelivery {
    WebhookDelivery {
        id: row.get("id"),
        payment_id: row.get("payment_id"),
        url: row.get("url"),
        payload: row.get("payload"),
        event_type: row.get("event_type"),
        status: row.get("status"),
        attempts: row.get("attempts"),
        last_attempt: row.get("last_attempt"),
        created_at: normalize_ts(&row.get::<String, _>("created_at")),
    }
}

/// Deliveries eligible for the background redrive worker: not yet delivered,
/// under the attempt cap, and idle long enough that no in-flight `dispatch()`
/// call for the same row can still be running.
///
/// A delivery row's status only changes at the *end* of a `dispatch()` call
/// (success, final failure, or an SSRF rejection); while an attempt is still
/// in progress the row stays `pending` with no signal that work is under way.
/// `grace_secs` is the idle window past `last_attempt` (or `created_at` for a
/// row never attempted) that a row must clear before being considered stuck
/// rather than merely in flight — callers must size it comfortably above the
/// worst-case inline delivery time so this worker never races a live
/// `dispatch()` for the same row. It is also the hard floor under the
/// exponential backoff below, so a row is never touched sooner than this
/// regardless of `attempts`.
///
/// A row that has failed at least once (`attempts > 0`) additionally has to
/// clear an exponential backoff — `backoff_initial_secs * 2^(attempts-1)`,
/// capped at `backoff_max_secs` — before it is considered eligible again.
/// A row with `attempts == 0` (left behind by a crash between insert and its
/// first send, not a delivery failure) is exempt from this backoff and is
/// gated by `grace_secs` alone.
pub async fn list_redrivable_deliveries(
    pool: &Db,
    max_attempts: i64,
    grace_secs: i64,
    backoff_initial_secs: i64,
    backoff_max_secs: i64,
) -> Result<Vec<WebhookDelivery>> {
    let rows = sqlx::query(
        "SELECT id, payment_id, url, payload, event_type, status, attempts, last_attempt, created_at
         FROM webhook_deliveries
         WHERE status IN ('pending', 'failed')
           AND attempts < ?
           AND datetime(COALESCE(last_attempt, created_at), '+' || (
                 CASE WHEN attempts = 0 THEN ?
                      ELSE MAX(?, MIN(? * (1 << MIN(attempts - 1, 32)), ?))
                 END
               ) || ' seconds') <= datetime('now')
         ORDER BY created_at ASC",
    )
    .bind(max_attempts)
    .bind(grace_secs)
    .bind(grace_secs)
    .bind(backoff_initial_secs)
    .bind(backoff_max_secs)
    .fetch_all(pool)
    .await?;

    Ok(rows.iter().map(row_to_webhook_delivery).collect())
}

/// Get all webhook deliveries for a payment, ordered by created_at descending.
pub async fn list_webhook_deliveries(pool: &Db, payment_id: &str) -> Result<Vec<WebhookDelivery>> {
    let rows = sqlx::query(
        "SELECT id, payment_id, url, payload, event_type, status, attempts, last_attempt, created_at
         FROM webhook_deliveries WHERE payment_id = ? ORDER BY created_at DESC",
    )
    .bind(payment_id)
    .fetch_all(pool)
    .await?;

    Ok(rows.iter().map(row_to_webhook_delivery).collect())
}

/// Get a specific webhook delivery by id.
pub async fn get_webhook_delivery(pool: &Db, id: &str) -> Result<Option<WebhookDelivery>> {
    let row = sqlx::query(
        "SELECT id, payment_id, url, payload, event_type, status, attempts, last_attempt, created_at
         FROM webhook_deliveries WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    Ok(row.as_ref().map(row_to_webhook_delivery))
}

/// Probe database connectivity. Returns `Ok(())` if the pool can execute a
/// trivial query, or `Err` if the database is unreachable.
pub async fn ping(pool: &Db) -> Result<()> {
    sqlx::query_scalar::<_, i64>("SELECT 1")
        .fetch_one(pool)
        .await?;
    Ok(())
}

/* ---------------------------------------------------------------------------
Merchant API-key management
--------------------------------------------------------------------------- */

/// Hash a raw API key with SHA-256, returning the hex digest.
/// This is the only representation stored in the database.
fn hash_api_key(raw: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(raw.as_bytes()))
}

/// Create a merchant and its first API key, atomically — a merchant that
/// exists with no way to authenticate would be unusable and un-fixable through
/// the API.
///
/// The raw key must be shown to the caller once; it is not recoverable.
///
/// `merchants.api_key_hash` is written only to satisfy the column's NOT NULL
/// constraint on databases created before `api_keys` existed. Nothing reads it
/// any more — authentication goes through `api_keys` so that rotation and
/// revocation work — and it is not maintained as keys change.
pub async fn create_merchant(pool: &Db, id: &str, raw_key: &str, prefix: &str) -> Result<String> {
    let mut tx = pool.begin().await?;

    sqlx::query("INSERT INTO merchants (id, api_key_hash) VALUES (?, ?)")
        .bind(id)
        .bind(hash_api_key(raw_key))
        .execute(&mut *tx)
        .await?;

    let key_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO api_keys (id, merchant_id, key_hash, prefix, label)
         VALUES (?, ?, ?, ?, 'initial')",
    )
    .bind(&key_id)
    .bind(id)
    .bind(hash_api_key(raw_key))
    .bind(prefix)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(key_id)
}

/* ---------------------------------------------------------------------------
Retention
--------------------------------------------------------------------------- */

/// Rows removed per statement. Deleting in batches keeps each write lock short:
/// SQLite has a single writer, so one unbounded `DELETE` over a large table
/// would stall every payment write until it finished.
pub const PRUNE_BATCH: i64 = 500;

/// Delete one batch of idempotency keys older than `retention_days`.
///
/// A key only has to outlive the window in which a client might retry the
/// create it guarded. Past that it is dead weight, and the table has no other
/// bound (issue #110).
///
/// Returns how many rows went; the caller loops until a batch comes back
/// short.
pub async fn prune_idempotency_keys(pool: &Db, retention_days: i64) -> Result<u64> {
    let cutoff = format!("-{retention_days} days");
    let n = sqlx::query(
        "DELETE FROM idempotency_keys
          WHERE rowid IN (
              SELECT rowid FROM idempotency_keys
               WHERE created_at < strftime('%Y-%m-%dT%H:%M:%SZ','now',?)
               LIMIT ?
          )",
    )
    .bind(&cutoff)
    .bind(PRUNE_BATCH)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(n)
}

/// Delete one batch of webhook deliveries that have finished and aged out.
///
/// Only `delivered` and `failed` rows are eligible. A `pending` row is still
/// owned by the redrive worker — pruning it would silently drop a delivery
/// that was going to be retried. The worker marks rows `failed` once attempts
/// are exhausted, so nothing stays exempt forever (issue #111).
pub async fn prune_webhook_deliveries(pool: &Db, retention_days: i64) -> Result<u64> {
    let cutoff = format!("-{retention_days} days");
    let n = sqlx::query(
        "DELETE FROM webhook_deliveries
          WHERE rowid IN (
              SELECT rowid FROM webhook_deliveries
               WHERE status IN ('delivered','failed')
                 AND created_at < strftime('%Y-%m-%dT%H:%M:%SZ','now',?)
               LIMIT ?
          )",
    )
    .bind(&cutoff)
    .bind(PRUNE_BATCH)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(n)
}

/// A key as exposed to an operator. Never carries the secret — only a
/// prefix, so two keys can be told apart in a list.
#[derive(Debug, Clone)]
pub struct ApiKeyInfo {
    pub id: String,
    pub prefix: String,
    pub label: Option<String>,
    pub created_at: String,
    pub last_used_at: Option<String>,
    pub revoked_at: Option<String>,
}

/// Characters of the raw key kept in `prefix` for display.
const KEY_PREFIX_LEN: usize = 12;

/// Mint a new API key: 256 bits from the OS CSPRNG, hex-encoded behind an
/// `sg_` marker.
///
/// Deliberately not a UUID. A v4 UUID carries only 122 random bits and spends
/// 6 of them encoding its version and variant, which is fine for an identifier
/// and wrong for a bearer credential. The `sg_` marker makes a leaked key
/// recognisable in logs and lets secret scanners match on it.
///
/// Returns `(raw_key, prefix)`. The raw key is shown once and never stored.
pub fn generate_api_key() -> (String, String) {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    let raw = format!("sg_{}", hex::encode(bytes));
    let prefix = raw.chars().take(KEY_PREFIX_LEN).collect();
    (raw, prefix)
}

/// Store a new API key for a merchant. Only the digest is persisted.
pub async fn create_api_key(
    pool: &Db,
    merchant_id: &str,
    raw_key: &str,
    prefix: &str,
    label: Option<&str>,
) -> Result<String> {
    let id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO api_keys (id, merchant_id, key_hash, prefix, label)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(merchant_id)
    .bind(hash_api_key(raw_key))
    .bind(prefix)
    .bind(label)
    .execute(pool)
    .await?;
    Ok(id)
}

/// Resolve a raw API key to its merchant, rejecting revoked keys.
///
/// Also refreshes `last_used_at`, but at most once a minute per key: this runs
/// on every authenticated request, and SQLite takes a write lock for each
/// update, so touching it unconditionally would put a write in the path of
/// every read. Minute granularity is plenty to spot a key that has gone quiet
/// or one that is being used when it should not be.
pub async fn find_merchant_by_key(pool: &Db, raw_key: &str) -> Result<Option<String>> {
    let hash = hash_api_key(raw_key);

    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT id, merchant_id FROM api_keys WHERE key_hash = ? AND revoked_at IS NULL",
    )
    .bind(&hash)
    .fetch_optional(pool)
    .await?;

    let Some((key_id, merchant_id)) = row else {
        return Ok(None);
    };

    sqlx::query(
        "UPDATE api_keys
            SET last_used_at = strftime('%Y-%m-%dT%H:%M:%SZ','now')
          WHERE id = ?
            AND (last_used_at IS NULL
                 OR last_used_at < strftime('%Y-%m-%dT%H:%M:%SZ','now','-60 seconds'))",
    )
    .bind(&key_id)
    .execute(pool)
    .await?;

    Ok(Some(merchant_id))
}

/// Every key issued to a merchant, newest first, including revoked ones so the
/// history stays visible.
pub async fn list_api_keys(pool: &Db, merchant_id: &str) -> Result<Vec<ApiKeyInfo>> {
    /// (id, prefix, label, created_at, last_used_at, revoked_at) as selected below.
    type KeyRow = (
        String,
        String,
        Option<String>,
        String,
        Option<String>,
        Option<String>,
    );

    let rows: Vec<KeyRow> = sqlx::query_as(
        "SELECT id, prefix, label, created_at, last_used_at, revoked_at
               FROM api_keys WHERE merchant_id = ? ORDER BY created_at DESC, id DESC",
    )
    .bind(merchant_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(
            |(id, prefix, label, created_at, last_used_at, revoked_at)| ApiKeyInfo {
                id,
                prefix,
                label,
                created_at,
                last_used_at,
                revoked_at,
            },
        )
        .collect())
}

/// The outcome of an atomic revocation attempt (issue #247).
///
/// The previous implementation called `count_active_api_keys` and then
/// `revoke_api_key` in two separate statements. Two concurrent revocations of
/// a merchant's two remaining keys would each read a count of 2, each pass the
/// guard, and both succeed — locking the merchant out. SQLite's serialised
/// write path makes it safe to fold the guard into the UPDATE's `WHERE` clause
/// and inspect `rows_affected`, exactly as `update_payment_status` and
/// `expire_overdue` do.
#[derive(Debug, PartialEq, Eq)]
pub enum RevokeKeyOutcome {
    /// The key was revoked successfully.
    Revoked,
    /// The key exists and is active, but it is the merchant's only remaining
    /// active key — revoking it would lock them out.
    LastActiveKey,
    /// No active key with that id was found for this merchant (never existed,
    /// wrong merchant, or already revoked).
    NotFound,
}

/// Revoke a key atomically. Scoped by merchant so one merchant cannot revoke
/// another's. The last-active-key guard is embedded in the `WHERE` clause, so
/// it is enforced under SQLite's serialised write path rather than as a
/// separate check-then-act pair that concurrent requests can both pass.
///
/// Returns:
/// - [`RevokeKeyOutcome::Revoked`] if the key was revoked.
/// - [`RevokeKeyOutcome::LastActiveKey`] if the key exists but is the only
///   remaining active key. The caller should issue a replacement first.
/// - [`RevokeKeyOutcome::NotFound`] if no active key with that id exists for
///   this merchant.
pub async fn revoke_api_key(
    pool: &Db,
    merchant_id: &str,
    key_id: &str,
) -> Result<RevokeKeyOutcome> {
    // The subquery `(SELECT COUNT(*) … WHERE … AND revoked_at IS NULL) > 1`
    // is evaluated atomically with the UPDATE under SQLite's single-writer
    // serialisation. If it evaluates to false (only one active key left), the
    // UPDATE matches zero rows — the same outcome as when the key does not
    // exist. We distinguish the two by checking whether the key is live at all
    // in a second, read-only query run only when rows_affected == 0.
    let affected = sqlx::query(
        "UPDATE api_keys
            SET revoked_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
          WHERE id = ?
            AND merchant_id = ?
            AND revoked_at IS NULL
            AND (SELECT COUNT(*) FROM api_keys
                  WHERE merchant_id = ? AND revoked_at IS NULL) > 1",
    )
    .bind(key_id)
    .bind(merchant_id)
    .bind(merchant_id)
    .execute(pool)
    .await?
    .rows_affected();

    if affected > 0 {
        return Ok(RevokeKeyOutcome::Revoked);
    }

    // rows_affected == 0: either the key does not exist, or it is the last
    // active key. Distinguish the two with a read query.
    let is_last: bool = sqlx::query_scalar(
        "SELECT EXISTS(
             SELECT 1 FROM api_keys
              WHERE id = ? AND merchant_id = ? AND revoked_at IS NULL
         )",
    )
    .bind(key_id)
    .bind(merchant_id)
    .fetch_one(pool)
    .await?;

    if is_last {
        Ok(RevokeKeyOutcome::LastActiveKey)
    } else {
        Ok(RevokeKeyOutcome::NotFound)
    }
}

/// Number of usable keys a merchant has. Retained for callers that need a
/// count for informational purposes; the revocation guard itself is now
/// embedded atomically inside [`revoke_api_key`] (issue #247).
pub async fn count_active_api_keys(pool: &Db, merchant_id: &str) -> Result<i64> {
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM api_keys WHERE merchant_id = ? AND revoked_at IS NULL",
    )
    .bind(merchant_id)
    .fetch_one(pool)
    .await?;
    Ok(n)
}

/// Whether a merchant exists, so key endpoints can 404 rather than silently
/// operating on nothing.
pub async fn merchant_exists(pool: &Db, merchant_id: &str) -> Result<bool> {
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM merchants WHERE id = ?")
        .bind(merchant_id)
        .fetch_one(pool)
        .await?;
    Ok(n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn memory_db() -> Db {
        let pool = SqlitePoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .unwrap();
        migrate(&pool).await.unwrap();
        pool
    }

    /// An upgrade must not lock out merchants whose keys predate the
    /// `api_keys` table. This simulates a pre-upgrade database — the old
    /// `merchants.api_key_hash` schema with no `api_keys` table — runs the
    /// migration over it, and checks the original key still authenticates.
    ///
    /// Getting this wrong would revoke every live credential on deploy, with
    /// no way to recover them: the raw keys are unrecoverable by design.
    #[tokio::test]
    async fn legacy_single_key_merchants_survive_the_api_keys_migration() {
        let pool = SqlitePoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .unwrap();

        // The schema as it existed before api_keys.
        sqlx::query(
            "CREATE TABLE merchants (
                id TEXT PRIMARY KEY,
                api_key_hash TEXT NOT NULL UNIQUE,
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now'))
            )",
        )
        .execute(&pool)
        .await
        .unwrap();

        let legacy_key = "3f1c9a7e-0b2d-4e5f-8a6b-7c8d9e0f1a2b"; // an old UUID key
        sqlx::query("INSERT INTO merchants (id, api_key_hash) VALUES (?, ?)")
            .bind("legacy-merchant")
            .bind(hash_api_key(legacy_key))
            .execute(&pool)
            .await
            .unwrap();

        migrate(&pool).await.unwrap();

        assert_eq!(
            find_merchant_by_key(&pool, legacy_key).await.unwrap(),
            Some("legacy-merchant".to_string()),
            "a key issued before the api_keys table must keep working"
        );

        // It is now a first-class key: listable, and revocable once replaced.
        let keys = list_api_keys(&pool, "legacy-merchant").await.unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].prefix, "legacy");
        assert!(keys[0].revoked_at.is_none());
    }

    /// Revoking a key must take effect immediately for authentication.
    #[tokio::test]
    async fn revoked_keys_stop_authenticating() {
        let pool = memory_db().await;
        let (raw, prefix) = generate_api_key();
        let key_id = create_merchant(&pool, "m1", &raw, &prefix).await.unwrap();

        assert_eq!(
            find_merchant_by_key(&pool, &raw).await.unwrap(),
            Some("m1".to_string())
        );

        assert!(revoke_api_key(&pool, "m1", &key_id).await.unwrap());
        assert_eq!(find_merchant_by_key(&pool, &raw).await.unwrap(), None);

        // Revoking again reports that nothing was revoked.
        assert!(!revoke_api_key(&pool, "m1", &key_id).await.unwrap());
    }

    /// Generated keys must be unique and carry full entropy.
    #[tokio::test]
    async fn generated_keys_are_unique_and_prefixed() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..500 {
            let (raw, prefix) = generate_api_key();
            assert!(raw.starts_with("sg_"));
            assert_eq!(raw.len(), 67);
            assert!(raw.starts_with(&prefix));
            assert!(seen.insert(raw), "generated a duplicate key");
        }
    }

    fn new_payment<'a>(id: &'a str, memo: &'a str, ttl_secs: i64) -> NewPayment<'a> {
        NewPayment {
            id,
            merchant_id: "m",
            destination_address: "GGATEWAY",
            memo,
            amount: "10",
            asset: "XLM",
            asset_issuer: None,
            webhook_url: None,
            ttl_secs,
        }
    }

    #[tokio::test]
    async fn create_sets_expiry_from_ttl() {
        let pool = memory_db().await;
        // A one-hour TTL lands the expiry strictly in the future...
        let live = create_payment(&pool, new_payment("a", "MEMOA", 3600))
            .await
            .unwrap();
        assert!(live.expires_at > live.created_at);
        // ...while a negative TTL produces an already-overdue expiry.
        let dead = create_payment(&pool, new_payment("b", "MEMOB", -10))
            .await
            .unwrap();
        assert!(dead.expires_at < dead.created_at);
    }

    #[tokio::test]
    async fn list_pending_excludes_overdue_even_before_sweep() {
        let pool = memory_db().await;
        create_payment(&pool, new_payment("live", "MEMOL", 3600))
            .await
            .unwrap();
        create_payment(&pool, new_payment("dead", "MEMOD", -10))
            .await
            .unwrap();

        let pending = list_pending(&pool).await.unwrap();
        let ids: Vec<&str> = pending.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, vec!["live"]);
    }

    #[tokio::test]
    async fn underpaid_payment_remains_findable_for_topup() {
        let pool = memory_db().await;
        create_payment(&pool, new_payment("partial", "MEMOP", 3600))
            .await
            .unwrap();
        update_payment_status(&pool, "partial", "underpaid", "TX1", "3")
            .await
            .unwrap();

        let found = find_pending_by_memo(&pool, "MEMOP").await.unwrap().unwrap();
        assert_eq!(found.id, "partial");
        assert_eq!(found.status, "underpaid");
        assert_eq!(found.paid_amount.as_deref(), Some("3"));
    }

    #[tokio::test]
    async fn overdue_underpaid_payment_expires() {
        let pool = memory_db().await;
        create_payment(&pool, new_payment("partial-dead", "MEMOX", -10))
            .await
            .unwrap();
        update_payment_status(&pool, "partial-dead", "underpaid", "TX1", "3")
            .await
            .unwrap();

        assert!(find_pending_by_memo(&pool, "MEMOX")
            .await
            .unwrap()
            .is_none());

        let expired = expire_overdue(&pool).await.unwrap();
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].id, "partial-dead");
        assert_eq!(expired[0].status, "expired");
    }

    #[tokio::test]
    async fn expire_overdue_transitions_and_is_idempotent() {
        let pool = memory_db().await;
        create_payment(&pool, new_payment("live", "MEMOL", 3600))
            .await
            .unwrap();
        create_payment(&pool, new_payment("dead", "MEMOD", -10))
            .await
            .unwrap();

        // First sweep expires exactly the overdue intent and returns it.
        let expired = expire_overdue(&pool).await.unwrap();
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].id, "dead");
        assert_eq!(expired[0].status, "expired");

        // GET /payments/:id reflects the expired status.
        let fetched = get_payment(&pool, "dead").await.unwrap().unwrap();
        assert_eq!(fetched.status, "expired");
        // The live intent is untouched.
        assert_eq!(
            get_payment(&pool, "live").await.unwrap().unwrap().status,
            "pending"
        );

        // A second sweep is a no-op — nothing is double-reported.
        assert_eq!(expire_overdue(&pool).await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn list_redrivable_deliveries_excludes_delivered_and_over_cap() {
        let pool = memory_db().await;
        create_payment(&pool, new_payment("p1", "MEMOR1", 3600))
            .await
            .unwrap();

        save_webhook_delivery(
            &pool,
            "delivered",
            "p1",
            "http://x",
            "{}",
            "payment.completed",
        )
        .await
        .unwrap();
        update_webhook_delivery(&pool, "delivered", "delivered", 1)
            .await
            .unwrap();

        save_webhook_delivery(
            &pool,
            "over-cap",
            "p1",
            "http://x",
            "{}",
            "payment.completed",
        )
        .await
        .unwrap();
        update_webhook_delivery(&pool, "over-cap", "failed", 8)
            .await
            .unwrap();

        save_webhook_delivery(
            &pool,
            "eligible",
            "p1",
            "http://x",
            "{}",
            "payment.completed",
        )
        .await
        .unwrap();

        let candidates = list_redrivable_deliveries(&pool, 8, 0, 0, 0).await.unwrap();
        let ids: Vec<&str> = candidates.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["eligible"],
            "only the pending row under the attempt cap must be redrivable"
        );
    }

    #[tokio::test]
    async fn record_processed_tx_is_idempotent_and_sums_over_the_set() {
        let pool = memory_db().await;
        create_payment(&pool, new_payment("p", "MEMOTX", 3600))
            .await
            .unwrap();

        // First time a transaction is seen it is recorded and counted.
        assert!(record_processed_tx(&pool, "p", "TX_A", 40_000_000)
            .await
            .unwrap());
        assert_eq!(sum_processed_stroops(&pool, "p").await.unwrap(), 40_000_000);

        // Re-seeing the same transaction is a no-op — no double credit.
        assert!(!record_processed_tx(&pool, "p", "TX_A", 40_000_000)
            .await
            .unwrap());
        assert_eq!(sum_processed_stroops(&pool, "p").await.unwrap(), 40_000_000);

        // A distinct transaction adds to the running total.
        assert!(record_processed_tx(&pool, "p", "TX_B", 30_000_000)
            .await
            .unwrap());
        assert_eq!(sum_processed_stroops(&pool, "p").await.unwrap(), 70_000_000);

        // Re-seeing an *earlier* transaction after a later one is still a no-op,
        // regardless of order (issue #119).
        assert!(!record_processed_tx(&pool, "p", "TX_A", 40_000_000)
            .await
            .unwrap());
        assert_eq!(sum_processed_stroops(&pool, "p").await.unwrap(), 70_000_000);

        // Rows are scoped per intent.
        assert_eq!(sum_processed_stroops(&pool, "other").await.unwrap(), 0);
    }

    #[tokio::test]
    async fn migrate_backfills_processed_transactions_from_legacy_rows() {
        let pool = memory_db().await;
        create_payment(&pool, new_payment("legacy", "MEMOLEG", 3600))
            .await
            .unwrap();
        // Simulate a pre-#119 underpaid intent: only the latest tx_hash and the
        // cumulative paid_amount were persisted.
        update_payment_status(&pool, "legacy", "underpaid", "TX_OLD", "3")
            .await
            .unwrap();
        // The join table is empty until a backfill runs.
        assert_eq!(sum_processed_stroops(&pool, "legacy").await.unwrap(), 0);

        // A subsequent migrate() (as on the next startup) backfills the ledger.
        migrate(&pool).await.unwrap();
        assert_eq!(
            sum_processed_stroops(&pool, "legacy").await.unwrap(),
            30_000_000
        );
        // And it is idempotent across restarts.
        migrate(&pool).await.unwrap();
        assert_eq!(
            sum_processed_stroops(&pool, "legacy").await.unwrap(),
            30_000_000
        );
    }

    #[tokio::test]
    async fn list_redrivable_deliveries_respects_grace_window() {
        let pool = memory_db().await;
        create_payment(&pool, new_payment("p1", "MEMOR2", 3600))
            .await
            .unwrap();
        save_webhook_delivery(&pool, "fresh", "p1", "http://x", "{}", "payment.completed")
            .await
            .unwrap();

        // Freshly inserted, so a large grace window makes it ineligible...
        assert!(list_redrivable_deliveries(&pool, 8, 3600, 0, 0)
            .await
            .unwrap()
            .is_empty());
        // ...while a zero grace window makes it immediately eligible.
        assert_eq!(
            list_redrivable_deliveries(&pool, 8, 0, 0, 0)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn list_redrivable_deliveries_exempts_never_attempted_rows_from_backoff() {
        let pool = memory_db().await;
        create_payment(&pool, new_payment("p1", "MEMOR3", 3600))
            .await
            .unwrap();
        save_webhook_delivery(
            &pool,
            "crashed",
            "p1",
            "http://x",
            "{}",
            "payment.completed",
        )
        .await
        .unwrap();

        // attempts == 0 (never sent) is gated by grace_secs alone, not the
        // exponential backoff, even with a huge backoff floor configured.
        assert_eq!(
            list_redrivable_deliveries(&pool, 8, 0, 3600, 3600)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn list_redrivable_deliveries_backs_off_exponentially_after_a_failure() {
        let pool = memory_db().await;
        create_payment(&pool, new_payment("p1", "MEMOR4", 3600))
            .await
            .unwrap();
        save_webhook_delivery(&pool, "flaky", "p1", "http://x", "{}", "payment.completed")
            .await
            .unwrap();
        update_webhook_delivery(&pool, "flaky", "failed", 1)
            .await
            .unwrap();

        // One failed attempt (attempts=1): backoff = initial * 2^0 = initial.
        // A huge initial delay makes it ineligible even with grace_secs=0.
        assert!(
            list_redrivable_deliveries(&pool, 8, 0, 3600, 3600)
                .await
                .unwrap()
                .is_empty(),
            "a row with a recent failure must wait out the backoff delay"
        );
        // grace_secs is a floor under the backoff: even with backoff disabled
        // (initial=max=0), a large grace_secs still holds the row back.
        assert!(
            list_redrivable_deliveries(&pool, 8, 3600, 0, 0)
                .await
                .unwrap()
                .is_empty(),
            "grace_secs must floor eligibility even when backoff computes to 0"
        );
    }

    #[tokio::test]
    async fn list_redrivable_deliveries_caps_attempt_33_at_the_configured_max() {
        let pool = memory_db().await;
        create_payment(&pool, new_payment("p1", "MEMOR33", 3600))
            .await
            .unwrap();
        save_webhook_delivery(
            &pool,
            "many-failures",
            "p1",
            "http://x",
            "{}",
            "payment.completed",
        )
        .await
        .unwrap();
        update_webhook_delivery(&pool, "many-failures", "failed", 33)
            .await
            .unwrap();

        // At attempt 33, the uncapped factor is 2^32. With the accepted
        // one-day extreme, the row must use the 86,400-second cap without
        // evaluating an overflowing initial * factor product.
        sqlx::query(
            "UPDATE webhook_deliveries
                SET last_attempt = strftime('%Y-%m-%dT%H:%M:%SZ','now','-86399 seconds')
              WHERE id = 'many-failures'",
        )
        .execute(&pool)
        .await
        .unwrap();
        assert!(
            list_redrivable_deliveries(&pool, 34, 0, 86_400, 86_400, 0)
                .await
                .unwrap()
                .is_empty(),
            "attempt 33 must remain ineligible until the configured cap elapses"
        );

        sqlx::query(
            "UPDATE webhook_deliveries
                SET last_attempt = strftime('%Y-%m-%dT%H:%M:%SZ','now','-86401 seconds')
              WHERE id = 'many-failures'",
        )
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(
            list_redrivable_deliveries(&pool, 34, 0, 86_400, 86_400, 0)
                .await
                .unwrap()
                .len(),
            1,
            "attempt 33 must become eligible immediately after the cap"
        );
    }

    /// The partial index on `webhook_deliveries` must exist after migration
    /// (issue #239) — this is a precondition for the index-scan assertion below.
    #[tokio::test]
    async fn redrive_partial_index_exists_after_migration() {
        let pool = memory_db().await;

        let index_exists: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'index'
               AND name = 'idx_webhook_deliveries_redrive'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(
            index_exists, 1,
            "idx_webhook_deliveries_redrive must exist after db::migrate"
        );

        let table: String = sqlx::query_scalar(
            "SELECT tbl_name FROM sqlite_master
             WHERE type = 'index'
               AND name = 'idx_webhook_deliveries_redrive'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(table, "webhook_deliveries");
    }

    /// EXPLAIN QUERY PLAN for the redrive worker's inner query must reference
    /// the partial index, not a full table scan (issue #239).
    ///
    /// We assert that at least one plan step's `detail` column mentions the
    /// index by name, which would not happen on a full scan.
    #[tokio::test]
    async fn redrive_index_is_used_by_list_redrivable_deliveries() {
        let pool = memory_db().await;

        let plan_rows: Vec<(i64, i64, i64, String)> = sqlx::query_as(
            "EXPLAIN QUERY PLAN
             SELECT id FROM webhook_deliveries
             WHERE status IN ('pending', 'failed')
               AND attempts < 8
             ORDER BY created_at ASC",
        )
        .fetch_all(&pool)
        .await
        .unwrap();

        let uses_index = plan_rows
            .iter()
            .any(|(_, _, _, detail)| detail.contains("idx_webhook_deliveries_redrive"));

        assert!(
            uses_index,
            "EXPLAIN QUERY PLAN must reference idx_webhook_deliveries_redrive. \
             Plan was:\n{:#?}",
            plan_rows
                .iter()
                .map(|(_, _, _, d)| d.as_str())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn redrive_backoff_exponent_cap_avoids_extreme_products() {
        assert_eq!(redrive_backoff_exponent_cap(0, 86_400), 0);
        assert_eq!(redrive_backoff_exponent_cap(86_400, 86_400), 0);
        assert_eq!(redrive_backoff_exponent_cap(1, 86_400), 17);

        // Even callers that bypass Config cannot make the SQL multiply two
        // values whose product would exceed SQLite's signed integer range.
        assert_eq!(redrive_backoff_exponent_cap(1, i64::MAX), 63);
        assert_eq!(redrive_backoff_exponent_cap(i64::MAX / 2, i64::MAX), 2);
    }

    // ── file_sizes / sqlite_path (issue: missing DB metrics) ────────────────

    #[test]
    fn file_sizes_is_none_for_in_memory_database() {
        let (main, wal, shm) = file_sizes("sqlite::memory:");
        assert_eq!((main, wal, shm), (None, None, None));
    }

    #[test]
    fn file_sizes_reports_the_main_file_and_absent_wal_shm() {
        let contents = b"pretend sqlite header bytes";
        let path =
            std::env::temp_dir().join(format!("stellargate-metrics-test-{}", uuid::Uuid::new_v4()));
        std::fs::write(&path, contents).unwrap();

        let url = format!("sqlite:{}", path.display());
        let (main, wal, shm) = file_sizes(&url);
        assert_eq!(
            main,
            Some(contents.len() as u64),
            "main file size must be reported"
        );
        assert_eq!(wal, None, "no -wal file exists yet");
        assert_eq!(shm, None, "no -shm file exists yet");

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn file_sizes_strips_query_parameters_before_stat() {
        // The shared-memory DSN other tests in this module use
        // (`sqlite:file:<uuid>?mode=memory&cache=shared`) has no on-disk
        // file, but must not panic or attempt to stat a path still carrying
        // its `?mode=...` query string.
        let (main, wal, shm) = file_sizes(&shared_memory_dsn());
        assert_eq!((main, wal, shm), (None, None, None));
    }
}
