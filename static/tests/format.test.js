/* Unit tests for static/dashboard-format.js (issue #723).
 *
 * Run with `node --test static/tests/`. No dependencies, no DOM: the module
 * under test only ever touches its arguments and an injected clock, which is
 * what makes it testable in a bare Node process at all.
 */

import { test } from "node:test";
import assert from "node:assert/strict";

import {
  buildListQuery,
  countdown,
  csvField,
  CSV_COLUMNS,
  EMPTY,
  EXPLORER_TX_BASE,
  explorerTx,
  filterPayments,
  fmtTime,
  formatAmount,
  pillClass,
  relativeTime,
  shortId,
  toCsv,
} from "../format.js";

/* ── fmtTime ───────────────────────────────────────────────────────────── */

test("fmtTime returns the placeholder for an absent value", () => {
  assert.equal(fmtTime(null), EMPTY);
  assert.equal(fmtTime(undefined), EMPTY);
  assert.equal(fmtTime(""), EMPTY);
});

test("fmtTime echoes an unparseable value instead of printing Invalid Date", () => {
  // A bad expires_at must be visible as itself: "Invalid Date" hides the very
  // value an operator is trying to diagnose.
  assert.equal(fmtTime("not-a-date"), "not-a-date");
});

test("fmtTime renders a valid instant as a non-empty string", () => {
  const out = fmtTime("2026-01-02T03:04:05Z");
  assert.equal(typeof out, "string");
  assert.notEqual(out, "");
  assert.notEqual(out, EMPTY);
});

/* ── shortId ───────────────────────────────────────────────────────────── */

test("shortId truncates only identifiers longer than 12 characters", () => {
  assert.equal(
    shortId("a-very-long-payment-identifier"),
    "a-very-l...",
    "a long id is clipped to 8 characters plus an ellipsis"
  );
  assert.equal(shortId("short"), "short", "a short id is shown in full");
  assert.equal(
    shortId("exactly12chr"),
    "exactly12chr",
    "12 characters is the boundary and is not clipped"
  );
});

test("shortId passes non-strings through untouched", () => {
  assert.equal(shortId(null), null);
  assert.equal(shortId(undefined), undefined);
  assert.equal(shortId(42), 42);
});

/* ── formatAmount ──────────────────────────────────────────────────────── */

test("formatAmount trims the trailing zeros toFixed leaves behind", () => {
  assert.equal(formatAmount("10", "XLM"), "10 XLM");
  assert.equal(formatAmount("10.5000000", "XLM"), "10.5 XLM");
  assert.equal(formatAmount("0.0000001", "XLM"), "0.0000001 XLM");
});

test("formatAmount keeps seven decimal places of precision", () => {
  // Stroops are 1e-7 XLM; a value below that cannot be represented and must not
  // be silently rounded to something a merchant would over- or under-pay.
  assert.equal(formatAmount("1.2345678", "XLM"), "1.2345678 XLM");
});

test("formatAmount never renders NaN for a non-numeric amount", () => {
  // The amount column must not read "NaN XLM": that looks like a real figure.
  assert.equal(formatAmount("not-a-number", "XLM"), "not-a-number XLM");
  assert.equal(formatAmount(undefined, "USDC"), "undefined USDC");
});

test("formatAmount does not mangle a whole number into exponent form", () => {
  // toFixed is safe here; the guard is against a future switch to a
  // numeric-formatting helper that would emit 1e+21.
  assert.equal(formatAmount("1000000", "XLM"), "1000000 XLM");
});

/* ── countdown ─────────────────────────────────────────────────────────── */

const NOW = Date.parse("2026-01-01T00:00:00Z");

test("countdown buckets by magnitude", () => {
  const at = (seconds) => new Date(NOW + seconds * 1000).toISOString();
  assert.equal(countdown(at(0), NOW), "0s");
  assert.equal(countdown(at(45), NOW), "45s");
  assert.equal(countdown(at(60), NOW), "1m");
  assert.equal(countdown(at(59), NOW), "59s", "59s is still seconds, not a minute");
  assert.equal(countdown(at(3600), NOW), "1h");
  assert.equal(countdown(at(86400), NOW), "1d");
});

test("countdown floors within a bucket so it never overstates the time left", () => {
  const at = (seconds) => new Date(NOW + seconds * 1000).toISOString();
  // 119.9s must read "1m", not "2m": rounding up would tell an operator an
  // intent has less time left than it does.
  assert.equal(countdown(at(119), NOW), "1m");
  assert.equal(countdown(at(119.9), NOW), "1m");
});

test("countdown clamps an elapsed instant to zero rather than going negative", () => {
  const past = new Date(NOW - 90_000).toISOString();
  assert.equal(countdown(past, NOW), "0s");
});

test("countdown returns an empty string for an unparseable instant", () => {
  assert.equal(countdown("not-a-date", NOW), "");
  assert.equal(countdown("not-a-date", NaN), "");
});

/* ── relativeTime ──────────────────────────────────────────────────────── */

const at = (seconds) => new Date(NOW - seconds * 1000).toISOString();

test("relativeTime says never when there is no timestamp", () => {
  assert.equal(relativeTime(null, NOW), "never");
  assert.equal(relativeTime("", NOW), "never");
});

test("relativeTime buckets elapsed time", () => {
  assert.equal(relativeTime(at(5), NOW), "5s ago");
  assert.equal(relativeTime(at(300), NOW), "5m ago");
  assert.equal(relativeTime(at(7200), NOW), "2h ago");
  assert.equal(relativeTime(at(172800), NOW), "2d ago");
});

test("relativeTime echoes an unparseable value rather than guessing", () => {
  assert.equal(relativeTime("not-a-date", NOW), "not-a-date");
});

/* ── pillClass ─────────────────────────────────────────────────────────── */

test("pillClass maps every status the API can return", () => {
  assert.equal(pillClass("completed"), "pill pill-ok");
  assert.equal(pillClass("delivered"), "pill pill-ok");
  assert.equal(pillClass("pending"), "pill pill-warn");
  assert.equal(pillClass("underpaid"), "pill pill-warn");
  assert.equal(pillClass("expired"), "pill pill-err");
  assert.equal(pillClass("failed"), "pill pill-err");
});

test("pillClass falls back to the neutral pill for an unknown status", () => {
  // A merchant-influenced value must never be interpolated into a class name;
  // the table is exhaustive and the default is inert.
  assert.equal(pillClass("weird"), "pill pill-idle");
  assert.equal(pillClass(undefined), "pill pill-idle");
  assert.equal(pillClass('<img onerror=alert(1)>'), "pill pill-idle");
});

/* ── explorerTx ────────────────────────────────────────────────────────── */

test("explorerTx builds an explorer URL and encodes the hash", () => {
  assert.equal(
    explorerTx("abc123"),
    EXPLORER_TX_BASE + "abc123"
  );
  assert.equal(
    explorerTx("a/b?c=d#e"),
    EXPLORER_TX_BASE + "a%2Fb%3Fc%3Dd%23e",
    "a hash is encoded so it cannot break out of the path segment"
  );
});

/* ── CSV export (#706) ─────────────────────────────────────────────────── */

test("csvField quotes every field and doubles embedded quotes", () => {
  assert.equal(csvField("plain"), '"plain"');
  assert.equal(csvField('say "hi"'), '"say ""hi"""');
  assert.equal(csvField('a,b'), '"a,b"');
  assert.equal(csvField(""), '""');
});

test("csvField renders a missing value as an empty quoted field", () => {
  assert.equal(csvField(undefined), '""');
  assert.equal(csvField(null), '""');
});

test("toCsv emits a header row followed by one row per payment", () => {
  const csv = toCsv([
    { id: "p1", status: "completed", amount: "10", asset: "XLM", memo: "a" },
    { id: "p2", status: "pending", amount: "5", asset: "XLM", memo: "b" },
  ]);
  const lines = csv.split("\n");
  assert.equal(lines.length, 3);
  assert.equal(lines[0], CSV_COLUMNS.map((c) => `"${c}"`).join(","));
  assert.ok(lines[1].startsWith('"p1","completed"'));
  assert.ok(lines[2].startsWith('"p2","pending"'));
});

test("toCsv neutralises a formula injection in a merchant-controlled memo", () => {
  // A memo of =cmd|... must survive as data, not be re-interpreted by Excel or
  // Sheets when the operator opens the export.
  const csv = toCsv([{ id: "p1", memo: '=1+1,"x"' }]);
  assert.ok(csv.includes('"=1+1,""x"""'), csv);
});

test("toCsv handles an empty payment list", () => {
  assert.equal(toCsv([]), CSV_COLUMNS.map((c) => `"${c}"`).join(","));
  assert.equal(toCsv(undefined), CSV_COLUMNS.map((c) => `"${c}"`).join(","));
});

/* ── buildListQuery ────────────────────────────────────────────────────── */

test("buildListQuery always sends an explicit limit", () => {
  assert.equal(buildListQuery({}), "/payments?limit=25");
  assert.equal(buildListQuery({ pageSize: 50 }), "/payments?limit=50");
});

test("buildListQuery omits absent filters rather than sending them empty", () => {
  const q = buildListQuery({ status: "", createdAfter: "", cursor: "" });
  assert.equal(q, "/payments?limit=25");
});

test("buildListQuery expands date filters to whole UTC days", () => {
  assert.equal(
    buildListQuery({ createdAfter: "2026-01-01", createdBefore: "2026-01-31" }),
    "/payments?limit=25&created_after=2026-01-01T00%3A00%3A00Z" +
      "&created_before=2026-01-31T23%3A59%3A59Z"
  );
});

test("buildListQuery percent-encodes every filter value", () => {
  const q = buildListQuery({ status: "pending&limit=999", cursor: "a/b+c=d" });
  assert.ok(q.includes("status=pending%26limit%3D999"), q);
  assert.ok(q.includes("cursor=a%2Fb%2Bc%3Dd"), q);
});

test("buildListQuery does not include the API base", () => {
  // The version prefix is defined once in dashboard.js and pinned by
  // tests/dashboard_asset_tests.rs; repeating it here would allow /v1/v1.
  assert.ok(!buildListQuery({}).includes("/v1"));
});

test("buildListQuery never carries a credential", () => {
  // #726: the key travels in the Authorization header only.
  const q = buildListQuery({ status: "pending" });
  assert.ok(!/key|token|secret|authorization/i.test(q), q);
});

/* ── filterPayments ────────────────────────────────────────────────────── */

const ROWS = [
  { id: "pay_1", memo: "order-42" },
  { id: "pay_2", memo: "refund" },
  { id: "abc", memo: "other" },
];

test("filterPayments matches the memo case-insensitively", () => {
  assert.deepEqual(filterPayments(ROWS, "REFUND"), [ROWS[1]]);
  assert.deepEqual(filterPayments(ROWS, "order"), [ROWS[0]]);
});

test("filterPayments matches the payment id", () => {
  assert.deepEqual(filterPayments(ROWS, "pay_"), [ROWS[0], ROWS[1]]);
});

test("filterPayments returns everything for a blank query", () => {
  assert.deepEqual(filterPayments(ROWS, ""), ROWS);
  assert.deepEqual(filterPayments(ROWS, "   "), ROWS);
  assert.deepEqual(filterPayments(ROWS, null), ROWS);
});

test("filterPayments returns nothing when the query matches nothing", () => {
  assert.deepEqual(filterPayments(ROWS, "zzz"), []);
});

test("filterPayments tolerates malformed rows", () => {
  assert.deepEqual(filterPayments([{}, null], "x"), []);
  assert.deepEqual(filterPayments(undefined, "x"), []);
});
