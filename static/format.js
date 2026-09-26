/* Pure formatting helpers for the dashboard.
 *
 * Everything here is a total function of its arguments: no DOM access, no
 * module-level mutable state, no reading of `window` or `Date.now()` other than
 * through an explicit `now` parameter where the value is time-dependent. That is
 * what makes the module testable under `node --test` (issue #723) without a
 * browser or a DOM shim, and it is why the countdown/relative-time helpers take
 * `now` instead of calling the clock themselves.
 *
 * A note on separators: the em dash and ellipsis below are the values the
 * dashboard has always rendered. They are written as escapes rather than
 * literals so a copy/paste or an editor that mangles non-ASCII cannot silently
 * change what a merchant sees.
 */

/** Placeholder for an absent value. */
export const EMPTY = "—";

/** Public explorer used for the transaction link in the detail panel. */
export const EXPLORER_TX_BASE =
  "https://stellar.expert/explorer/public/tx/";

/**
 * Localised timestamp for an ISO-8601 instant.
 *
 * An unparseable value is echoed back verbatim rather than rendered as
 * "Invalid Date": the raw string is far more useful to an operator debugging a
 * bad `expires_at` than the browser's own error text.
 */
export function fmtTime(iso) {
  if (!iso) return EMPTY;
  var d = new Date(iso);
  return isNaN(d.getTime()) ? iso : d.toLocaleString();
}

/**
 * Truncate an opaque identifier for display. Anything 12 characters or shorter
 * is already short enough to show in full, which keeps UUID-shaped test ids
 * readable while still clipping real 64-character transaction hashes.
 */
export function shortId(id) {
  return typeof id === "string" && id.length > 12
    ? id.slice(0, 8) + "..."
    : id;
}

/**
 * Render a decimal amount with its asset code, trimming the trailing zeros that
 * `toFixed` leaves behind so "10.0000000 XLM" displays as "10 XLM".
 *
 * A non-numeric amount is passed through with the asset appended rather than
 * replaced by NaN: the amount column must never render the string "NaN", since
 * that reads as a real value to whoever is reconciling it against the chain.
 */
export function formatAmount(amount, asset) {
  var n = Number(amount);
  if (!isFinite(n)) return String(amount) + " " + asset;
  return trimZeros(n.toFixed(7)) + " " + asset;
}

/** Strip trailing zeros (and a bare trailing decimal point) from a fixed-7 string. */
function trimZeros(fixed) {
  return fixed.replace(/\.?0+$/, "");
}

/**
 * Coarse time remaining until `iso`, e.g. "45s", "12m", "3h", "2d".
 *
 * `now` is injected so the output is deterministic under test. An instant in the
 * past clamps to "0s" rather than going negative — an expired intent showing
 * "-3m left" would be actively misleading while the operator is triaging it.
 * A non-finite `now` (a malformed clock) yields "" so the caller can omit the
 * whole suffix instead of printing a guess.
 */
export function countdown(iso, now) {
  var d = new Date(iso);
  var at = now === undefined ? Date.now() : now;
  if (!isFinite(at) || isNaN(d.getTime())) return "";
  var seconds = Math.floor((d.getTime() - at) / 1000);
  if (seconds < 0) seconds = 0;
  return coarseDuration(seconds);
}

/**
 * Coarse time elapsed since `iso`, e.g. "just now"-ish "12s ago", "5m ago".
 *
 * Rounds rather than floors, and returns "never" for an absent value, which is
 * the honest rendering for a delivery that has not been attempted yet.
 */
export function relativeTime(iso, now) {
  if (!iso) return "never";
  var d = new Date(iso);
  var at = now === undefined ? Date.now() : now;
  if (isNaN(d.getTime()) || !isFinite(at)) return String(iso);
  var seconds = Math.round((at - d.getTime()) / 1000);
  if (seconds < 60) return seconds + "s ago";
  if (seconds < 3600) return Math.round(seconds / 60) + "m ago";
  if (seconds < 86400) return Math.round(seconds / 3600) + "h ago";
  return Math.round(seconds / 86400) + "d ago";
}

/** Shared bucket boundaries for countdown() and relativeTime(). */
function coarseDuration(seconds) {
  if (seconds < 60) return seconds + "s";
  if (seconds < 3600) return Math.floor(seconds / 60) + "m";
  if (seconds < 86400) return Math.floor(seconds / 3600) + "h";
  return Math.floor(seconds / 86400) + "d";
}

/**
 * Map a payment or webhook-delivery status onto a pill modifier class.
 *
 * The class is composed from a fixed table rather than interpolated from the
 * status string, so a status a merchant controls can never inject a class name.
 */
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

/** Explorer deep link for a transaction hash. The hash is encoded, never trusted. */
export function explorerTx(txHash) {
  return EXPLORER_TX_BASE + encodeURIComponent(String(txHash));
}

/** Column order of the CSV export (issue #706). */
export const CSV_COLUMNS = [
  "id",
  "status",
  "amount",
  "asset",
  "asset_issuer",
  "memo",
  "destination_address",
  "created_at",
  "expires_at",
];

/**
 * Quote one CSV field per RFC 4180: always quoted, with embedded quotes doubled.
 *
 * A memo is merchant-controlled and routinely contains commas and quotes, so
 * quoting every field unconditionally is both simpler and safer than
 * quote-only-when-needed.
 */
export function csvField(value) {
  return '"' + String(value === undefined || value === null ? "" : value).replace(/"/g, '""') + '"';
}

/** Serialise loaded payments to a CSV document, header row first. */
export function toCsv(payments, columns) {
  var cols = columns || CSV_COLUMNS;
  var lines = [cols.map(csvField).join(",")];
  (payments || []).forEach(function (p) {
    lines.push(
      cols
        .map(function (key) {
          return csvField(p ? p[key] : "");
        })
        .join(",")
    );
  });
  return lines.join("\n");
}

/**
 * Build the `/payments` query string from the current filter state.
 *
 * Every value is percent-encoded and the API base is *not* included: the caller
 * concatenates it, which keeps the version prefix defined in exactly one place
 * (pinned by `tests/dashboard_asset_tests.rs`). An absent filter is omitted
 * rather than sent empty, so the server keeps applying its own default.
 *
 * `search` is deliberately absent: the search box narrows the rows already
 * loaded (see `filterPayments`) rather than asking the server for a matching
 * set, so there is no `search` parameter to send. Adding one here would imply a
 * server-side filter the API does not offer.
 */
export function buildListQuery(filters) {
  var f = filters || {};
  var parts = ["limit=" + encodeURIComponent(f.pageSize || 25)];
  if (f.status) parts.push("status=" + encodeURIComponent(f.status));
  if (f.createdAfter) {
    parts.push(
      "created_after=" + encodeURIComponent(f.createdAfter + "T00:00:00Z")
    );
  }
  if (f.createdBefore) {
    parts.push(
      "created_before=" + encodeURIComponent(f.createdBefore + "T23:59:59Z")
    );
  }
  if (f.cursor) parts.push("cursor=" + encodeURIComponent(f.cursor));
  return "/payments?" + parts.join("&");
}

/**
 * Case-insensitive filter over the rows already loaded, matching a query against
 * the memo and the payment id.
 *
 * This is deliberately client-side: it narrows what the operator can currently
 * see without another round trip, and it is what the `/` shortcut focuses.
 */
export function filterPayments(payments, query) {
  var q = String(query || "").trim().toLowerCase();
  if (!q) return payments || [];
  return (payments || []).filter(function (p) {
    return (
      String(p && p.memo ? p.memo : "")
        .toLowerCase()
        .indexOf(q) >= 0 ||
      String(p && p.id ? p.id : "")
        .toLowerCase()
        .indexOf(q) >= 0
    );
  });
}
