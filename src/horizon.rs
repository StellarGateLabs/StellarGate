//! Stellar Horizon integration: detecting and verifying on-chain payments.
//!
//! A background poller pages forward through the gateway account's payments
//! from a persisted cursor, matches each record against pending payment intents
//! by transaction memo, verifies the asset and amount, and transitions the
//! intent to `completed` (or `failed` on underpayment), firing a webhook either
//! way. Because it pages forward from where it last left off — rather than
//! re-scanning a fixed newest-first window — no intent is skipped no matter how
//! many payments land between cycles, and a restart resumes from the saved
//! cursor instead of "now".
//!
//! The matching logic in [`verify`] is pure and unit-tested; the networked
//! [`fetch_payments_page`] and [`run_poller`] wrap it with I/O.

use crate::{db, money, webhook, AppState};
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};

/// Key under which the last fully-processed Horizon paging token is stored in
/// the `kv_state` table, so polling resumes from it across restarts.
const PAYMENT_CURSOR_KEY: &str = "horizon_payment_cursor";

/// How many payment records to request per Horizon page while catching up.
const PAGE_LIMIT: u32 = 200;

/// A single payment operation as returned by Horizon, with the embedded
/// transaction (requested via `join=transactions`) so we can read its memo.
#[derive(Debug, Clone, Deserialize)]
pub struct HorizonPayment {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub amount: Option<String>,
    #[serde(default)]
    pub asset_type: Option<String>,
    #[serde(default)]
    pub asset_code: Option<String>,
    #[serde(default)]
    pub asset_issuer: Option<String>,
    #[serde(default)]
    pub to: Option<String>,
    #[serde(default)]
    pub transaction_hash: Option<String>,
    #[serde(default)]
    pub transaction: Option<TransactionRef>,
    /// Horizon's opaque paging cursor for this record. We persist the latest
    /// processed token so polling resumes from it instead of re-scanning.
    #[serde(default)]
    pub paging_token: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TransactionRef {
    #[serde(default)]
    pub memo: Option<String>,
    #[serde(default)]
    pub memo_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PaymentsPage {
    #[serde(rename = "_embedded")]
    embedded: Embedded,
}

#[derive(Debug, Deserialize)]
struct Embedded {
    records: Vec<HorizonPayment>,
}

/// The outcome of matching a Horizon payment against a pending intent.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Paid the full amount (or more) — mark the intent completed.
    Completed { tx_hash: String, paid_amount: String },
    /// Paid, but less than requested — mark the intent failed.
    Underpaid { tx_hash: String, paid_amount: String },
}

impl HorizonPayment {
    fn memo(&self) -> Option<&str> {
        self.transaction.as_ref().and_then(|t| t.memo.as_deref())
    }
}

/// Decide whether a Horizon payment satisfies a pending intent.
///
/// Returns `None` when the payment is unrelated (wrong type, destination, memo,
/// or asset). When it matches, returns whether the amount was sufficient.
pub fn verify(payment: &db::Payment, hp: &HorizonPayment, usdc_issuer: &str) -> Option<Verdict> {
    if hp.kind != "payment" {
        return None;
    }
    if hp.to.as_deref() != Some(payment.destination_address.as_str()) {
        return None;
    }
    if hp.memo() != Some(payment.memo.as_str()) {
        return None;
    }

    let asset_matches = match payment.asset.as_str() {
        "XLM" => hp.asset_type.as_deref() == Some("native"),
        "USDC" => {
            hp.asset_code.as_deref() == Some("USDC")
                && hp.asset_issuer.as_deref() == Some(usdc_issuer)
        }
        _ => false,
    };
    if !asset_matches {
        return None;
    }

    let raw_amount = hp.amount.as_deref()?;
    let paid = money::parse_stroops(raw_amount)?;
    let expected = money::parse_stroops(&payment.amount)?;
    let tx_hash = hp.transaction_hash.clone().unwrap_or_default();

    if paid >= expected {
        Some(Verdict::Completed {
            tx_hash,
            paid_amount: raw_amount.to_string(),
        })
    } else {
        Some(Verdict::Underpaid {
            tx_hash,
            paid_amount: raw_amount.to_string(),
        })
    }
}

/// Fetch one page of payments into `account` from Horizon in ascending
/// (oldest-first) order starting strictly after `cursor`, with transactions
/// joined so memos are available. A `cursor` of `"0"` starts from the account's
/// first payment.
pub async fn fetch_payments_page(
    client: &reqwest::Client,
    horizon_url: &str,
    account: &str,
    cursor: &str,
    limit: u32,
) -> anyhow::Result<Vec<HorizonPayment>> {
    let url = format!(
        "{}/accounts/{}/payments?order=asc&cursor={}&limit={}&join=transactions",
        horizon_url.trim_end_matches('/'),
        account,
        cursor,
        limit
    );
    let page: PaymentsPage = client
        .get(&url)
        .header("Accept", "application/json")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(page.embedded.records)
}

/// Resolve the cursor this cycle should start paging from.
///
/// On the very first run (no persisted cursor) we baseline at the account's
/// most recent payment so we don't replay its entire history; from then on we
/// resume from the saved token. If the account has no payments yet, we start
/// from `"0"` so the first payment that ever arrives is still captured.
async fn starting_cursor(state: &Arc<AppState>) -> anyhow::Result<String> {
    if let Some(cursor) = db::get_state(&state.pool, PAYMENT_CURSOR_KEY).await? {
        return Ok(cursor);
    }

    let url = format!(
        "{}/accounts/{}/payments?order=desc&limit=1",
        state.config.horizon_url.trim_end_matches('/'),
        state.config.gateway_public,
    );
    let page: PaymentsPage = state
        .http
        .get(&url)
        .header("Accept", "application/json")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    match page.embedded.records.first().and_then(|p| p.paging_token.clone()) {
        Some(token) => {
            // Persist immediately so a crash before the first page still leaves
            // us baselined rather than replaying history next time.
            db::set_state(&state.pool, PAYMENT_CURSOR_KEY, &token).await?;
            info!(cursor = %token, "Horizon poller baselined at latest payment");
            Ok(token)
        }
        None => Ok("0".to_string()),
    }
}

/// Run one poll cycle: page forward from the persisted cursor through every
/// payment that has landed since, settling any that satisfy a pending intent,
/// until caught up. The cursor is advanced (and persisted) only after a page's
/// records have been processed, so no record is ever skipped and a restart
/// resumes exactly where it left off. Safe to call repeatedly; re-seeing an
/// already-settled record is a no-op (its intent is no longer pending).
pub async fn poll_once(state: &Arc<AppState>) -> anyhow::Result<usize> {
    let mut cursor = starting_cursor(state).await?;
    let mut settled = 0;

    loop {
        let page = fetch_payments_page(
            &state.http,
            &state.config.horizon_url,
            &state.config.gateway_public,
            &cursor,
            PAGE_LIMIT,
        )
        .await?;

        if page.is_empty() {
            break;
        }
        let count = page.len();

        for hp in &page {
            settled += process_record(state, hp).await? as usize;
            // Advance the in-memory cursor past this processed record.
            if let Some(token) = hp.paging_token.as_deref() {
                cursor = token.to_string();
            }
        }

        // Checkpoint after the whole page is processed. If we crash mid-page the
        // cursor still points at the last fully-processed page, and re-reading
        // the unfinished page is harmless (settled intents are skipped).
        db::set_state(&state.pool, PAYMENT_CURSOR_KEY, &cursor).await?;

        // A short page means Horizon has nothing newer — we're caught up.
        if count < PAGE_LIMIT as usize {
            break;
        }
    }

    Ok(settled)
}

/// Match a single Horizon record against the pending intent with its memo and
/// settle it if the payment satisfies the intent. Returns whether an intent was
/// settled. Records with no memo, no matching pending intent, or that fail
/// verification are ignored.
async fn process_record(state: &Arc<AppState>, hp: &HorizonPayment) -> anyhow::Result<bool> {
    let Some(memo) = hp.memo() else {
        return Ok(false);
    };
    // Look the intent up fresh per record so an intent created mid-cycle is
    // still matched, and one already settled this cycle is not touched again.
    let Some(payment) = db::find_pending_by_memo(&state.pool, memo).await? else {
        return Ok(false);
    };

    match verify(&payment, hp, &state.config.usdc_issuer) {
        Some(Verdict::Completed {
            tx_hash,
            paid_amount,
        }) => {
            settle(state, &payment, "completed", &tx_hash, &paid_amount, "payment.success").await;
            Ok(true)
        }
        Some(Verdict::Underpaid {
            tx_hash,
            paid_amount,
        }) => {
            warn!(payment_id = %payment.id, expected = %payment.amount, paid = %paid_amount, "underpayment");
            settle(state, &payment, "failed", &tx_hash, &paid_amount, "payment.failed").await;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// Persist a terminal status for `payment` and fire its webhook.
async fn settle(
    state: &Arc<AppState>,
    payment: &db::Payment,
    status: &str,
    tx_hash: &str,
    paid_amount: &str,
    event: &str,
) {
    if let Err(e) =
        db::update_payment_status(&state.pool, &payment.id, status, tx_hash, paid_amount).await
    {
        warn!(payment_id = %payment.id, error = %e, "failed to update payment status");
        return;
    }
    info!(payment_id = %payment.id, status, %tx_hash, "payment settled");

    // Reflect the new state in the copy we hand to the webhook.
    let mut settled = payment.clone();
    settled.status = status.to_string();
    settled.tx_hash = Some(tx_hash.to_string());
    settled.paid_amount = Some(paid_amount.to_string());
    webhook::dispatch(state, &settled, event).await;
}

/// Background loop that polls Horizon on the configured interval until the
/// process shuts down. Idles (without polling) while no gateway is configured.
pub async fn run_poller(state: Arc<AppState>) {
    if !state.config.gateway_configured() {
        warn!("STELLAR_GATEWAY_PUBLIC is unconfigured; Horizon poller disabled");
        return;
    }

    let interval = Duration::from_secs(state.config.poll_interval_secs.max(1));
    info!(
        account = %state.config.gateway_public,
        interval_secs = state.config.poll_interval_secs,
        "Horizon poller started"
    );

    loop {
        tokio::time::sleep(interval).await;
        match poll_once(&state).await {
            Ok(0) => debug!("poll: nothing to settle"),
            Ok(n) => info!(settled = n, "poll cycle settled payments"),
            Err(e) => warn!(error = %e, "poll cycle failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending(asset: &str, amount: &str) -> db::Payment {
        db::Payment {
            id: "id-1".into(),
            merchant_id: "m".into(),
            destination_address: "GGATEWAY".into(),
            memo: "MEMO1234".into(),
            amount: amount.into(),
            asset: asset.into(),
            status: "pending".into(),
            webhook_url: None,
            tx_hash: None,
            paid_amount: None,
            created_at: "now".into(),
            updated_at: "now".into(),
        }
    }

    fn native_payment(amount: &str, memo: &str, to: &str) -> HorizonPayment {
        HorizonPayment {
            kind: "payment".into(),
            amount: Some(amount.into()),
            asset_type: Some("native".into()),
            asset_code: None,
            asset_issuer: None,
            to: Some(to.into()),
            transaction_hash: Some("TXHASH".into()),
            transaction: Some(TransactionRef {
                memo: Some(memo.into()),
                memo_type: Some("text".into()),
            }),
            paging_token: Some("1".into()),
        }
    }

    const USDC_ISSUER: &str = "GUSDC";

    #[test]
    fn exact_xlm_payment_completes() {
        let p = pending("XLM", "10.00");
        let hp = native_payment("10.0000000", "MEMO1234", "GGATEWAY");
        assert_eq!(
            verify(&p, &hp, USDC_ISSUER),
            Some(Verdict::Completed {
                tx_hash: "TXHASH".into(),
                paid_amount: "10.0000000".into(),
            })
        );
    }

    #[test]
    fn overpayment_completes() {
        let p = pending("XLM", "10");
        let hp = native_payment("12.5", "MEMO1234", "GGATEWAY");
        assert!(matches!(
            verify(&p, &hp, USDC_ISSUER),
            Some(Verdict::Completed { .. })
        ));
    }

    #[test]
    fn underpayment_fails() {
        let p = pending("XLM", "10");
        let hp = native_payment("9.9999999", "MEMO1234", "GGATEWAY");
        assert!(matches!(
            verify(&p, &hp, USDC_ISSUER),
            Some(Verdict::Underpaid { .. })
        ));
    }

    #[test]
    fn wrong_memo_is_ignored() {
        let p = pending("XLM", "10");
        let hp = native_payment("10", "OTHER", "GGATEWAY");
        assert_eq!(verify(&p, &hp, USDC_ISSUER), None);
    }

    #[test]
    fn wrong_destination_is_ignored() {
        let p = pending("XLM", "10");
        let hp = native_payment("10", "MEMO1234", "GSOMEONEELSE");
        assert_eq!(verify(&p, &hp, USDC_ISSUER), None);
    }

    #[test]
    fn xlm_intent_rejects_usdc_payment() {
        let p = pending("XLM", "10");
        let mut hp = native_payment("10", "MEMO1234", "GGATEWAY");
        hp.asset_type = Some("credit_alphanum4".into());
        hp.asset_code = Some("USDC".into());
        hp.asset_issuer = Some(USDC_ISSUER.into());
        assert_eq!(verify(&p, &hp, USDC_ISSUER), None);
    }

    #[test]
    fn usdc_payment_with_correct_issuer_completes() {
        let p = pending("USDC", "5");
        let hp = HorizonPayment {
            kind: "payment".into(),
            amount: Some("5.0".into()),
            asset_type: Some("credit_alphanum4".into()),
            asset_code: Some("USDC".into()),
            asset_issuer: Some(USDC_ISSUER.into()),
            to: Some("GGATEWAY".into()),
            transaction_hash: Some("TXHASH".into()),
            transaction: Some(TransactionRef {
                memo: Some("MEMO1234".into()),
                memo_type: Some("text".into()),
            }),
            paging_token: Some("1".into()),
        };
        assert!(matches!(
            verify(&p, &hp, USDC_ISSUER),
            Some(Verdict::Completed { .. })
        ));
    }

    #[test]
    fn usdc_payment_with_wrong_issuer_is_ignored() {
        let p = pending("USDC", "5");
        let mut hp = HorizonPayment {
            kind: "payment".into(),
            amount: Some("5.0".into()),
            asset_type: Some("credit_alphanum4".into()),
            asset_code: Some("USDC".into()),
            asset_issuer: Some("GFAKEISSUER".into()),
            to: Some("GGATEWAY".into()),
            transaction_hash: Some("TXHASH".into()),
            transaction: Some(TransactionRef {
                memo: Some("MEMO1234".into()),
                memo_type: Some("text".into()),
            }),
            paging_token: Some("1".into()),
        };
        assert_eq!(verify(&p, &hp, USDC_ISSUER), None);
        // Sanity: with the right issuer it would have matched.
        hp.asset_issuer = Some(USDC_ISSUER.into());
        assert!(verify(&p, &hp, USDC_ISSUER).is_some());
    }

    #[test]
    fn non_payment_operation_is_ignored() {
        let p = pending("XLM", "10");
        let mut hp = native_payment("10", "MEMO1234", "GGATEWAY");
        hp.kind = "create_account".into();
        assert_eq!(verify(&p, &hp, USDC_ISSUER), None);
    }

    #[test]
    fn deserializes_horizon_payments_page() {
        let body = r#"{
            "_embedded": { "records": [
                {
                    "type": "payment",
                    "amount": "10.0000000",
                    "asset_type": "native",
                    "to": "GGATEWAY",
                    "transaction_hash": "abc",
                    "paging_token": "123456789-1",
                    "transaction": { "memo": "MEMO1234", "memo_type": "text" }
                }
            ]}
        }"#;
        let page: PaymentsPage = serde_json::from_str(body).unwrap();
        assert_eq!(page.embedded.records.len(), 1);
        assert_eq!(page.embedded.records[0].memo(), Some("MEMO1234"));
        assert_eq!(
            page.embedded.records[0].paging_token.as_deref(),
            Some("123456789-1")
        );
    }
}
