/* Unit tests for static/dashboard-state.js (issue #723, module from #679).
 *
 * The store is the dashboard's single source of truth, so these tests cover the
 * two properties the rest of the UI relies on: a mutation notifies exactly the
 * subscribers that care, and the derived reads ("which rows are visible", "is
 * the highlighted row still in range") are correct after a filter change.
 */

import { test } from "node:test";
import assert from "node:assert/strict";

import {
  createStore,
  initialState,
  parseHash,
  serializeHash,
} from "../state.js";

const ROWS = [
  { id: "p1", status: "pending", memo: "alpha" },
  { id: "p2", status: "completed", memo: "beta" },
  { id: "p3", status: "pending", memo: "gamma" },
];

/* ── initialState ──────────────────────────────────────────────────────── */

test("initialState starts signed out with no filters and no selection", () => {
  const s = initialState();
  assert.equal(s.key, null);
  assert.equal(s.status, "");
  assert.equal(s.search, "");
  assert.equal(s.autoRefresh, false);
  assert.equal(s.selectedPaymentId, null);
  assert.equal(s.activeRow, -1);
  assert.equal(s.helpOpen, false);
  assert.deepEqual(s.loadedPayments, []);
});

test("initialState returns a fresh object each call", () => {
  // A shared mutable default would let one store's rows leak into another's.
  const a = initialState();
  a.loadedPayments.push({ id: "x" });
  assert.deepEqual(initialState().loadedPayments, []);
});

test("createStore seeds from initialState", () => {
  const store = createStore();
  assert.deepEqual(store.get().loadedPayments, []);
  assert.equal(store.get().activeRow, -1);
});

test("createStore accepts a seed", () => {
  const store = createStore({ status: "pending", pageSize: 50 });
  assert.equal(store.get().status, "pending");
  assert.equal(store.get().pageSize, 50);
});

/* ── update ────────────────────────────────────────────────────────────── */

test("update merges a patch and returns the next state", () => {
  const store = createStore();
  const next = store.update({ status: "completed" });
  assert.equal(next.status, "completed");
  assert.equal(store.get().status, "completed");
});

test("update leaves untouched keys alone", () => {
  const store = createStore({ pageSize: 10 });
  store.update({ status: "pending" });
  assert.equal(store.get().pageSize, 10);
});

test("update drops keys set to undefined rather than blanking them", () => {
  // A partially built patch must not wipe a field it simply did not populate.
  const store = createStore({ status: "pending" });
  store.update({ search: undefined });
  assert.equal(store.get().status, "pending");
  assert.equal(store.get().search, "");
});

test("update does not notify when nothing actually changed", () => {
  const store = createStore({ status: "pending" });
  let calls = 0;
  store.subscribe(() => calls++);
  store.update({ status: "pending" });
  assert.equal(calls, 0, "a no-op patch must not cause a re-render");
  store.update({ status: "completed" });
  assert.equal(calls, 1);
});

test("update reports only the keys that changed", () => {
  const store = createStore({ status: "pending", pageSize: 25 });
  let seen = null;
  store.subscribe((_state, changed) => {
    seen = changed;
  });
  store.update({ status: "completed", pageSize: 25 });
  assert.deepEqual(seen, ["status"]);
});

test("update compares arrays by identity, not contents", () => {
  // loadedPayments is replaced wholesale, so a new array is a real change even
  // when the contents happen to be equal.
  const store = createStore();
  let calls = 0;
  store.subscribe(() => calls++);
  store.update({ loadedPayments: [] });
  assert.equal(calls, 1);
});

/* ── subscribe ─────────────────────────────────────────────────────────── */

test("subscribe receives the state and the changed keys", () => {
  const store = createStore();
  let received = null;
  store.subscribe((state, changed) => {
    received = { status: state.status, changed };
  });
  store.update({ status: "pending" });
  assert.deepEqual(received, { status: "pending", changed: ["status"] });
});

test("subscribe returns an unsubscribe function", () => {
  const store = createStore();
  let calls = 0;
  const off = store.subscribe(() => calls++);
  store.update({ status: "a" });
  off();
  store.update({ status: "b" });
  assert.equal(calls, 1);
});

test("unsubscribing twice is harmless", () => {
  const store = createStore();
  const off = store.subscribe(() => {});
  off();
  assert.doesNotThrow(() => off());
});

test("a subscriber may unsubscribe during notification", () => {
  // Iterating the live list would skip the next subscriber entirely.
  const store = createStore();
  let second = 0;
  const offFirst = store.subscribe(() => offFirst());
  store.subscribe(() => second++);
  store.update({ status: "pending" });
  assert.equal(second, 1);
});

test("every subscriber is notified", () => {
  const store = createStore();
  const seen = [];
  store.subscribe(() => seen.push("a"));
  store.subscribe(() => seen.push("b"));
  store.update({ status: "pending" });
  assert.deepEqual(seen, ["a", "b"]);
});

/* ── replace ───────────────────────────────────────────────────────────── */

test("replace resets to the defaults merged with the new state", () => {
  const store = createStore();
  store.update({ status: "pending", search: "x", activeRow: 3 });
  store.replace({ status: "completed" });
  const s = store.get();
  assert.equal(s.status, "completed");
  assert.equal(s.search, "", "a field absent from the new state resets");
  assert.equal(s.activeRow, -1, "the highlight does not survive a full reset");
});

/* ── derived reads ─────────────────────────────────────────────────────── */

test("visiblePayments returns loaded rows when the search is blank", () => {
  const store = createStore({ loadedPayments: ROWS });
  assert.deepEqual(store.visiblePayments(), ROWS);
});

test("visiblePayments narrows by the search term", () => {
  const store = createStore({ loadedPayments: ROWS, search: "beta" });
  assert.deepEqual(store.visiblePayments(), [ROWS[1]]);
});

test("clampActiveRow pulls an out-of-range highlight back to the last row", () => {
  // Typing in the search box can leave the cursor pointing past the end; `j`
  // must keep working afterwards.
  const store = createStore({ loadedPayments: ROWS, activeRow: 2, search: "alpha" });
  store.clampActiveRow();
  assert.equal(store.get().activeRow, 0);
  assert.deepEqual(store.visiblePayments(), [ROWS[0]]);
});

test("clampActiveRow clears the highlight when nothing matches", () => {
  const store = createStore({ loadedPayments: ROWS, activeRow: 2, search: "zzz" });
  store.clampActiveRow();
  assert.equal(store.get().activeRow, -1);
});

test("clampActiveRow leaves an in-range highlight alone", () => {
  const store = createStore({ loadedPayments: ROWS, activeRow: 1 });
  store.clampActiveRow();
  assert.equal(store.get().activeRow, 1);
});

test("clampActiveRow repairs a highlight below the valid range", () => {
  const store = createStore({ loadedPayments: ROWS, activeRow: -5 });
  store.clampActiveRow();
  assert.equal(store.get().activeRow, -1);
});

test("resetPaging clears the cursor and the highlight", () => {
  // Called on every filter change: a cursor from the previous result set would
  // page into the wrong place.
  const store = createStore();
  store.update({ cursor: "abc", activeRow: 4, status: "completed" });
  store.resetPaging();
  assert.equal(store.get().cursor, null);
  assert.equal(store.get().activeRow, -1);
  assert.equal(store.get().status, "completed", "filters survive the reset");
});

/* ── URL hash (#696) ───────────────────────────────────────────────────── */

test("parseHash reads the known filter keys", () => {
  assert.deepEqual(parseHash("#status=pending&search=abc&auto_refresh=1"), {
    status: "pending",
    search: "abc",
    autoRefresh: true,
  });
});

test("parseHash accepts a hash with no leading #", () => {
  assert.equal(parseHash("status=pending").status, "pending");
});

test("parseHash is empty for an absent or empty hash", () => {
  assert.deepEqual(parseHash(""), { status: "", search: "", autoRefresh: false });
  assert.deepEqual(parseHash("#"), { status: "", search: "", autoRefresh: false });
  assert.deepEqual(parseHash(undefined), {
    status: "",
    search: "",
    autoRefresh: false,
  });
});

test("parseHash ignores unknown keys", () => {
  // A hand-edited URL must not be able to set an arbitrary state field.
  const parsed = parseHash("#status=pending&key=stolen&pageSize=9999");
  assert.equal(parsed.status, "pending");
  assert.equal(parsed.key, undefined);
  assert.equal(parsed.pageSize, undefined);
});

test("parseHash decodes a percent-encoded value", () => {
  assert.equal(parseHash("#search=a%20b%26c").search, "a b&c");
});

test("parseHash only treats an explicit 1 or true as auto-refresh", () => {
  assert.equal(parseHash("#auto_refresh=1").autoRefresh, true);
  assert.equal(parseHash("#auto_refresh=true").autoRefresh, true);
  assert.equal(parseHash("#auto_refresh=0").autoRefresh, false);
  assert.equal(parseHash("#auto_refresh=yes").autoRefresh, false);
});

test("parseHash survives a malformed hash without throwing", () => {
  assert.doesNotThrow(() => parseHash("#%%%%"));
});

test("serializeHash round-trips the filter state", () => {
  const state = { status: "pending", search: "abc", autoRefresh: true };
  assert.deepEqual(parseHash("#" + serializeHash(state)), state);
});

test("serializeHash is empty when no filter is set", () => {
  // Keeps the URL free of a bare trailing "#".
  assert.equal(serializeHash(initialState()), "");
  assert.equal(serializeHash({}), "");
});

test("serializeHash omits auto_refresh when it is off", () => {
  assert.equal(serializeHash({ autoRefresh: false }), "");
  assert.equal(serializeHash({ autoRefresh: true }), "auto_refresh=1");
});

test("serializeHash escapes a filter value", () => {
  // A filter value is echoed into the address bar, so it must not be able to
  // break out of its own parameter.
  const hash = serializeHash({ search: "a&status=pending" });
  assert.ok(!hash.includes("&status=pending"), hash);
  assert.deepEqual(parseHash("#" + hash), {
    status: "",
    search: "a&status=pending",
    autoRefresh: false,
  });
});

test("serializeHash never emits a credential", () => {
  // #726: the hash is a shareable, client-visible URL, so it must be provably
  // free of anything secret.
  const hash = serializeHash({
    status: "pending",
    search: "x",
    key: "sg_live_secret",
  });
  assert.ok(!hash.includes("secret"), hash);
  assert.ok(!/key|token|auth/i.test(hash), hash);
});

test("serializeHash never emits a merchant id or key material (#767, #768)", () => {
  // The admin key panel must stay out of the address bar entirely: a merchant
  // id is an internal identifier and a freshly issued key is a live credential.
  const hash = serializeHash({
    status: "pending",
    merchantId: "mch_123",
    apiKey: "sk_live_abcdef",
    keyPrefix: "sk_live_ab",
    revealedKey: "sk_live_abcdef",
  });
  assert.ok(!hash.includes("mch_123"), hash);
  assert.ok(!hash.includes("sk_live"), hash);
  assert.ok(!/merchant|api_?key|prefix|reveal/i.test(hash), hash);
});

/* ── admin API keys (#767, #768, #769) ─────────────────────────────────── */

test("initialState starts with no merchant, no keys and no revealed key", () => {
  const s = initialState();
  assert.equal(s.merchantId, "");
  assert.deepEqual(s.merchantKeys, []);
  assert.equal(s.revealedKey, null);
});

test("revealKey stores the one-time key in state", () => {
  const store = createStore();
  store.revealKey("sk_live_abcdef", "sk_live_ab");
  assert.equal(store.get().revealedKey, "sk_live_abcdef");
  assert.equal(store.get().revealedKeyPrefix, "sk_live_ab");
});

test("clearRevealedKey wipes the key from state", () => {
  // #768: closing the panel must drop the key from memory, not just hide it.
  const store = createStore();
  store.revealKey("sk_live_abcdef", "sk_live_ab");
  store.clearRevealedKey();
  assert.equal(store.get().revealedKey, null);
  assert.equal(store.get().revealedKeyPrefix, null);
  assert.ok(!JSON.stringify(store.get()).includes("sk_live_abcdef"));
});

test("clearRevealedKey notifies subscribers so the DOM can be cleared", () => {
  const store = createStore();
  store.revealKey("sk_live_abcdef", "sk_live_ab");
  let changed = null;
  store.subscribe((_state, keys) => {
    changed = keys;
  });
  store.clearRevealedKey();
  assert.ok(changed.includes("revealedKey"), String(changed));
});

test("clearRevealedKey is a no-op when nothing is revealed", () => {
  const store = createStore();
  let calls = 0;
  store.subscribe(() => calls++);
  store.clearRevealedKey();
  assert.equal(calls, 0);
});

test("setMerchantKeys replaces the list without keeping key material", () => {
  // The list endpoint only returns ids, prefixes and timestamps.
  const store = createStore();
  store.setMerchantKeys([
    { id: "k1", prefix: "sk_live_ab", created_at: "2024-01-01T00:00:00Z", revoked_at: null },
  ]);
  assert.equal(store.get().merchantKeys.length, 1);
  assert.equal(store.get().merchantKeys[0].prefix, "sk_live_ab");
  assert.ok(!JSON.stringify(store.get().merchantKeys).includes("sk_live_abcdef"));
});

test("canRevokeKey requires an exact prefix match (#769)", () => {
  const store = createStore();
  store.setMerchantKeys([
    { id: "k1", prefix: "sk_live_ab", created_at: "2024-01-01T00:00:00Z", revoked_at: null },
  ]);
  assert.equal(store.canRevokeKey("k1", "sk_live_ab"), true);
  assert.equal(store.canRevokeKey("k1", "sk_live_a"), false, "a partial prefix must not confirm");
  assert.equal(store.canRevokeKey("k1", "sk_live_abc"), false);
  assert.equal(store.canRevokeKey("k1", "SK_LIVE_AB"), false, "case must match");
  assert.equal(store.canRevokeKey("k1", " sk_live_ab"), false, "no trimming");
  assert.equal(store.canRevokeKey("nope", "sk_live_ab"), false);
});

test("canRevokeKey refuses an already revoked key", () => {
  const store = createStore();
  store.setMerchantKeys([
    { id: "k1", prefix: "sk_live_ab", created_at: "2024-01-01T00:00:00Z", revoked_at: "2024-02-01T00:00:00Z" },
  ]);
  assert.equal(store.canRevokeKey("k1", "sk_live_ab"), false);
});

/* ── sorting (#770) ────────────────────────────────────────────────────── */

const SORT_ROWS = [
  { id: "p1", amount: 300, created_at: "2024-01-02T00:00:00Z" },
  { id: "p2", amount: 100, created_at: "2024-01-03T00:00:00Z" },
  { id: "p3", amount: 200, created_at: "2024-01-01T00:00:00Z" },
];

test("initialState has no sort applied", () => {
  const s = initialState();
  assert.equal(s.sortKey, null);
  assert.equal(s.sortDir, "asc");
});

test("sortedPayments returns loaded rows untouched with no sort", () => {
  const store = createStore({ loadedPayments: SORT_ROWS });
  assert.deepEqual(store.sortedPayments(), SORT_ROWS);
});

test("sortedPayments orders by amount ascending and descending", () => {
  const store = createStore({ loadedPayments: SORT_ROWS, sortKey: "amount", sortDir: "asc" });
  assert.deepEqual(store.sortedPayments().map((r) => r.id), ["p2", "p3", "p1"]);
  store.update({ sortDir: "desc" });
  assert.deepEqual(store.sortedPayments().map((r) => r.id), ["p1", "p3", "p2"]);
});

test("sortedPayments orders by created_at ascending and descending", () => {
  const store = createStore({ loadedPayments: SORT_ROWS, sortKey: "created_at", sortDir: "asc" });
  assert.deepEqual(store.sortedPayments().map((r) => r.id), ["p3", "p1", "p2"]);
  store.update({ sortDir: "desc" });
  assert.deepEqual(store.sortedPayments().map((r) => r.id), ["p2", "p1", "p3"]);
});

test("sortedPayments does not mutate the loaded rows", () => {
  const store = createStore({ loadedPayments: SORT_ROWS, sortKey: "amount", sortDir: "desc" });
  store.sortedPayments();
  assert.deepEqual(store.get().loadedPayments.map((r) => r.id), ["p1", "p2", "p3"]);
});

test("sortedPayments is stable for equal keys", () => {
  const rows = [
    { id: "a", amount: 100 },
    { id: "b", amount: 100 },
    { id: "c", amount: 100 },
  ];
  const store = createStore({ loadedPayments: rows, sortKey: "amount", sortDir: "asc" });
  assert.deepEqual(store.sortedPayments().map((r) => r.id), ["a", "b", "c"]);
});

test("toggleSort cycles a column through asc, desc and off", () => {
  const store = createStore();
  store.toggleSort("amount");
  assert.equal(store.get().sortKey, "amount");
  assert.equal(store.get().sortDir, "asc");
  store.toggleSort("amount");
  assert.equal(store.get().sortDir, "desc");
  store.toggleSort("amount");
  assert.equal(store.get().sortKey, null);
});

test("toggleSort on a new column starts ascending", () => {
  const store = createStore({ sortKey: "amount", sortDir: "desc" });
  store.toggleSort("created_at");
  assert.equal(store.get().sortKey, "created_at");
  assert.equal(store.get().sortDir, "asc");
});

test("sorting is labelled as client-side when more pages remain", () => {
  // #770: the sort only covers loaded rows, so the UI must say so.
  const store = createStore({ loadedPayments: SORT_ROWS, cursor: "abc" });
  assert.equal(store.sortIsPartial(), true);
  store.update({ cursor: null });
  assert.equal(store.sortIsPartial(), false);
});
