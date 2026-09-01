//! Stellar Horizon integration: detecting and verifying on-chain payments.
//!
//! A background poller periodically asks Horizon for the most recent payments
//! into the gateway account, matches them against pending payment intents by
//! transaction memo, verifies the asset and amount, and transitions the intent
//! to a terminal or watchable state, firing a webhook in each case.
//!
//! ## Payment resolution policy
//!
//! | Scenario | DB status | Webhook event | Notes |
//! |---|---|---|---|
//! | Paid exactly the requested amount | `completed` | `payment.completed` | — |
//! | Paid **more** than requested | `completed` | `payment.overpaid` | `delta` = excess; merchant should refund |
//! | Paid **less** than requested | `underpaid` | `payment.underpaid` | `delta` = shortfall; intent stays watchable |
//! | Top-up brings total to exactly expected | `completed` | `payment.completed` | — |
//! | Top-up brings total above expected | `completed` | `payment.overpaid` | `delta` = cumulative excess |
//! | Payment after intent is `completed` or `expired` | unchanged | `payment.unexpected` | `delta` = the unexpected amount; merchant must refund |
//!
//! Once an intent reaches `completed` or `expired`, its status is never
//! changed. A subsequent on-chain payment to the same address and memo fires a
//! `payment.unexpected` webhook carrying the amount so the merchant can arrange
//! a refund — the gateway is the only component that can see such a payment.
//!
//! Multiple follow-up (top-up) payments are supported per underpaid intent.
//! Every processed transaction is recorded in the `processed_transactions`
//! join table, and the cumulative received amount is the SUM over that set, so
//! re-seeing a transaction (on a later poll cycle, over the stream, or from a
//! concurrent reconciler) never double-counts and the ledger is independent of
//! the order records arrive in. The payment row's `tx_hash` still records the
//! most recent processed transaction for display.
//!
//! ## Restart behaviour
//!
//! Both listeners resume from a cursor persisted in `kv_state`, under separate
//! keys ([`PAYMENT_CURSOR_KEY`] and [`STREAM_CURSOR_KEY`]) so neither drags the
//! other backwards. Only a database with no cursor under either key starts at
//! the live edge. The poller additionally checks its shutdown signal at every
//! page boundary — immediately after checkpointing — so a `SIGTERM` during a
//! long catch-up is honoured within one page instead of being ignored until the
//! whole backlog drains (issues #226, #228).
//!
//! ## Finality
//!
//! A payment only settles an intent when its joined transaction reports
//! `successful: true`. Matching on type/destination/memo/asset/amount is not
//! sufficient for money movement — a failed, replaced, or reorg-orphaned
//! transaction can carry all the right fields yet move no funds. We therefore
//! require the `successful` flag explicitly (records are always fetched with
//! `join=transactions`, so it is present) rather than relying on the implicit
//! and undocumented behaviour that Horizon's payments-for-account endpoint
//! tends to surface only successful operations.
//!
//! The matching logic in [`verify`] is pure and unit-tested; the networked
//! functions wrap it with I/O.

use crate::{db, money, webhook, AppState};
use futures_util::StreamExt;
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use tokio::sync::watch;
use tracing::{debug, info, warn};

/// Key under which the last fully-processed Horizon paging token is stored in
/// the `kv_state` table, so polling resumes from it across restarts.
const PAYMENT_CURSOR_KEY: &str = "horizon_payment_cursor";

/// Key under which the stream listener persists the paging token of the last
/// event it handled. Deliberately separate from [`PAYMENT_CURSOR_KEY`]: the two
/// listeners advance at different rates, and a shared key would let whichever
/// one wrote last drag the other backwards or forwards (issue #228).
const STREAM_CURSOR_KEY: &str = "horizon_stream_cursor";

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
    /// RFC 3339 timestamp of the operation (ledger close time), used to
    /// measure how far behind the poller/stream cursor is running.
    #[serde(default)]
    pub created_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TransactionRef {
    #[serde(default)]
    pub memo: Option<String>,
    #[serde(default)]
    pub memo_type: Option<String>,
    /// Whether the enclosing transaction succeeded on-chain. Horizon populates
    /// this on every transaction record; we treat a missing value as *not*
    /// known-successful and refuse to settle against it (see
    /// [`HorizonPayment::is_successful`]).
    #[serde(default)]
    pub successful: Option<bool>,
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

/// The gateway account as returned by Horizon's `/accounts/{id}` endpoint. We
/// only care about its balance lines, which double as its trustlines: a
/// non-native asset appears here only if the account trusts that issuer.
#[derive(Debug, Deserialize)]
struct AccountResponse {
    #[serde(default)]
    balances: Vec<AccountBalance>,
}

/// One balance / trustline line on a Stellar account.
///
/// `is_authorized` can be `false` even when the balance line exists if the
/// asset's issuer uses `AUTH_REQUIRED` and has not yet granted (or has since
/// revoked) authorization. An unauthorized trustline cannot receive payments —
/// a deposit attempt bounces on-chain just like a missing trustline would.
///
/// `limit` is the maximum number of units this account will accept for the
/// asset (in the same decimal format as `balance`). A payment that would push
/// `balance` past `limit` also fails on-chain.
#[derive(Debug, Clone, Deserialize)]
pub struct AccountBalance {
    #[serde(default)]
    pub asset_type: Option<String>,
    #[serde(default)]
    pub asset_code: Option<String>,
    #[serde(default)]
    pub asset_issuer: Option<String>,
    #[serde(default)]
    pub balance: Option<String>,
    /// Whether the account is authorized to hold this asset.
    /// Always `true` for native XLM. `false` means the issuer has not
    /// granted (or has revoked) authorization; the trustline is present
    /// but unusable for incoming payments.
    #[serde(default = "default_true")]
    pub is_authorized: bool,
    /// Maximum units the account will accept for this asset.
    /// `"922337203685.4775807"` is the Stellar maximum (i64::MAX stroops).
    #[serde(default)]
    pub limit: Option<String>,
}

/// Default value for `is_authorized` when the field is absent in the JSON
/// (e.g. native XLM, which Horizon omits it for). Native XLM is always
/// implicitly authorized, so this default must be `true` rather than `false`.
fn default_true() -> bool {
    true
}

/// The outcome of matching a Horizon payment against a pending intent.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Cumulative paid amount equals the requested amount exactly.
    Completed {
        tx_hash: String,
        paid_amount: String,
    },
    /// Cumulative paid amount exceeds the requested amount.
    /// The intent is fulfilled; `delta` is the excess the merchant should refund.
    Overpaid {
        tx_hash: String,
        paid_amount: String,
    },
    /// Cumulative paid amount is still below the requested amount.
    /// The intent remains open; `delta` is the shortfall still owed.
    Underpaid {
        tx_hash: String,
        paid_amount: String,
    },
}

impl HorizonPayment {
    /// The transaction's memo, but only when Horizon reports it as `memo_type:
    /// "text"`.
    ///
    /// We generate text memos exclusively (see [`crate::api::payments`]).
    /// Stellar transactions can instead carry a `memo_id` (u64) or
    /// `memo_hash`/`memo_return` (32-byte) memo, and Horizon still populates
    /// the JSON `memo` field for those — as a decimal string or base64,
    /// respectively. A `memo_id` consisting only of digits could coincide
    /// with one of our hex memos as plain text, so the type must be checked;
    /// otherwise an unrelated `memo_id` payment could be mistaken for one of
    /// ours.
    fn memo(&self) -> Option<&str> {
        let t = self.transaction.as_ref()?;
        if t.memo_type.as_deref() != Some("text") {
            return None;
        }
        t.memo.as_deref()
    }

    /// Whether the transaction that carried this payment is known to have
    /// succeeded on-chain.
    ///
    /// Horizon's payments-for-account endpoint generally returns operations
    /// from successful transactions, but that is an implementation detail we
    /// must not rely on for money movement: a failed, replaced, or
    /// reorg-orphaned transaction must never settle an intent. We therefore
    /// require the joined transaction to explicitly report `successful: true`.
    /// A missing flag (e.g. a record fetched without `join=transactions`, or a
    /// future/altered payload) is treated as not-successful and rejected.
    fn is_successful(&self) -> bool {
        self.transaction.as_ref().and_then(|t| t.successful) == Some(true)
    }
}

/// Seconds elapsed between an RFC 3339 timestamp and now. Used to observe
/// cursor age (how stale the poller/stream cursor is) and settlement latency
/// (how long an intent took to settle). Returns `None` if `ts` doesn't parse.
fn elapsed_secs(ts: &str) -> Option<i64> {
    let then = OffsetDateTime::parse(ts, &Rfc3339).ok()?;
    Some((OffsetDateTime::now_utc() - then).whole_seconds())
}

/// Parsed stroop amounts for a Horizon payment that matched an intent's
/// destination/memo/asset, restored (from commit 8daebd2) for use by
/// [`reconcile_post_terminal_payment`], which — unlike [`verify`] — checks a
/// payment against an already-terminal intent using only the fields recorded
/// on that intent (no `accepted_assets` allow-list lookup, since a terminal
/// intent's priced asset/issuer is fixed regardless of today's configuration).
#[derive(Debug, Clone, Copy)]
struct IntentMatch {
    new_stroops: i64,
}

/// Return the parsed amount when a Horizon payment belongs to this intent.
/// Unrelated, unsuccessful, wrong-destination/memo/asset, or malformed records
/// return `None` and must not enter the authoritative processed-transaction
/// ledger. Mirrors the match checks in [`verify`], but against a (typically
/// terminal) intent's own recorded `asset`/`asset_issuer` rather than the
/// current `accepted_assets` allow-list.
fn matches_intent(payment: &db::Payment, hp: &HorizonPayment) -> Option<IntentMatch> {
    if hp.kind != "payment" {
        return None;
    }
    if hp.to.as_deref() != Some(payment.destination_address.as_str()) {
        return None;
    }
    if hp.memo() != Some(payment.memo.as_str()) {
        return None;
    }
    /* Only settle against a transaction Horizon reports as successful. Matching
    on type/destination/memo/asset/amount is not enough for money movement: a
    failed or reorg-orphaned transaction can carry all the right fields yet
    never have moved funds. See [`HorizonPayment::is_successful`] for the
    finality assumptions this encodes. */
    if !hp.is_successful() {
        return None;
    }

    /* Match the issuer this intent was priced in — not any allow-list entry
    that happens to share the code. Two `USDC` issuers used to both settle an
    intent that stored only the code (issue #222). */
    let asset_matches = match payment.asset_issuer.as_deref().filter(|s| !s.is_empty()) {
        None => {
            payment.asset.eq_ignore_ascii_case("XLM") && hp.asset_type.as_deref() == Some("native")
        }
        Some(issuer) => {
            hp.asset_code.as_deref() == Some(payment.asset.as_str())
                && hp.asset_issuer.as_deref() == Some(issuer)
        }
    };
    if !asset_matches {
        return None;
    }

    Some(IntentMatch {
        new_stroops: money::parse_stroops(hp.amount.as_deref()?)?,
    })
}

/// Decide whether a Horizon payment satisfies a pending intent.
///
/// `already_paid_stroops` is the cumulative amount already received for this
/// intent (0 for a fresh `pending` payment, non-zero for an `underpaid` one).
///
/// Returns `None` when the payment is unrelated (wrong type, destination, memo,
/// or asset). When it matches, returns the verdict for the cumulative total.
pub fn verify(
    payment: &db::Payment,
    hp: &HorizonPayment,
    accepted_assets: &[crate::config::AcceptedAsset],
    already_paid_stroops: i64,
) -> Option<Verdict> {
    if hp.kind != "payment" {
        return None;
    }
    if hp.to.as_deref() != Some(payment.destination_address.as_str()) {
        return None;
    }
    if hp.memo() != Some(payment.memo.as_str()) {
        return None;
    }
    /* Only settle against a transaction Horizon reports as successful. Matching
    on type/destination/memo/asset/amount is not enough for money movement: a
    failed or reorg-orphaned transaction can carry all the right fields yet
    never have moved funds. See [`HorizonPayment::is_successful`] for the
    finality assumptions this encodes. */
    if !hp.is_successful() {
        return None;
    }

    /* The asset must still be one the gateway accepts, but *which* issuer
    counts comes from the intent, not from today's configuration: an intent
    priced in one issuer's USDC must not become payable in another's because
    `ACCEPTED_ASSETS` was edited after it was created (issue #223). Rows written
    before `asset_issuer` existed carry `None`, and fall back to the configured
    issuer — the same behaviour they had before. */
    let asset_matches = accepted_assets.iter().any(|a| {
        if a.code != payment.asset {
            return false;
        }
        match payment.asset_issuer.as_deref().or(a.issuer.as_deref()) {
            None => hp.asset_type.as_deref() == Some("native"),
            Some(issuer) => {
                hp.asset_code.as_deref() == Some(a.code.as_str())
                    && hp.asset_issuer.as_deref() == Some(issuer)
            }
        }
    });
    if !asset_matches {
        return None;
    }

    let raw_amount = hp.amount.as_deref()?;
    let new_paid = money::parse_stroops(raw_amount)?;
    let expected = money::parse_stroops(&payment.amount)?;
    let total_paid = already_paid_stroops + new_paid;
    let tx_hash = hp.transaction_hash.clone().unwrap_or_default();
    let paid_amount = money::stroops_to_string(total_paid);

    use std::cmp::Ordering;
    match total_paid.cmp(&expected) {
        Ordering::Equal => Some(Verdict::Completed {
            tx_hash,
            paid_amount,
        }),
        Ordering::Greater => Some(Verdict::Overpaid {
            tx_hash,
            paid_amount,
        }),
        Ordering::Less => Some(Verdict::Underpaid {
            tx_hash,
            paid_amount,
        }),
    }
}

/// A Horizon HTTP failure, distinguishing throttling (`429 Too Many Requests`
/// or `503 Service Unavailable`, carrying `Retry-After` when Horizon sends
/// one) from any other failure. Before this, `fetch_recent_payments` turned
/// every non-2xx response into an opaque `reqwest::Error` via
/// `error_for_status()`, discarding the status code and headers — the poller
/// had no way to tell "back off, and for how long" from an ordinary blip
/// (issue #313).
#[derive(Debug)]
pub struct HorizonHttpError {
    pub status: reqwest::StatusCode,
    pub retry_after: Option<Duration>,
    body: String,
}

impl std::fmt::Display for HorizonHttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Horizon returned {}: {}", self.status, self.body)
    }
}

impl std::error::Error for HorizonHttpError {}

impl HorizonHttpError {
    /// `429`/`503` mean "back off", not "something is broken" — every other
    /// non-2xx status is treated as an ordinary failure.
    pub fn is_rate_limited(&self) -> bool {
        matches!(
            self.status,
            reqwest::StatusCode::TOO_MANY_REQUESTS | reqwest::StatusCode::SERVICE_UNAVAILABLE
        )
    }
}

/// Parse `Retry-After` as delta-seconds (RFC 9110) — the form every rate
/// limiter this service talks to actually sends, including Horizon's. The
/// HTTP-date form is not handled: nothing observed in the wild sends it for
/// `429`/`503`, and misparsing a date as a much smaller or larger delay would
/// be worse than falling back to the exponential backoff below.
fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

/// Build a `.../accounts/{account}/payments` URL with properly
/// percent-encoded query parameters.
///
/// A Horizon paging token is opaque and may contain `&`, `#`, or other
/// characters that are significant in a query string — interpolating one
/// directly into a hand-built URL (as every call site here used to) corrupts
/// the request the moment such a token shows up, silently truncating the
/// cursor or attaching stray parameters.
fn payments_url(
    horizon_url: &str,
    account: &str,
    order: Option<&str>,
    cursor: Option<&str>,
    limit: Option<u32>,
    join_transactions: bool,
) -> anyhow::Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(horizon_url)
        .map_err(|e| anyhow::anyhow!("invalid Horizon URL {horizon_url:?}: {e}"))?;
    {
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| anyhow::anyhow!("Horizon URL cannot be used as a path base"))?;
        segments.pop_if_empty();
        segments.extend(["accounts", account, "payments"]);
    }
    {
        let mut pairs = url.query_pairs_mut();
        if let Some(order) = order {
            pairs.append_pair("order", order);
        }
        if let Some(cursor) = cursor {
            pairs.append_pair("cursor", cursor);
        }
        if let Some(limit) = limit {
            pairs.append_pair("limit", &limit.to_string());
        }
        if join_transactions {
            pairs.append_pair("join", "transactions");
        }
    }
    Ok(url)
}

/// Fetch the most recent payments into `account` from Horizon, newest first,
/// with their transactions joined so memos are available.
///
/// A non-2xx response is returned as a [`HorizonHttpError`] rather than
/// `error_for_status`'s opaque `reqwest::Error`, so a caller can tell a `429`/
/// `503` — and any `Retry-After` Horizon attached — apart from an ordinary
/// failure (issue #313).
pub async fn fetch_recent_payments(
    client: &reqwest::Client,
    horizon_url: &str,
    account: &str,
    cursor: &str,
    limit: u32,
) -> anyhow::Result<Vec<HorizonPayment>> {
    let url = payments_url(horizon_url, account, Some("asc"), Some(cursor), Some(limit), true)?;
    let resp = client
        .get(url)
        .header("Accept", "application/json")
        .send()
        .await?;

    let status = resp.status();
    if !status.is_success() {
        let retry_after = parse_retry_after(resp.headers());
        let body = resp.text().await.unwrap_or_default();
        return Err(HorizonHttpError {
            status,
            retry_after,
            body,
        }
        .into());
    }

    let page: PaymentsPage = resp.json().await?;
    Ok(page.embedded.records)
}

/// Return the accepted assets the gateway account holds **no usable** trustline
/// for.
///
/// A trustline is considered missing (i.e. the asset cannot be received) when
/// any of the following is true:
/// - No balance line exists for the asset at all.
/// - The balance line exists but `is_authorized` is `false` (the issuer uses
///   `AUTH_REQUIRED` and has revoked or not yet granted authorization).
///
/// Native XLM never needs a trustline, so it is always considered held and
/// authorized. Pure, so it is unit-tested without any network.
pub fn missing_trustlines<'a>(
    accepted_assets: &'a [crate::config::AcceptedAsset],
    balances: &[AccountBalance],
) -> Vec<&'a crate::config::AcceptedAsset> {
    accepted_assets
        .iter()
        .filter(|asset| match asset.issuer.as_deref() {
            // Native asset — no trustline required.
            None => false,
            Some(issuer) => {
                match balances.iter().find(|b| {
                    b.asset_code.as_deref() == Some(asset.code.as_str())
                        && b.asset_issuer.as_deref() == Some(issuer)
                }) {
                    // Balance line absent — missing trustline.
                    None => true,
                    // Balance line present but not authorized — unusable.
                    Some(b) => !b.is_authorized,
                }
            }
        })
        .collect()
}

/// Parse a Stellar decimal amount string (e.g. `"1000.5000000"`) into stroops.
/// Returns `None` when `s` is `None` or unparseable.
fn parse_stroops_opt(s: Option<&str>) -> Option<i64> {
    money::parse_stroops(s?)
}

/// Return the remaining headroom (in stroops) for each accepted non-native
/// asset that has a trustline on the gateway account: `limit - balance`.
///
/// A payment that would push the balance past `limit` fails on-chain, so a
/// headroom approaching zero is an actionable signal. Returns only assets where
/// both `limit` and `balance` are parseable; assets with missing or
/// unparseable values are skipped (not treated as zero headroom).
pub fn trustline_headroom<'a>(
    accepted_assets: &'a [crate::config::AcceptedAsset],
    balances: &[AccountBalance],
) -> Vec<(&'a crate::config::AcceptedAsset, i64)> {
    accepted_assets
        .iter()
        .filter_map(|asset| {
            let issuer = asset.issuer.as_deref()?;
            let b = balances.iter().find(|b| {
                b.asset_code.as_deref() == Some(asset.code.as_str())
                    && b.asset_issuer.as_deref() == Some(issuer)
            })?;
            let limit = parse_stroops_opt(b.limit.as_deref())?;
            let balance = parse_stroops_opt(b.balance.as_deref())?;
            let headroom = limit.saturating_sub(balance);
            Some((asset, headroom))
        })
        .collect()
}

/// Build the Horizon `/accounts/{id}` URL for the given account.
///
/// Written fresh (rather than restored from history) because an earlier
/// history version of this helper took `&reqwest::Url`, which does not match
/// `Config::horizon_url`'s current `String` type or any current call site;
/// this mirrors the plain string-formatting the old inline `check_trustlines`
/// body used.
fn horizon_account_url(horizon_url: &str, account: &str) -> anyhow::Result<String> {
    Ok(format!(
        "{}/accounts/{}",
        horizon_url.trim_end_matches('/'),
        account,
    ))
}

/// Fetch the gateway account (balances/trustlines) from Horizon at `url`.
async fn fetch_account(state: &Arc<AppState>, url: String) -> anyhow::Result<AccountResponse> {
    let account: AccountResponse = state
        .http
        .get(&url)
        .header("Accept", "application/json")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(account)
}

/// Check that the gateway account holds a trustline for every accepted
/// non-native asset, and warn about any that are missing.
///
/// An accepted asset without a trustline mints unpayable intents: the gateway
/// advertises (say) USDC, a customer pays, and the payment bounces on-chain
/// because the account cannot receive it. Surfacing this turns a silent
/// runtime failure into an actionable warning.
///
/// Called both at boot and, from [`run_trustline_checker`], on a recurring
/// interval — a trustline can be revoked, or an asset added to
/// `ACCEPTED_ASSETS`, at any time after boot, so a boot-only check would go
/// stale the moment either happens. Every call — success or failure — updates
/// `state.trustline_metrics`, which is what `GET /metrics` and `POST
/// /payments` actually read; the return value exists for the boot-time log
/// line and tests.
///
/// Best-effort by design: a Horizon error (unreachable, account not yet
/// funded) is returned to the caller to log, but must not abort boot or the
/// periodic checker — the account may be provisioned shortly after start.
/// Such a failure bumps `stellargate_trustline_check_failures_total` and
/// leaves the per-asset gauge untouched, rather than reporting a guess.
/// Returns the list of accepted asset codes that are missing a trustline
/// (empty when all are present).
pub async fn check_trustlines(state: &Arc<AppState>) -> anyhow::Result<Vec<String>> {
    let url = horizon_account_url(&state.config.horizon_url, &state.config.gateway_public)?;
    let account = match fetch_account(state, url).await {
        Ok(account) => account,
        Err(e) => {
            state.trustline_metrics.record_check_failure();
            return Err(e);
        }
    };

    // Log the native XLM balance.
    if let Some(native_balance) = account
        .balances
        .iter()
        .find(|b| b.asset_type.as_deref() == Some("native"))
    {
        if let Some(amt) = &native_balance.balance {
            info!(
                balance = %amt,
                account = %state.config.gateway_public,
                "gateway account native XLM balance"
            );
        }
    }

    // Collect assets that are missing a trustline entirely OR have one but are
    // unauthorized. Both prevent incoming payments from settling.
    let missing = missing_trustlines(&state.config.accepted_assets, &account.balances);

    // Among the missing set, distinguish unauthorized trustlines (present but
    // unusable) from absent ones. Different root causes, different remedies.
    let unauthorized_codes: Vec<String> = state
        .config
        .accepted_assets
        .iter()
        .filter_map(|asset| {
            let issuer = asset.issuer.as_deref()?;
            // Only flag if the balance line actually exists but is not authorized.
            let b = account.balances.iter().find(|b| {
                b.asset_code.as_deref() == Some(asset.code.as_str())
                    && b.asset_issuer.as_deref() == Some(issuer)
            })?;
            if b.is_authorized {
                None
            } else {
                Some(asset.code.clone())
            }
        })
        .collect();

    if missing.is_empty() {
        info!("gateway trustlines verified for all accepted assets");
    } else {
        let missing_codes: Vec<_> = missing.iter().map(|a| a.code.clone()).collect();
        info!(
            missing = ?missing_codes,
            "accepted assets with no usable trustline on the gateway account"
        );
        for asset in &missing {
            if unauthorized_codes.iter().any(|c| c == &asset.code) {
                warn!(
                    asset = %asset.code,
                    issuer = %asset.issuer.as_deref().unwrap_or(""),
                    "gateway account trustline exists but is not authorized; \
                     intents in this asset will be unpayable until the issuer \
                     grants authorization"
                );
            } else {
                warn!(
                    asset = %asset.code,
                    issuer = %asset.issuer.as_deref().unwrap_or(""),
                    "gateway account has no trustline for an accepted asset; intents in \
                     this asset will be unpayable until a trustline is established"
                );
            }
        }
    }

    // Compute headroom (limit - balance in stroops) for alerting on capacity.
    let headroom = trustline_headroom(&state.config.accepted_assets, &account.balances);
    for (asset, stroops) in &headroom {
        // Warn when headroom is below ~10 XLM-equivalent (10_000_000 stroops)
        // in absolute terms — an early signal that a large inbound payment could
        // be rejected. The threshold is informational; the metric itself is the
        // authoritative, alertable signal.
        if *stroops < 10_000_000 {
            warn!(
                asset = %asset.code,
                headroom_stroops = %stroops,
                "trustline headroom is critically low; a large payment may be \
                 rejected on-chain before the gateway receives it"
            );
        }
    }

    let missing_codes: Vec<String> = missing.iter().map(|a| a.code.clone()).collect();
    let headroom_refs: Vec<(&str, i64)> = headroom
        .iter()
        .map(|(a, s)| (a.code.as_str(), *s))
        .collect();
    let checked_codes = state
        .config
        .accepted_assets
        .iter()
        .filter(|a| a.issuer.is_some())
        .map(|a| a.code.as_str());
    state.trustline_metrics.record_check_full(
        checked_codes,
        &missing_codes,
        &unauthorized_codes,
        &headroom_refs,
    );
    Ok(missing_codes)
}

/// Background task that re-runs [`check_trustlines`] on `RETENTION_INTERVAL_SECS`
/// for as long as the gateway wallet is configured — a trustline can be
/// revoked, or an asset added to `ACCEPTED_ASSETS`, at any time after boot, so
/// a boot-only check would go stale the moment either happens.
pub async fn run_trustline_checker(
    state: Arc<AppState>,
    mut shutdown: watch::Receiver<bool>,
) -> crate::supervise::TaskExit {
    if !state.config.gateway_configured() {
        return crate::supervise::TaskExit::DisabledByConfig(
            "STELLAR_GATEWAY_PUBLIC is unconfigured",
        );
    }

    let interval = Duration::from_secs(state.config.retention_interval_secs.max(1));
    info!(
        interval_secs = state.config.retention_interval_secs,
        "trustline checker started"
    );

    loop {
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = shutdown.changed() => {
                info!("trustline checker shutting down");
                return crate::supervise::TaskExit::ShutdownRequested;
            }
        }

        match check_trustlines(&state).await {
            Ok(missing) if missing.is_empty() => {
                debug!("trustline check: all accepted assets have a trustline")
            }
            Ok(missing) => info!(
                ?missing,
                "accepted assets with no trustline on the gateway account"
            ),
            Err(e) => warn!(error = %e, "could not verify gateway trustlines"),
        }
    }
}

/// How many pages [`starting_cursor`] will walk backward, at most, while
/// searching for a baseline that covers every currently open intent (issue
/// #311). Bounds the worst case — an account with a large payment history and
/// an old open intent — to a fixed number of Horizon requests at boot rather
/// than an unbounded backward scan. `MAX_BASELINE_PAGES * PAGE_LIMIT` (5,000
/// records) is the same order of magnitude as `poll_max_pages_per_cycle`'s
/// default per-cycle budget.
const MAX_BASELINE_PAGES: usize = 25;

/// Resolve the cursor this cycle should start paging from.
///
/// From the second run onward this simply resumes from the saved token. On
/// the very first run (no persisted cursor) it deliberately baselines with
/// overlap rather than adopting the account's single most recent payment
/// exactly, which used to skip two kinds of payment silently:
///
/// - **Reused account.** Pointing a fresh instance at an account that already
///   received payments — a redeploy after losing the volume, a migration
///   between hosts — would adopt whatever the account's newest payment
///   happened to be as the floor, hiding every payment for an intent created
///   before that point even though it is still open.
/// - **Startup race.** A payment that lands between Horizon answering the
///   baselining query and the first forward poll can sort at or below the
///   single-record baseline (e.g. read-replica lag on Horizon's side), and
///   was silently skipped.
///
/// Neither produces an error: the intent just stays `pending` until the
/// sweeper expires it, with no record connecting the customer's on-chain
/// payment to anything. Re-processing an already-settled transaction is a
/// no-op through `processed_transactions` (issue #78), so the cost of
/// over-scanning is a few wasted queries — cheap insurance against silently
/// under-scanning.
///
/// The baseline is chosen by paging backward (`order=desc`) from the tip
/// until the oldest record seen is older than every currently open intent's
/// `created_at` (a payment cannot be relevant to an intent it predates), or
/// until [`MAX_BASELINE_PAGES`] is reached, or until Horizon has no more
/// history. When nothing is currently open (a genuinely fresh deployment),
/// one page of backward overlap is taken anyway, purely to cover the startup
/// race. If the account has no payments at all, baselining starts from `"0"`
/// so the first payment that ever arrives is still captured.
async fn starting_cursor(state: &Arc<AppState>) -> anyhow::Result<String> {
    if let Some(cursor) = db::get_state(&state.pool, PAYMENT_CURSOR_KEY).await? {
        return Ok(cursor);
    }

    /* `list_pending` returns oldest-first, so the first row (if any) is the
    earliest `created_at` we must not scan past. A payment settling this
    intent cannot have landed before the intent existed. */
    let earliest_open = db::list_pending(&state.pool)
        .await?
        .into_iter()
        .next()
        .map(|p| p.created_at);

    let mut next_cursor: Option<String> = None;
    let mut skipped = 0usize;
    let mut pages = 0usize;

    let token = loop {
        let url = payments_url(
            &state.config.horizon_url,
            &state.config.gateway_public,
            Some("desc"),
            next_cursor.as_deref(),
            Some(PAGE_LIMIT),
            false,
        )?;
        let page: PaymentsPage = state
            .http
            .get(url)
            .header("Accept", "application/json")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        pages += 1;

        let Some(oldest) = page.embedded.records.last() else {
            /* Horizon has no more history behind us — we have walked all the
            way back to the account's first-ever payment (or it has none at
            all). Either way, "0" is the maximally safe baseline: nothing is
            skipped, at the cost of a full replay on the next forward poll. */
            return Ok("0".to_string());
        };
        skipped += page.embedded.records.len();
        let token = oldest.paging_token.clone().unwrap_or_default();
        next_cursor = Some(token.clone());

        let covers_every_open_intent = match &earliest_open {
            // Nothing open yet — one page of overlap is enough to cover the
            // startup race; there is no older business state to protect.
            None => true,
            /* Strictly older, not `<=`: the boundary record itself must be
            excluded from "covered", because the persisted cursor becomes the
            *exclusive* start of the next forward poll. Stopping at `<=` could
            land the cursor exactly on (or after) the earliest open intent's
            creation instant and exclude a payment that landed in the same
            instant — reintroducing the exact race this baselining exists to
            close. An unparseable or absent `created_at` is treated as "not
            proven older" so the walk keeps going rather than risk stopping
            too early. */
            Some(earliest) => oldest
                .created_at
                .as_deref()
                .map(|ts| ts < earliest.as_str())
                .unwrap_or(false),
        };

        if covers_every_open_intent {
            break token;
        }
        if pages >= MAX_BASELINE_PAGES {
            warn!(
                pages,
                skipped_records = skipped,
                "Horizon baseline walk hit its page cap before clearing every open \
                 intent's creation time; baselining with the overlap found so far"
            );
            break token;
        }
    };

    /* Persist immediately so a crash right after this scan still leaves us
    baselined rather than repeating a potentially multi-page walk next time. */
    db::set_state(&state.pool, PAYMENT_CURSOR_KEY, &token).await?;
    info!(
        cursor = %token,
        skipped_records = skipped,
        pages,
        "Horizon poller baselined with overlap"
    );
    Ok(token)
}

/// Run one poll cycle: page forward from the persisted cursor through every
/// payment that has landed since, settling any that satisfy a pending intent,
/// until caught up. The cursor is advanced (and persisted) only after a page's
/// records have been processed, so no record is ever skipped and a restart
/// resumes exactly where it left off. Safe to call repeatedly; re-seeing an
/// already-settled record is a no-op (its intent is no longer pending).
///
/// The cycle gives up its turn at a page boundary — always with the cursor
/// checkpointed first — in two cases (issue #226):
///
/// * `shutdown` has been signalled. A gateway that has been down for a while
///   faces a catch-up measured in minutes (each page is up to [`PAGE_LIMIT`]
///   records, each of which means DB writes and an outbound webhook), and
///   without this check none of it observes SIGTERM: the process would be
///   killed mid-page once the shutdown grace expired, replaying the unfinished
///   page on the next boot and making the *next* shutdown worse.
/// * `POLL_MAX_PAGES_PER_CYCLE` pages have been walked. Even with no shutdown
///   pending, one cycle must not monopolise the poller task indefinitely.
///
/// In both cases the remaining backlog is simply picked up by the next cycle
/// (or the next boot) from the checkpointed cursor.
pub async fn poll_once(
    state: &Arc<AppState>,
    shutdown: &watch::Receiver<bool>,
) -> anyhow::Result<usize> {
    let mut cursor = starting_cursor(state).await?;
    let mut settled = 0;
    let max_pages = state.config.poll_max_pages_per_cycle;
    let mut pages = 0u32;

    loop {
        /* Checked at the top of the iteration, i.e. after the previous page's
        checkpoint, so returning here never loses processed work. */
        if *shutdown.borrow() {
            info!(
                settled,
                pages, "poller shutting down mid-catch-up; cursor checkpointed"
            );
            return Ok(settled);
        }

        let page = fetch_recent_payments(
            &state.http,
            &state.config.horizon_url,
            &state.config.gateway_public,
            &cursor,
            PAGE_LIMIT,
        )
        .await?;
        let count = page.len();

        for hp in &page {
            if let Some(token) = &hp.paging_token {
                cursor = token.clone();
            }
            match reconcile_payment(state, hp).await {
                Ok(true) => settled += 1,
                Ok(false) => {}
                Err(e) => warn!(error = %e, "failed to reconcile polled payment"),
            }
        }

        if let Some(cursor_age_secs) = page
            .last()
            .and_then(|hp| hp.created_at.as_deref())
            .and_then(elapsed_secs)
        {
            info!(cursor_age_secs, "poller cursor advanced");
        }

        /* Checkpoint after the whole page is processed. If we crash mid-page the
        cursor still points at the last fully-processed page, and re-reading
        the unfinished page is harmless (settled intents are skipped). */
        db::set_state(&state.pool, PAYMENT_CURSOR_KEY, &cursor).await?;

        // A short page means Horizon has nothing newer — we're caught up.
        if count < PAGE_LIMIT as usize {
            break;
        }

        /* Yield the task once the per-cycle page budget is spent. `0` means
        unlimited, for operators who would rather one cycle ran to completion. */
        pages += 1;
        if max_pages > 0 && pages >= max_pages {
            info!(
                pages,
                settled, "poll cycle hit its page budget; resuming from the checkpoint next cycle"
            );
            break;
        }
    }

    Ok(settled)
}

/// Look up the pending intent matching this Horizon payment by memo, verify it,
/// and settle it if it matches. Returns `true` when an intent was settled.
///
/// When the memo matches a terminal (completed/expired) intent, the transaction
/// is recorded and a `payment.unexpected` webhook is fired — the merchant needs
/// to know so they can refund the funds. The intent's terminal status is never
/// mutated (issue #232).
///
/// This is intentionally `pub` so integration tests can drive concurrent
/// reconciliations to verify the single-settlement guarantee (issue #155).
pub async fn reconcile_payment(state: &Arc<AppState>, hp: &HorizonPayment) -> anyhow::Result<bool> {
    let memo = match hp.memo() {
        Some(m) => m,
        None => return Ok(false),
    };

    // Fast path: find a still-active (pending/underpaid) intent.
    if let Some(payment) = db::find_pending_by_memo(&state.pool, memo).await? {
        return reconcile_active_payment(state, hp, payment).await;
    }

    // Slow path: the memo matched no active intent. Check whether it matches a
    // terminal one — a customer who pays after completion/expiry has lost real
    // funds that the gateway is the only component that can see (issue #232).
    if let Some(payment) = db::find_by_memo_any_status(&state.pool, memo).await? {
        // Only act on terminal intents (completed/expired). An underpaid intent
        // that somehow slipped through the active check (extremely unlikely) is
        // simply left alone.
        if payment.status == "completed" || payment.status == "expired" {
            reconcile_post_terminal_payment(state, hp, &payment).await?;
        }
    }

    Ok(false)
}

/// Handle a Horizon payment that matches an **active** (pending/underpaid)
/// intent — the normal settlement path.
async fn reconcile_active_payment(
    state: &Arc<AppState>,
    hp: &HorizonPayment,
    payment: db::Payment,
) -> anyhow::Result<bool> {
    let hp_hash = hp.transaction_hash.as_deref().unwrap_or("");

    /* Cumulative amount already received for this intent — 0 for a fresh
    `pending` payment, non-zero for an `underpaid` one topping up. */
    let already_paid_stroops = payment
        .paid_amount
        .as_deref()
        .and_then(money::parse_stroops)
        .unwrap_or(0);

    /* Gate on a real, matching, on-chain payment before recording anything, so
    unrelated traffic never pollutes the ledger. `verify` returns `None` for
    anything that does not satisfy this intent (wrong type/destination/memo/
    asset, or an unparseable amount). */
    if verify(
        &payment,
        hp,
        &state.config.accepted_assets,
        already_paid_stroops,
    )
    .is_none()
    {
        return Ok(false);
    }

    /* Record this transaction idempotently. If it was already credited — seen
    on an earlier poll cycle, redelivered over the stream, or racing a
    concurrent reconciler — the insert is a no-op and we must not settle again.
    This makes re-processing any past transaction a no-op regardless of the
    order records arrive in (issue #119). */
    let new_stroops = hp
        .amount
        .as_deref()
        .and_then(money::parse_stroops)
        .unwrap_or(0);
    if !db::record_processed_tx(&state.pool, &payment.id, hp_hash, new_stroops).await? {
        return Ok(false);
    }

    /* Re-sum over the recorded set (now including this transaction) so the
    persisted `paid_amount` always reflects every processed transaction. */
    let total_stroops = db::sum_processed_stroops(&state.pool, &payment.id).await?;
    let expected_stroops = money::parse_stroops(&payment.amount).unwrap_or(0);
    let paid_amount = money::stroops_to_string(total_stroops);

    use std::cmp::Ordering;
    let (status, event, delta) = match total_stroops.cmp(&expected_stroops) {
        Ordering::Equal => ("completed", "payment.completed", None),
        Ordering::Greater => {
            let excess = money::stroops_to_string(total_stroops - expected_stroops);
            info!(
                payment_id = %payment.id,
                excess = %excess,
                "overpayment — intent completed, excess should be refunded"
            );
            ("completed", "payment.overpaid", Some(excess))
        }
        Ordering::Less => {
            let remaining = money::stroops_to_string(expected_stroops - total_stroops);
            warn!(
                payment_id = %payment.id,
                expected = %payment.amount,
                paid = %paid_amount,
                remaining = %remaining,
                "underpayment — intent remains open for a top-up"
            );
            ("underpaid", "payment.underpaid", Some(remaining))
        }
    };

    let did_settle = settle(
        state,
        &payment,
        status,
        hp_hash,
        &paid_amount,
        event,
        delta.as_deref(),
    )
    .await;
    Ok(did_settle)
}

/// Handle a Horizon payment that matches a **terminal** (completed/expired)
/// intent (issue #232).
///
/// The intent's status is NOT changed — the terminal state is authoritative.
/// The transaction is recorded in `processed_transactions` to avoid reprocessing
/// it on subsequent poll cycles, and a `payment.unexpected` webhook is fired so
/// the merchant knows funds arrived and can arrange a refund.
async fn reconcile_post_terminal_payment(
    state: &Arc<AppState>,
    hp: &HorizonPayment,
    payment: &db::Payment,
) -> anyhow::Result<()> {
    let hp_hash = hp.transaction_hash.as_deref().unwrap_or("");

    /* Verify asset and amount match the intent before treating this as a
    meaningful post-terminal payment. An unrelated transaction to the same
    address with a colliding memo should not fire a webhook. */
    let matched = match matches_intent(payment, hp) {
        Some(m) => m,
        None => return Ok(()),
    };

    /* Record idempotently so a re-seen transaction fires no duplicate webhook.
    If this hash is already present the payment was already handled; skip. */
    if !db::record_processed_tx(&state.pool, &payment.id, hp_hash, matched.new_stroops).await? {
        return Ok(());
    }

    let amount_str = money::stroops_to_string(matched.new_stroops);
    warn!(
        payment_id = %payment.id,
        prior_status = %payment.status,
        tx_hash = %hp_hash,
        amount = %amount_str,
        "unexpected payment received for a terminal intent; \
         merchant must arrange a refund"
    );

    /* Fire the webhook so the merchant can act. The intent copy passed to
    dispatch carries the original terminal status — it is not mutated. The
    `delta` field carries the unexpected amount so the merchant knows exactly
    how much to refund. */
    webhook::dispatch(state, payment, "payment.unexpected", Some(&amount_str)).await;
    Ok(())
}

/// Persist a terminal or intermediate status for `payment` and fire its webhook.
/// Returns `true` when the row was actually updated (i.e. a settlement was
/// committed); returns `false` when the status guard rejected the update because
/// a concurrent reconciler already settled the intent.
///
/// Callers must propagate this return value so `reconcile_payment` can report
/// accurately whether it settled an intent, which is what the concurrency test
/// asserts (issue #155).
async fn settle(
    state: &Arc<AppState>,
    payment: &db::Payment,
    status: &str,
    tx_hash: &str,
    paid_amount: &str,
    event: &str,
    delta: Option<&str>,
) -> bool {
    match db::update_payment_status(&state.pool, &payment.id, status, tx_hash, paid_amount).await {
        Err(e) => {
            warn!(payment_id = %payment.id, error = %e, "failed to update payment status");
            return false;
        }
        Ok(false) => {
            // A concurrent reconciler already settled this intent — skip the webhook.
            debug!(
                payment_id = %payment.id,
                "skipping duplicate settlement (status guard rejected update)"
            );
            return false;
        }
        Ok(true) => {}
    }
    let settlement_latency_secs = elapsed_secs(&payment.created_at);
    info!(
        payment_id = %payment.id,
        status,
        %tx_hash,
        ?settlement_latency_secs,
        "payment settled"
    );

    // Reflect the new state in the copy we hand to the webhook.
    let mut settled = payment.clone();
    settled.status = status.to_string();
    settled.tx_hash = Some(tx_hash.to_string());
    settled.paid_amount = Some(paid_amount.to_string());
    /* Delivered inline, deliberately: the common case settles and notifies in
    one pass with no added latency. This does mean a slow receiver stalls this
    poll cycle for the duration of all inline retry attempts — the redrive
    worker is the safety net for the crash case, not a replacement for inline
    delivery. See issue #76 for the separate task of decoupling dispatch. */
    webhook::dispatch(state, &settled, event, delta).await;
    true
}

/// Floor and cap for the poller's failure backoff. Reuses the same
/// equal-jitter schedule (`webhook::retry_delay`) the webhook redrive worker
/// backs off with (issue #318) — the shape solves the same problem here:
/// growth so repeated failures stop hammering Horizon at the configured poll
/// interval, jitter so it doesn't retry in lockstep with itself every cycle.
const POLL_BACKOFF_BASE: Duration = Duration::from_secs(1);
const POLL_BACKOFF_MAX: Duration = Duration::from_secs(120);

/// Choose how long to wait before the poller's next attempt, given this
/// cycle's outcome (issue #313).
///
/// A `429`/`503` that carried `Retry-After` is honored exactly — Horizon told
/// us how long to wait, and second-guessing it with our own backoff could
/// still be too short. Every other failure (including a rate limit with no
/// `Retry-After`) falls back to the exponential-with-jitter schedule, keyed
/// on `consecutive_failures` so it keeps growing across repeated failures
/// instead of resetting each cycle.
fn next_poll_delay(consecutive_failures: u32, retry_after: Option<Duration>) -> Duration {
    retry_after.unwrap_or_else(|| {
        webhook::retry_delay(consecutive_failures, POLL_BACKOFF_BASE, POLL_BACKOFF_MAX)
    })
}

/// Background loop that polls Horizon on the configured interval until the
/// process shuts down. Idles (without polling) while no gateway is configured.
///
/// A failed cycle no longer waits out the fixed `POLL_INTERVAL_SECS` before
/// trying again at the same rate that likely caused it (issue #313): a `429`
/// backs off for at least `Retry-After`, and repeated failures of any kind
/// back off exponentially with jitter, both reset to the configured interval
/// by the next success.
pub async fn run_poller(state: Arc<AppState>, mut shutdown: watch::Receiver<bool>) {
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

    let mut consecutive_failures: u32 = 0;
    let mut next_delay = interval;

    loop {
        tokio::select! {
            _ = tokio::time::sleep(next_delay) => {}
            _ = shutdown.changed() => {
                info!("Horizon poller shutting down");
                return;
            }
        }

        match poll_once(&state, &shutdown).await {
            Ok(n) => {
                if n == 0 {
                    debug!("poll: nothing to settle");
                } else {
                    info!(settled = n, "poll cycle settled payments");
                }
                state.horizon_metrics.record_success();
                consecutive_failures = 0;
                next_delay = interval;
            }
            Err(e) => {
                consecutive_failures = consecutive_failures.saturating_add(1);
                let horizon_err = e.downcast_ref::<HorizonHttpError>();
                let rate_limited = horizon_err.is_some_and(|he| he.is_rate_limited());
                let retry_after = horizon_err.and_then(|he| he.retry_after);

                if rate_limited {
                    state.horizon_metrics.record_rate_limited();
                } else {
                    state.horizon_metrics.record_error();
                }

                next_delay = next_poll_delay(consecutive_failures, retry_after);
                warn!(
                    error = %e,
                    rate_limited,
                    retry_after_secs = retry_after.map(|d| d.as_secs()),
                    consecutive_failures,
                    next_delay_secs = next_delay.as_secs(),
                    "poll cycle failed"
                );
            }
        }
    }
}

/// One parsed Server-Sent-Events block: the fields of a single `\n\n`-delimited
/// event. See <https://html.spec.whatwg.org/multipage/server-sent-events.html>.
#[derive(Debug, Default, PartialEq, Eq)]
struct SseEvent {
    /// The `event:` name, if any (Horizon uses `open` for the greeting).
    event: Option<String>,
    /// The `id:` field — Horizon sets it to the paging token, which we reuse as
    /// the reconnect cursor so no payments are missed across a dropped stream.
    id: Option<String>,
    /// The concatenated `data:` lines.
    data: String,
}

/// Parse one SSE event block (the text between blank-line delimiters) into its
/// fields. Comment lines (`:`...) and unrecognised fields such as `retry:` are
/// ignored. Pure, so it is unit-tested without any network.
fn parse_sse_block(block: &str) -> SseEvent {
    let mut ev = SseEvent::default();
    let mut data_lines: Vec<&str> = Vec::new();
    for line in block.lines() {
        /* Per the spec a value has its single leading space (if present)
        stripped after the colon. */
        if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.strip_prefix(' ').unwrap_or(rest));
        } else if let Some(rest) = line.strip_prefix("event:") {
            ev.event = Some(rest.strip_prefix(' ').unwrap_or(rest).to_string());
        } else if let Some(rest) = line.strip_prefix("id:") {
            ev.id = Some(rest.strip_prefix(' ').unwrap_or(rest).to_string());
        }
    }
    ev.data = data_lines.join("\n");
    ev
}

/// Background task that subscribes to Horizon's payment SSE stream for the
/// gateway account and settles matching intents as records arrive. Reconnects
/// automatically with exponential backoff, resuming from the last seen cursor
/// so no payments are missed across a dropped connection. Idles (without
/// connecting) while no gateway is configured.
pub async fn run_stream_listener(state: Arc<AppState>, mut shutdown: watch::Receiver<bool>) {
    if !state.config.gateway_configured() {
        warn!("STELLAR_GATEWAY_PUBLIC is unconfigured; Horizon stream listener disabled");
        return;
    }

    info!(account = %state.config.gateway_public, "Horizon payment stream listener started");

    /* A dedicated client without the shared client's overall request timeout —
    the SSE connection is long-lived and must not be cut off mid-stream. */
    let client = match reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .user_agent(concat!("StellarGate/", env!("CARGO_PKG_VERSION")))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "failed to build stream HTTP client; stream listener disabled");
            return;
        }
    };

    let base_backoff = Duration::from_secs(1);
    let max_backoff = Duration::from_secs(30);
    let mut backoff = base_backoff;
    let mut cursor = stream_starting_cursor(&state).await;
    let idle_timeout = Duration::from_secs(state.config.stream_idle_timeout_secs);
    let mut first_connection = true;

    loop {
        if !first_connection {
            /* Every pass through the loop body after the first is a reconnect —
            whether the previous connection closed cleanly, errored, or (issue
            #312) went idle past `idle_timeout` with no error at all. A
            persistently-reconnecting stream is the alertable signal that a
            half-open connection keeps disabling live payment detection. */
            state.horizon_metrics.record_stream_reconnect();
        }
        first_connection = false;

        let cursor_before = cursor.clone();
        tokio::select! {
            result = stream_once(&state, &client, &mut cursor, idle_timeout) => {
                match result {
                    Ok(()) => debug!("Horizon stream closed by server; reconnecting"),
                    Err(e) => warn!(error = %e, "Horizon stream dropped; reconnecting"),
                }
            }
            _ = shutdown.changed() => {
                info!("Horizon stream listener shutting down");
                return;
            }
        }

        if cursor != cursor_before {
            backoff = base_backoff;
        }

        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = shutdown.changed() => {
                info!("Horizon stream listener shutting down");
                return;
            }
        }
        backoff = (backoff * 2).min(max_backoff);
    }
}

/// Resolve the cursor the stream should subscribe from.
///
/// Preference order (issue #228):
///
/// 1. The stream's own persisted cursor, so a restart resumes exactly where the
///    last connection left off.
/// 2. The poller's cursor, for a database written before the stream persisted
///    one of its own — still far better than the live edge.
/// 3. `"now"`, only when neither exists (a genuinely fresh deployment), which
///    is the same baseline the poller takes on its first run.
///
/// Re-seeing records the poller has already reconciled is harmless: settlement
/// is idempotent through `processed_transactions`.
async fn stream_starting_cursor(state: &Arc<AppState>) -> String {
    for key in [STREAM_CURSOR_KEY, PAYMENT_CURSOR_KEY] {
        match db::get_state(&state.pool, key).await {
            Ok(Some(cursor)) => {
                info!(cursor = %cursor, source = key, "stream listener resuming from persisted cursor");
                return cursor;
            }
            Ok(None) => {}
            /* A read failure must not take the listener down; baseline at the
            live edge and let the poller cover the gap. */
            Err(e) => warn!(error = %e, key, "could not read persisted stream cursor"),
        }
    }
    info!("no persisted cursor; stream listener baselining at the live edge");
    "now".to_string()
}

/// Open one SSE connection and process events until the stream ends, errors,
/// or goes `idle_timeout` without delivering a single byte. Advances `cursor`
/// to the latest event `id` so a reconnect resumes cleanly.
///
/// The dedicated stream client (built by [`run_stream_listener`]) carries no
/// overall request timeout — the connection is meant to live indefinitely —
/// so nothing else bounds a half-open socket that stops delivering bytes
/// without closing: a NAT or load balancer dropping idle state without
/// sending `RST`, or an upstream stall. Horizon sends periodic keep-alive
/// comment lines on its SSE endpoints, so an idle window with no bytes at all
/// is a reliable liveness signal (issue #312): every await on the next chunk
/// is itself bounded by `idle_timeout`, and running past it is reported as an
/// error so the caller's existing reconnect-with-backoff path picks it up
/// exactly as it would a dropped connection.
async fn stream_once(
    state: &Arc<AppState>,
    client: &reqwest::Client,
    cursor: &mut String,
    idle_timeout: Duration,
) -> anyhow::Result<()> {
    let url = payments_url(
        &state.config.horizon_url,
        &state.config.gateway_public,
        None,
        Some(cursor),
        None,
        true,
    )?;

    let resp = client
        .get(url)
        .header("Accept", "text/event-stream")
        .send()
        .await?
        .error_for_status()?;

    let mut stream = resp.bytes_stream();
    /* Accumulate raw bytes (not lossily-decoded str) so multibyte characters
    split across chunk boundaries are never corrupted. */
    let mut buf: Vec<u8> = Vec::new();

    loop {
        let chunk = match tokio::time::timeout(idle_timeout, stream.next()).await {
            Err(_) => {
                return Err(anyhow::anyhow!(
                    "no data received on Horizon stream for {idle_timeout:?}; \
                     treating the connection as dead"
                ));
            }
            Ok(None) => break,
            Ok(Some(chunk)) => chunk?,
        };
        buf.extend_from_slice(&chunk);

        /* Dispatch every complete event (terminated by a blank line) in the
        buffer, leaving any partial trailing event for the next chunk. */
        while let Some(end) = find_event_end(&buf) {
            let block: Vec<u8> = buf.drain(..end).collect();
            let text = String::from_utf8_lossy(&block);
            handle_stream_event(state, &text, cursor).await;
        }
    }

    Ok(())
}

/// Find the byte index just past the first event delimiter in `buf`, i.e. the
/// number of leading bytes that form one complete event plus its terminator.
/// SSE events are separated by a blank line — `\n\n` (LF) or `\r\n\r\n` (CRLF).
/// Returns `None` if no event is complete yet.
fn find_event_end(buf: &[u8]) -> Option<usize> {
    let lf = buf.windows(2).position(|w| w == b"\n\n").map(|i| i + 2);
    let crlf = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4);
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Parse one streamed event block and, when it carries a payment record, run it
/// through the shared reconciliation path. Non-payment frames (Horizon's `open`
/// greeting, keep-alives) are ignored.
async fn handle_stream_event(state: &Arc<AppState>, block: &str, cursor: &mut String) {
    let ev = parse_sse_block(block);

    /* Advance the reconnect cursor as soon as we learn a newer event id, and
    persist it so a restart resumes here rather than re-baselining at the live
    edge (issue #228). Stored under the stream's own key so it never fights
    with the poller's cursor. A write failure is logged and tolerated: the
    in-memory cursor still covers reconnects within this process, and the
    poller remains the backstop across restarts. */
    if let Some(id) = ev.id {
        if id != *cursor {
            if let Err(e) = db::set_state(&state.pool, STREAM_CURSOR_KEY, &id).await {
                warn!(error = %e, "failed to persist stream cursor");
            }
            *cursor = id;
        }
    }

    if ev.event.as_deref() == Some("open") || ev.data.is_empty() {
        return;
    }

    match serde_json::from_str::<HorizonPayment>(&ev.data) {
        Ok(hp) => {
            if let Some(cursor_age_secs) = hp.created_at.as_deref().and_then(elapsed_secs) {
                info!(cursor_age_secs, "stream cursor advanced");
            }
            if let Err(e) = reconcile_payment(state, &hp).await {
                warn!(error = %e, "failed to reconcile streamed payment");
            }
        }
        // The greeting payload (`"hello"`) and any non-record frames land here.
        Err(e) => debug!(error = %e, "ignoring non-payment stream frame"),
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
            asset_issuer: None,
            status: "pending".into(),
            webhook_url: None,
            tx_hash: None,
            paid_amount: None,
            created_at: "now".into(),
            updated_at: "now".into(),
            expires_at: "later".into(),
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
                successful: Some(true),
            }),
            paging_token: Some("1".into()),
            created_at: None,
        }
    }

    fn test_assets() -> Vec<crate::config::AcceptedAsset> {
        vec![
            crate::config::AcceptedAsset {
                code: "XLM".into(),
                issuer: None,
            },
            crate::config::AcceptedAsset {
                code: "USDC".into(),
                issuer: Some("GUSDC".into()),
            },
        ]
    }

    #[test]
    fn exact_xlm_payment_completes() {
        let p = pending("XLM", "10.00");
        let hp = native_payment("10.0000000", "MEMO1234", "GGATEWAY");
        assert_eq!(
            verify(&p, &hp, &test_assets(), 0),
            Some(Verdict::Completed {
                tx_hash: "TXHASH".into(),
                paid_amount: "10".into(),
            })
        );
    }

    #[test]
    fn overpayment_yields_overpaid_verdict() {
        let p = pending("XLM", "10");
        let hp = native_payment("12.5", "MEMO1234", "GGATEWAY");
        assert_eq!(
            verify(&p, &hp, &test_assets(), 0),
            Some(Verdict::Overpaid {
                tx_hash: "TXHASH".into(),
                paid_amount: "12.5".into(),
            })
        );
    }

    #[test]
    fn underpayment_yields_underpaid_verdict() {
        let p = pending("XLM", "10");
        let hp = native_payment("9.9999999", "MEMO1234", "GGATEWAY");
        assert_eq!(
            verify(&p, &hp, &test_assets(), 0),
            Some(Verdict::Underpaid {
                tx_hash: "TXHASH".into(),
                paid_amount: "9.9999999".into(),
            })
        );
    }

    #[test]
    fn topup_completing_underpaid_intent() {
        // First payment: 3 of 5 XLM — underpaid.
        let p = pending("XLM", "5");
        let hp1 = native_payment("3.0000000", "MEMO1234", "GGATEWAY");
        assert!(matches!(
            verify(&p, &hp1, &test_assets(), 0),
            Some(Verdict::Underpaid { .. })
        ));

        // Top-up: 2 XLM arrives; cumulative = 5 = expected — completes exactly.
        let hp2 = native_payment("2.0000000", "MEMO1234", "GGATEWAY");
        assert_eq!(
            verify(&p, &hp2, &test_assets(), 30_000_000),
            Some(Verdict::Completed {
                tx_hash: "TXHASH".into(),
                paid_amount: "5".into(),
            })
        );
    }

    #[test]
    fn topup_overpaying_underpaid_intent() {
        // First payment: 3 of 5 XLM — underpaid.
        let p = pending("XLM", "5");
        // Top-up of 3 XLM; cumulative = 6 > 5 — overpaid.
        let hp = native_payment("3.0000000", "MEMO1234", "GGATEWAY");
        assert_eq!(
            verify(&p, &hp, &test_assets(), 30_000_000),
            Some(Verdict::Overpaid {
                tx_hash: "TXHASH".into(),
                paid_amount: "6".into(),
            })
        );
    }

    #[test]
    fn wrong_memo_is_ignored() {
        let p = pending("XLM", "10");
        let hp = native_payment("10", "OTHER", "GGATEWAY");
        assert_eq!(verify(&p, &hp, &test_assets(), 0), None);
    }

    /// A `memo_id`/`memo_hash`/`memo_return` transaction that happens to
    /// render the same characters as one of our hex memos must never be
    /// mistaken for a match — only `memo_type: "text"` counts.
    #[test]
    fn non_text_memo_type_is_ignored_even_if_value_matches() {
        let p = pending("XLM", "10");
        let mut hp = native_payment("10", "MEMO1234", "GGATEWAY");
        hp.transaction.as_mut().unwrap().memo_type = Some("id".into());
        assert_eq!(verify(&p, &hp, &test_assets(), 0), None);
    }

    #[test]
    fn missing_memo_type_is_ignored() {
        let p = pending("XLM", "10");
        let mut hp = native_payment("10", "MEMO1234", "GGATEWAY");
        hp.transaction.as_mut().unwrap().memo_type = None;
        assert_eq!(verify(&p, &hp, &test_assets(), 0), None);
    }

    #[test]
    fn wrong_destination_is_ignored() {
        let p = pending("XLM", "10");
        let hp = native_payment("10", "MEMO1234", "GSOMEONEELSE");
        assert_eq!(verify(&p, &hp, &test_assets(), 0), None);
    }

    #[test]
    fn xlm_intent_rejects_usdc_payment() {
        let p = pending("XLM", "10");
        let mut hp = native_payment("10", "MEMO1234", "GGATEWAY");
        hp.asset_type = Some("credit_alphanum4".into());
        hp.asset_code = Some("USDC".into());
        hp.asset_issuer = Some("GUSDC".into());
        assert_eq!(verify(&p, &hp, &test_assets(), 0), None);
    }

    #[test]
    fn usdc_payment_with_correct_issuer_completes() {
        let p = pending("USDC", "5");
        let hp = HorizonPayment {
            kind: "payment".into(),
            amount: Some("5.0".into()),
            asset_type: Some("credit_alphanum4".into()),
            asset_code: Some("USDC".into()),
            asset_issuer: Some("GUSDC".into()),
            to: Some("GGATEWAY".into()),
            transaction_hash: Some("TXHASH".into()),
            transaction: Some(TransactionRef {
                memo: Some("MEMO1234".into()),
                memo_type: Some("text".into()),
                successful: Some(true),
            }),
            paging_token: Some("1".into()),
            created_at: None,
        };
        assert!(matches!(
            verify(&p, &hp, &test_assets(), 0),
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
                successful: Some(true),
            }),
            paging_token: Some("1".into()),
            created_at: None,
        };
        assert_eq!(verify(&p, &hp, &test_assets(), 0), None);
        // Sanity: with the right issuer it would have matched.
        hp.asset_issuer = Some("GUSDC".into());
        assert!(verify(&p, &hp, &test_assets(), 0).is_some());
    }

    #[test]
    fn non_payment_operation_is_ignored() {
        let p = pending("XLM", "10");
        let mut hp = native_payment("10", "MEMO1234", "GGATEWAY");
        hp.kind = "create_account".into();
        assert_eq!(verify(&p, &hp, &test_assets(), 0), None);
    }

    /// A transaction Horizon reports as `successful: false` must never settle an
    /// intent, even when type/destination/memo/asset/amount all match.
    #[test]
    fn failed_transaction_is_ignored() {
        let p = pending("XLM", "10");
        let mut hp = native_payment("10.0000000", "MEMO1234", "GGATEWAY");
        hp.transaction.as_mut().unwrap().successful = Some(false);
        assert_eq!(verify(&p, &hp, &test_assets(), 0), None);
        // Sanity: the same record with `successful: true` would have completed.
        hp.transaction.as_mut().unwrap().successful = Some(true);
        assert!(matches!(
            verify(&p, &hp, &test_assets(), 0),
            Some(Verdict::Completed { .. })
        ));
    }

    /// A record whose joined transaction omits the `successful` flag is treated
    /// as not-known-successful and rejected — we never settle on an absent flag.
    #[test]
    fn missing_successful_flag_is_ignored() {
        let p = pending("XLM", "10");
        let mut hp = native_payment("10.0000000", "MEMO1234", "GGATEWAY");
        hp.transaction.as_mut().unwrap().successful = None;
        assert_eq!(verify(&p, &hp, &test_assets(), 0), None);
    }

    fn native_balance() -> AccountBalance {
        AccountBalance {
            asset_type: Some("native".into()),
            asset_code: None,
            asset_issuer: None,
            balance: Some("100.0000000".into()),
            is_authorized: true,
            limit: None,
        }
    }

    fn issued_balance(code: &str, issuer: &str) -> AccountBalance {
        AccountBalance {
            asset_type: Some("credit_alphanum4".into()),
            asset_code: Some(code.into()),
            asset_issuer: Some(issuer.into()),
            balance: Some("0.0000000".into()),
            is_authorized: true,
            limit: Some("922337203685.4775807".into()),
        }
    }

    fn unauthorized_balance(code: &str, issuer: &str) -> AccountBalance {
        AccountBalance {
            is_authorized: false,
            ..issued_balance(code, issuer)
        }
    }

    #[test]
    fn missing_trustlines_flags_untrusted_issued_asset() {
        // Accepts XLM (native) and USDC:GUSDC, but the account holds only XLM.
        let assets = test_assets();
        let missing = missing_trustlines(&assets, &[native_balance()]);
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].code, "USDC");
    }

    #[test]
    fn missing_trustlines_none_when_all_assets_trusted() {
        let balances = [native_balance(), issued_balance("USDC", "GUSDC")];
        assert!(missing_trustlines(&test_assets(), &balances).is_empty());
    }

    #[test]
    fn missing_trustlines_requires_the_matching_issuer() {
        // Right code, wrong issuer — the trustline is still considered missing.
        let assets = test_assets();
        let balances = [issued_balance("USDC", "GWRONGISSUER")];
        let missing = missing_trustlines(&assets, &balances);
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].code, "USDC");
    }

    #[test]
    fn missing_trustlines_flags_unauthorized_trustline() {
        // Trustline exists but is_authorized=false — treated as missing.
        let assets = test_assets();
        let balances = [native_balance(), unauthorized_balance("USDC", "GUSDC")];
        let missing = missing_trustlines(&assets, &balances);
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].code, "USDC");
    }

    #[test]
    fn missing_trustlines_authorized_trustline_not_flagged() {
        // is_authorized=true (the default): not flagged as missing.
        let assets = test_assets();
        let balances = [native_balance(), issued_balance("USDC", "GUSDC")];
        assert!(missing_trustlines(&assets, &balances).is_empty());
    }

    #[test]
    fn trustline_headroom_computes_limit_minus_balance() {
        let assets = test_assets();
        // 1000 limit, 300 balance → 700 XLM headroom = 7_000_000_000 stroops
        let b = AccountBalance {
            asset_type: Some("credit_alphanum4".into()),
            asset_code: Some("USDC".into()),
            asset_issuer: Some("GUSDC".into()),
            balance: Some("300.0000000".into()),
            is_authorized: true,
            limit: Some("1000.0000000".into()),
        };
        let headroom = trustline_headroom(&assets, &[b]);
        assert_eq!(headroom.len(), 1);
        assert_eq!(headroom[0].0.code, "USDC");
        // 700 * 10_000_000 stroops = 7_000_000_000
        assert_eq!(headroom[0].1, 7_000_000_000);
    }

    #[test]
    fn missing_trustlines_never_flags_native_xlm() {
        // An XLM-only gateway with no balance lines at all still needs no
        // trustline for its native asset.
        let assets = [crate::config::AcceptedAsset {
            code: "XLM".into(),
            issuer: None,
        }];
        assert!(missing_trustlines(&assets, &[]).is_empty());
    }

    #[test]
    fn parses_payment_sse_event() {
        let block = "id: 123456789\nevent: \ndata: {\"type\":\"payment\",\"amount\":\"10.0\"}";
        let ev = parse_sse_block(block);
        assert_eq!(ev.id.as_deref(), Some("123456789"));
        assert_eq!(ev.data, "{\"type\":\"payment\",\"amount\":\"10.0\"}");
    }

    #[test]
    fn parses_open_greeting_event() {
        let block = "retry: 1000\nevent: open\ndata: \"hello\"";
        let ev = parse_sse_block(block);
        assert_eq!(ev.event.as_deref(), Some("open"));
        assert_eq!(ev.data, "\"hello\"");
        // The greeting payload is not a payment record.
        assert!(serde_json::from_str::<HorizonPayment>(&ev.data).is_err());
    }

    #[test]
    fn joins_multiline_sse_data_and_ignores_comments() {
        let block = ": keep-alive\ndata: {\"type\":\ndata: \"payment\"}\nid: 99";
        let ev = parse_sse_block(block);
        assert_eq!(ev.data, "{\"type\":\n\"payment\"}");
        assert_eq!(ev.id.as_deref(), Some("99"));
    }

    #[test]
    fn streamed_payment_deserializes_into_verifiable_record() {
        /* A single Horizon payment record as pushed over SSE (note: a streamed
        record carries its memo inline under `transaction`, same as the page). */
        let data = r#"{
            "type": "payment",
            "amount": "10.0000000",
            "asset_type": "native",
            "to": "GGATEWAY",
            "transaction_hash": "abc",
            "transaction": { "memo": "MEMO1234", "memo_type": "text", "successful": true }
        }"#;
        let hp: HorizonPayment = serde_json::from_str(data).unwrap();
        let p = pending("XLM", "10.00");
        assert!(matches!(
            verify(&p, &hp, &test_assets(), 0),
            Some(Verdict::Completed { .. })
        ));
    }

    #[test]
    fn find_event_end_detects_complete_events() {
        assert_eq!(find_event_end(b"data: x\n\nrest"), Some(9));
        assert_eq!(find_event_end(b"data: x\r\n\r\nrest"), Some(11));
        assert_eq!(find_event_end(b"data: partial\n"), None);
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
