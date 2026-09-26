//! One-off audit for intents under-credited by the multi-operation bug.
//!
//! `processed_transactions` is keyed on `(payment_id, tx_hash)`, so a single
//! transaction carrying several payment operations to the gateway for the same
//! intent used to be credited only for its first operation; the rest were
//! dropped as "already processed". This module compares, per recorded
//! transaction, the amount we credited with the sum of every matching payment
//! operation Horizon reports for that transaction, and lists the shortfalls.
//!
//! The audit is strictly read-only: it reports affected intents and the missing
//! amount so an operator can correct merchant balances deliberately.

use crate::{db, horizon, money};
use serde::Serialize;
use sqlx::Row;

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

/// Scan every `processed_transactions` row against Horizon and return the
/// under-credited ones.
pub async fn find_under_credited(
    pool: &db::Db,
    http: &reqwest::Client,
    horizon_url: &str,
) -> anyhow::Result<Vec<Discrepancy>> {
    let rows = sqlx::query(
        "SELECT pt.payment_id, pt.tx_hash, pt.amount_stroops
           FROM processed_transactions pt
           JOIN payments p ON p.id = pt.payment_id
          ORDER BY pt.created_at",
    )
    .fetch_all(pool)
    .await?;

    let mut out = Vec::new();
    for row in rows {
        let payment_id: String = row.get("payment_id");
        let tx_hash: String = row.get("tx_hash");
        let recorded: i64 = row.get("amount_stroops");
        let Some(payment) = db::get_payment(pool, &payment_id).await? else {
            continue;
        };
        let ops = horizon::fetch_transaction_payments(http, horizon_url, &tx_hash).await?;
        out.extend(discrepancy(&payment, &tx_hash, recorded, &ops));
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
}
