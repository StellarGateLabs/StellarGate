//! One-off audit for intents under-credited by the multi-operation bug.
//!
//! `processed_transactions` has one row **per operation** keyed on
//! `(payment_id, tx_hash, operation_index)`. To check whether an intent was
//! fully credited for a given transaction, all rows for that
//! `(payment_id, tx_hash)` pair must be summed and compared with the on-chain
//! total — comparing a single row's amount would falsely flag every correctly
//! credited multi-op transaction as under-credited (issue #1015).
//!
//! The audit is strictly read-only: it reports affected intents and the missing
//! amount so an operator can correct merchant balances deliberately.

use crate::{db, horizon, money};
use serde::Serialize;
use sqlx::Row;
use std::collections::HashMap;
use std::time::Duration;
use tracing::warn;

/// One recorded transaction whose on-chain total exceeds what was credited.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Discrepancy {
    pub payment_id: String,
    pub merchant_id: String,
    pub tx_hash: String,
    pub recorded_stroops: i64,
    pub on_chain_stroops: i64,
    pub missing_stroops: i64,
    pub missing_amount: String,
}

const INTER_REQUEST_DELAY: Duration = Duration::from_millis(100);
const MAX_FETCH_ATTEMPTS: u32 = 5;

/// Sum of every operation in `ops` (one transaction's payment operations) that
/// belongs to `payment`. Pure, so it is testable without a network.
pub fn on_chain_total(payment: &db::Payment, ops: &[horizon::HorizonPayment]) -> i64 {
    ops.iter()
        .filter_map(|op| horizon::matches_intent(payment, op))
        .map(|m| m.new_stroops)
        .sum()
}

/// Compare a recorded amount with the on-chain total; `None` when not under-credited.
pub fn discrepancy(
    payment: &db::Payment,
    tx_hash: &str,
    recorded_stroops: i64,
    ops: &[horizon::HorizonPayment],
) -> Option<Discrepancy> {
    let on_chain = on_chain_total(payment, ops);
    (on_chain > recorded_stroops).then(|| Discrepancy {
        payment_id: payment.id.clone(),
        merchant_id: payment.merchant_id.clone(),
        tx_hash: tx_hash.to_string(),
        recorded_stroops,
        on_chain_stroops: on_chain,
        missing_stroops: on_chain - recorded_stroops,
        missing_amount: money::stroops_to_string(on_chain - recorded_stroops),
    })
}

async fn fetch_payments_with_retry(
    http: &reqwest::Client,
    horizon_url: &str,
    tx_hash: &str,
) -> anyhow::Result<Vec<horizon::HorizonPayment>> {
    let mut backoff = Duration::from_secs(1);
    for attempt in 1..=MAX_FETCH_ATTEMPTS {
        match horizon::fetch_transaction_payments(http, horizon_url, tx_hash).await {
            Ok(ops) => return Ok(ops),
            Err(e) => {
                let retry_after = e.downcast_ref::<horizon::HorizonHttpError>().and_then(|h| {
                    if h.is_rate_limited() {
                        h.retry_after
                    } else {
                        None
                    }
                });
                let delay = retry_after.unwrap_or(backoff);
                if attempt == MAX_FETCH_ATTEMPTS {
                    return Err(e);
                }
                warn!(%tx_hash, attempt, ?delay, error = %e, "Horizon fetch failed; backing off");
                tokio::time::sleep(delay).await;
                backoff = backoff.saturating_mul(2).min(Duration::from_secs(60));
            }
        }
    }
    unreachable!()
}

/// Scan every `processed_transactions` row against Horizon and return the
/// under-credited ones.
///
/// Rows are grouped by `(payment_id, tx_hash)` and their `amount_stroops` are
/// summed before comparing with the on-chain total. This is essential because
/// since issue #615 there is one row per *operation* rather than one per
/// transaction: comparing a single row's amount with the full on-chain total
/// would falsely report every correctly-credited multi-op transaction as
/// under-credited (issue #1015).
pub async fn find_under_credited(
    pool: &db::Db,
    http: &reqwest::Client,
    horizon_url: &str,
) -> anyhow::Result<Vec<Discrepancy>> {
    // Aggregate in the database: sum amount_stroops per (payment_id, tx_hash)
    // so a multi-operation transaction is treated as a single credited amount.
    let rows = sqlx::query(
        "SELECT pt.payment_id, pt.tx_hash, SUM(pt.amount_stroops) AS amount_stroops
           FROM processed_transactions pt
           JOIN payments p ON p.id = pt.payment_id
          GROUP BY pt.payment_id, pt.tx_hash
          ORDER BY MIN(pt.created_at)",
    )
    .fetch_all(pool)
    .await?;

    let mut out = Vec::new();
    let mut cache: HashMap<String, Vec<horizon::HorizonPayment>> = HashMap::new();
    for row in rows {
        let payment_id: String = row.get("payment_id");
        let tx_hash: String = row.get("tx_hash");
        let recorded: i64 = row.get("amount_stroops");
        let Some(payment) = db::get_payment(pool, &payment_id).await? else {
            continue;
        };
        let ops = if let Some(cached) = cache.get(&tx_hash) {
            cached.clone()
        } else {
            match fetch_payments_with_retry(http, horizon_url, &tx_hash).await {
                Ok(ops) => {
                    cache.insert(tx_hash.clone(), ops.clone());
                    ops
                }
                Err(e) => {
                    warn!(%tx_hash, %payment_id, error = %e, "skipping row after Horizon fetch failures");
                    tokio::time::sleep(INTER_REQUEST_DELAY).await;
                    continue;
                }
            }
        };
        out.extend(discrepancy(&payment, &tx_hash, recorded, &ops));
        tokio::time::sleep(INTER_REQUEST_DELAY).await;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payment() -> db::Payment {
        db::Payment {
            id: "p1".into(),
            merchant_id: "m1".into(),
            destination_address: "GDEST".into(),
            memo: "abc".into(),
            amount: "30".into(),
            asset: "XLM".into(),
            asset_issuer: None,
            status: "underpaid".into(),
            webhook_url: None,
            tx_hash: None,
            paid_amount: None,
            created_at: String::new(),
            updated_at: String::new(),
            expires_at: String::new(),
        }
    }

    fn op(amount: &str, to: &str) -> horizon::HorizonPayment {
        serde_json::from_value(serde_json::json!({
            "type": "payment", "amount": amount, "asset_type": "native", "to": to,
            "transaction_hash": "h1",
            "transaction": {"memo": "abc", "memo_type": "text", "successful": true}
        }))
        .unwrap()
    }

    #[test]
    fn flags_transaction_where_later_operations_were_dropped() {
        let ops = [op("10", "GDEST"), op("20", "GDEST"), op("5", "GOTHER")];
        let d = discrepancy(&payment(), "h1", 100_000_000, &ops).unwrap();
        assert_eq!(d.on_chain_stroops, 300_000_000);
        assert_eq!(d.missing_stroops, 200_000_000);
    }

    #[test]
    fn ignores_correctly_credited_transaction() {
        let ops = [op("10", "GDEST")];
        assert!(discrepancy(&payment(), "h1", 100_000_000, &ops).is_none());
    }

    // ── Grouping fix (issue #1015) ────────────────────────────────────────

    /// A two-operation transaction credited correctly (5 + 5 = 10 XLM recorded,
    /// 10 XLM on chain) must produce no discrepancy when the recorded amount is
    /// the *sum* of both rows (100_000_000 stroops = 10 XLM).
    #[test]
    fn correctly_credited_two_op_transaction_produces_no_discrepancy() {
        // On-chain: two operations of 5 XLM each = 10 XLM total.
        let ops = [op("5", "GDEST"), op("5", "GDEST")];
        // Recorded: sum of both operation rows = 100_000_000 stroops (10 XLM).
        // Before the fix, `discrepancy` was called once per row with 50_000_000
        // stroops each, so both rows reported 50_000_000 missing.
        let recorded_sum: i64 = 100_000_000; // 5 XLM + 5 XLM
        assert!(
            discrepancy(&payment(), "h1", recorded_sum, &ops).is_none(),
            "a correctly credited two-op transaction must not be flagged"
        );
    }

    /// A transaction recorded under the old single-row scheme (one row with the
    /// full amount) must still be reported as under-credited when less was
    /// recorded than on-chain.
    #[test]
    fn old_single_row_key_undercredit_still_reported() {
        // On-chain: 20 XLM.
        let ops = [op("20", "GDEST")];
        // Old single-row recorded only 10 XLM.
        let d = discrepancy(&payment(), "h1", 100_000_000, &ops).unwrap();
        assert_eq!(d.on_chain_stroops, 200_000_000);
        assert_eq!(d.missing_stroops, 100_000_000);
    }

    /// The `discrepancy` function's grouping contract: when two rows for the
    /// same (payment_id, tx_hash) are summed correctly, an exact match against
    /// on-chain produces no discrepancy.
    #[test]
    fn summed_rows_exact_match_no_discrepancy() {
        // On-chain: 3 XLM total (two ops: 1 XLM + 2 XLM).
        let ops = [op("1", "GDEST"), op("2", "GDEST")];
        let on_chain_total = on_chain_total(&payment(), &ops);
        assert_eq!(on_chain_total, 30_000_000);
        // Recorded sum = on-chain total → no discrepancy.
        assert!(discrepancy(&payment(), "h1", on_chain_total, &ops).is_none());
    }
}