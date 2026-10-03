/* Pure formatting helpers for the StellarGate dashboard.
 *
 * This module is deliberately DOM-free: every function here is a pure
 * transformation of its arguments, so the whole file can be exercised under
 * `node --test` without a browser (issue #677). The dashboard controller in
 * `dashboard.js` imports these helpers rather than defining them inline.
 */

/** Placeholder for absent/empty values. */
export var EMPTY = "—";

/** Base URL for the Stellar Expert transaction explorer. */
export var EXPLORER_TX_BASE = "https://stellar.expert/explorer/public/tx/";

/** Column headers for the CSV export (#706). */
export var CSV_COLUMNS = ["id", "status", "amount", "asset", "memo", "created_at"];

/** Map a payment or delivery status onto a pill style. */
export function pillClass(status) {
  switch (status) {
    case "completed":
    case "delivered":
      return "pill pill-ok";
    case "pending":
    case "underpaid":
      return "pill pill-warn";
    case "expired":
    case "failed":
      return "pill pill-err";
    default:
      return "pill pill-idle";
  }
}

/** Format a payment amount with its asset code. */
export function formatAmount(amount, asset) {
  // Only skip truly absent values (null / empty string); undefined and
  // non-numeric strings must pass through so the column never silently hides
  // data or renders "NaN XLM".
  if (amount === null || amount === "") return EMPTY;
  // Strip trailing zeros from a numeric amount string, preserving up to
  // 7 decimal places (one stroop). Non-numeric values pass through intact.
  var str = String(amount);
  var n = Number(str);
  if (isFinite(n)) {
    // toFixed(7) then trim trailing zeros and a possible trailing dot.
    str = n.toFixed(7).replace(/\.?0+$/, "");
    // Guard against exponent form for very large/small numbers.
    if (str.indexOf("e") !== -1) str = String(amount);
  }
  return str + " " + (asset || "XLM");
}

/** Return a Stellar expert explorer URL for a transaction hash. */
export function explorerTx(hash) {
  return EXPLORER_TX_BASE + encodeURIComponent(hash);
}

/**
 * Human-readable relative time (e.g. "5s ago", "5m ago", "2h ago", "2d ago").
 *
 * Accepts an optional `now` epoch-ms override so tests can drive the clock.
 * Returns "never" for an absent or empty value, and echoes an unparseable
 * string as-is rather than printing "Invalid Date".
 *
 * @param {string|null|undefined} iso
 * @param {number} [now]
 */
export function relativeTime(iso, now) {
  if (!iso) return "never";
  var d = new Date(iso);
  if (isNaN(d.getTime())) return iso;
  var diffMs = (now !== undefined ? now : Date.now()) - d.getTime();
  var diffSec = Math.floor(diffMs / 1000);
  if (diffSec < 60) return diffSec + "s ago";
  var diffMin = Math.floor(diffSec / 60);
  if (diffMin < 60) return diffMin + "m ago";
  var diffHr = Math.floor(diffMin / 60);
  if (diffHr < 24) return diffHr + "h ago";
  return Math.floor(diffHr / 24) + "d ago";
}

/**
 * Human-readable countdown to an ISO timestamp.
 *
 * Accepts an optional `now` epoch-ms override so tests can drive the clock.
 * Floors within each bucket (never overstates time left).
 * Clamps to "0s" when the instant is in the past.
 * Returns "" for an unparseable instant.
 *
 * @param {string|null|undefined} iso
 * @param {number} [now]
 */
export function countdown(iso, now) {
  if (!iso) return "";
  var d = new Date(iso);
  var nowMs = now !== undefined ? now : Date.now();
  if (isNaN(d.getTime()) || isNaN(nowMs)) return "";
  var diffMs = d.getTime() - nowMs;
  if (diffMs <= 0) return "0s";
  var totalSec = Math.floor(diffMs / 1000);
  var days = Math.floor(totalSec / 86400);
  var h = Math.floor((totalSec % 86400) / 3600);
  var m = Math.floor((totalSec % 3600) / 60);
  var s = totalSec % 60;
  if (days > 0) return days + "d";
  if (h > 0) return h + "h";
  if (m > 0) return m + "m";
  return s + "s";
}

/** Format an ISO timestamp for display, falling back to the raw value. */
export function fmtTime(iso) {
  if (!iso) return EMPTY;
  var d = new Date(iso);
  if (isNaN(d.getTime())) return iso;
  return d.toLocaleString();
}

/**
 * Shorten a long identifier with an ellipsis. Values ≤ 12 characters pass
 * through unchanged; longer ones are clipped to 8 characters followed by "…".
 *
 * Non-string values pass through untouched (null, undefined, numbers).
 *
 * @param {*} id
 * @returns {*}
 */
export function shortId(id) {
  if (typeof id !== "string") return id;
  if (id.length <= 12) return id;
  return id.slice(0, 8) + "...";
}

/**
 * Build a SEP-7 `web+stellar:pay` URI for a payment intent.
 *
 * The URI encodes the destination, memo, amount and asset so a SEP-7-capable
 * wallet can prefill a payment directly from a QR code scan or a tap.
 *
 * @param {object} payment  A payment record from the API.
 * @returns {string}        The full web+stellar:pay URI.
 */
export function buildSep7Uri(payment) {
  var p = payment || {};
  var params = [];
  if (p.destination_address) {
    params.push("destination=" + encodeURIComponent(p.destination_address));
  }
  if (p.amount) {
    var n = Number(p.amount);
    var amtStr = isFinite(n)
      ? n.toFixed(7).replace(/\.?0+$/, "")
      : String(p.amount);
    params.push("amount=" + encodeURIComponent(amtStr));
  }
  var asset = String(p.asset || "XLM").toUpperCase();
  if (asset !== "XLM") {
    params.push("asset_code=" + encodeURIComponent(asset));
    if (p.asset_issuer) {
      params.push("asset_issuer=" + encodeURIComponent(p.asset_issuer));
    }
  }
  if (p.memo) {
    params.push("memo=" + encodeURIComponent(p.memo));
    params.push("memo_type=MEMO_TEXT");
  }
  return "web+stellar:pay?" + params.join("&");
}

/**
 * Case-insensitive filter over the rows already loaded, matching a query against
 * the memo and the payment id.
 *
 * This is deliberately client-side: it narrows what the operator can currently
 * see without another round trip.
 */
export function filterPayments(payments, query) {
  var q = String(query || "").trim().toLowerCase();
  if (!q) return payments || [];
  return (payments || []).filter(function (p) {
    if (!p) return false;
    return (
      String(p.memo || "").toLowerCase().indexOf(q) >= 0 ||
      String(p.id || "").toLowerCase().indexOf(q) >= 0
    );
  });
}

/* ── CSV export (#706) ──────────────────────────────────────────────────── */

/**
 * Quote a single CSV field: wrap in double-quotes and escape embedded
 * double-quotes by doubling them. A missing value becomes an empty quoted field.
 *
 * @param {*} value
 * @returns {string}
 */
export function csvField(value) {
  var str = (value === undefined || value === null) ? "" : String(value);
  return '"' + str.replace(/"/g, '""') + '"';
}

/**
 * Serialise an array of payment objects to a CSV string with a header row.
 * Each column is defined by a key in `columns`; the values are taken from the
 * matching property of each row.
 *
 * @param {object[]} payments
 * @param {string[]} [columns]  Defaults to CSV_COLUMNS.
 * @returns {string}
 */
export function toCsv(payments, columns) {
  var cols = columns || CSV_COLUMNS;
  var header = cols.map(function (c) { return csvField(c); }).join(",");
  var rows = (payments || []).map(function (p) {
    return cols.map(function (col) { return csvField(p ? p[col] : undefined); }).join(",");
  });
  return [header].concat(rows).join("\n");
}

/* ── List query builder ─────────────────────────────────────────────────── */

/**
 * Build the `/payments?…` query string from the current view state.
 *
 * The API base (/v1) is intentionally omitted — it is defined once in
 * dashboard.js so requests cannot gain an accidental double prefix. No
 * credential ever appears here: the key is always sent in the
 * `Authorization` header, never as a query parameter.
 *
 * @param {object} state  Partial view-state object from the store.
 * @returns {string}      Path + query string, e.g. "/payments?limit=25&status=pending".
 */
export function buildListQuery(state) {
  var s = state || {};
  var params = new URLSearchParams();
  params.set("limit", String(s.pageSize || 25));

  if (s.status) params.set("status", s.status);
  if (s.asset)  params.set("asset", s.asset);
  if (s.cursor) params.set("cursor", s.cursor);

  // Date pickers give back "YYYY-MM-DD"; expand to full UTC day boundaries.
  if (s.createdAfter)  params.set("created_after",  s.createdAfter  + "T00:00:00Z");
  if (s.createdBefore) params.set("created_before", s.createdBefore + "T23:59:59Z");

  return "/payments?" + params.toString();
}
