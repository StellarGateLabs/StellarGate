/* Unit tests for static/dashboard-session.js (issue #723, module from #678).
 *
 * The store takes its storage areas and its clock as arguments precisely so it
 * can be driven here with plain objects — no browser, no jsdom, no dependency.
 * The Web-Storage-blocked cases get real coverage because they are the ones that
 * would otherwise only be found by an operator in Safari private mode.
 */

import { test } from "node:test";
import assert from "node:assert/strict";

import {
  createSessionStore,
  KEY_NAME,
  KEY_SAVED_AT,
  memoryStorage,
  SESSION_TTL_MS,
} from "../session.js";

const KEY = "sg_live_aBcDeF0123456789";

/** A Storage-shaped object that records writes, standing in for the real thing. */
function fakeArea(options = {}) {
  const map = Object.create(null);
  return {
    writes: [],
    throwOn: options.throwOn || null,
    getItem(k) {
      if (options.throwOn === "get") throw new Error("storage blocked");
      return Object.prototype.hasOwnProperty.call(map, k) ? map[k] : null;
    },
    setItem(k, v) {
      if (options.throwOn === "set") throw new Error("quota exceeded");
      map[k] = String(v);
      this.writes.push([k, String(v)]);
    },
    removeItem(k) {
      if (options.throwOn === "remove") throw new Error("storage blocked");
      delete map[k];
    },
  };
}

const at = (ms) => () => ms;

function storeWith(areas = {}, now = 1_700_000_000_000) {
  return createSessionStore({
    session: areas.session || fakeArea(),
    local: areas.local || fakeArea(),
    now: at(now),
  });
}

/* ── read ──────────────────────────────────────────────────────────────── */

test("read returns null when nothing is stored", () => {
  assert.equal(storeWith().read(), null);
});

test("read returns a key written to session storage", () => {
  const session = fakeArea();
  session.setItem(KEY_NAME, KEY);
  assert.equal(storeWith({ session }).read(), KEY);
});

test("read returns a remembered key when session storage is empty", () => {
  const local = fakeArea();
  local.setItem(KEY_NAME, KEY);
  assert.equal(storeWith({ local }).read(), KEY);
});

test("session storage wins over local storage", () => {
  // Signing in without "remember me" must take effect immediately, not leave the
  // remembered key in charge of the next request.
  const session = fakeArea();
  const local = fakeArea();
  session.setItem(KEY_NAME, "session-key");
  local.setItem(KEY_NAME, "remembered-key");
  assert.equal(storeWith({ session, local }).read(), "session-key");
});

test("read survives storage that throws on access", () => {
  // Safari private mode throws on touching localStorage at all; the dashboard
  // must degrade to "no stored key", never crash.
  const store = storeWith({
    session: fakeArea({ throwOn: "get" }),
    local: fakeArea({ throwOn: "get" }),
  });
  assert.equal(store.read(), null);
});

/* ── write ─────────────────────────────────────────────────────────────── */

test("write stores in session storage when persist is false", () => {
  const session = fakeArea();
  const local = fakeArea();
  storeWith({ session, local }).write(KEY, false);
  assert.equal(session.getItem(KEY_NAME), KEY);
  assert.equal(local.getItem(KEY_NAME), null);
});

test("write stores in local storage when persist is true", () => {
  const session = fakeArea();
  const local = fakeArea();
  storeWith({ session, local }).write(KEY, true);
  assert.equal(local.getItem(KEY_NAME), KEY);
  assert.equal(session.getItem(KEY_NAME), null);
});

test("write records the save timestamp alongside the key", () => {
  const now = 1_700_000_000_000;
  const session = fakeArea();
  storeWith({ session }, now).write(KEY, false);
  assert.equal(session.getItem(KEY_SAVED_AT), String(now));
});

test("write refuses an empty key", () => {
  const session = fakeArea();
  const store = storeWith({ session });
  assert.equal(store.write("", false), false);
  assert.equal(store.write(null, false), false);
  assert.equal(session.getItem(KEY_NAME), null);
});

test("write reports failure but does not throw when storage rejects it", () => {
  // A full or blocked quota must not break sign-in: the key still works for
  // this page load, it just will not survive a reload.
  const store = storeWith({ session: fakeArea({ throwOn: "set" }) });
  assert.doesNotThrow(() => store.write(KEY, false));
  assert.equal(store.write(KEY, false), false);
});

/* ── clear ─────────────────────────────────────────────────────────────── */

test("clear removes the key from both areas", () => {
  const session = fakeArea();
  const local = fakeArea();
  const store = storeWith({ session, local });
  store.write(KEY, true);
  local.setItem(KEY_NAME, KEY);
  session.setItem(KEY_NAME, KEY);

  store.clear();

  assert.equal(session.getItem(KEY_NAME), null);
  assert.equal(local.getItem(KEY_NAME), null);
  assert.equal(session.getItem(KEY_SAVED_AT), null);
  assert.equal(local.getItem(KEY_SAVED_AT), null);
});

test("clear removes the save timestamp too", () => {
  // Leaving a timestamp behind would make the dashboard show a session-expiry
  // hint for a key the operator has already discarded.
  const session = fakeArea();
  const store = storeWith({ session });
  store.write(KEY, false);
  store.clear();
  assert.equal(store.savedAt(), null);
  assert.equal(store.expiresAt(), null);
});

test("clear does not throw when storage rejects removal", () => {
  const store = storeWith({
    session: fakeArea({ throwOn: "remove" }),
    local: fakeArea({ throwOn: "remove" }),
  });
  assert.doesNotThrow(() => store.clear());
});

/* ── savedAt / expiresAt ───────────────────────────────────────────────── */

test("savedAt reports the recorded save time", () => {
  const now = 1_700_000_000_000;
  const session = fakeArea();
  const store = storeWith({ session }, now);
  store.write(KEY, false);
  assert.equal(store.savedAt(), now);
});

test("savedAt is null when nothing is stored", () => {
  assert.equal(storeWith().savedAt(), null);
});

test("savedAt is null for a corrupted timestamp", () => {
  // A hand-edited or truncated value must not become NaN in the expiry hint.
  const session = fakeArea();
  session.setItem(KEY_NAME, KEY);
  session.setItem(KEY_SAVED_AT, "not-a-number");
  assert.equal(storeWith({ session }).savedAt(), null);
});

test("expiresAt is the save time plus the session window", () => {
  const now = 1_700_000_000_000;
  const session = fakeArea();
  const store = storeWith({ session }, now);
  store.write(KEY, false);
  assert.equal(store.expiresAt(), now + SESSION_TTL_MS);
});

test("expiresAt is null when nothing is stored", () => {
  assert.equal(storeWith().expiresAt(), null);
});

/* ── has / describe ────────────────────────────────────────────────────── */

test("has reflects whether a key is stored", () => {
  const session = fakeArea();
  const store = storeWith({ session });
  assert.equal(store.has(), false);
  store.write(KEY, false);
  assert.equal(store.has(), true);
  store.clear();
  assert.equal(store.has(), false);
});

test("describe never returns the secret", () => {
  // This is the one function a UI is allowed to render, so it must be provably
  // incapable of leaking the key (#726).
  const session = fakeArea();
  const store = storeWith({ session });
  store.write(KEY, false);
  const described = store.describe();
  assert.equal(described.length, KEY.length);
  assert.equal(described.prefix, KEY.slice(0, 8));
  assert.ok(
    !JSON.stringify(described).includes(KEY),
    "describe() must not embed the full key"
  );
});

test("describe is null when no key is stored", () => {
  assert.equal(storeWith().describe(), null);
});

/* ── memoryStorage fallback ────────────────────────────────────────────── */

test("memoryStorage behaves like a Storage object", () => {
  const area = memoryStorage();
  assert.equal(area.getItem("missing"), null);
  area.setItem("k", "v");
  assert.equal(area.getItem("k"), "v");
  area.setItem("n", 42);
  assert.equal(area.getItem("n"), "42", "values are stringified");
  area.removeItem("k");
  assert.equal(area.getItem("k"), null);
});

test("a store with no injected areas still works", () => {
  // Importing the module in a bare Node process must not throw; that is the
  // situation the unit tests themselves run in.
  const store = createSessionStore();
  assert.equal(store.read(), null);
  store.write(KEY, false);
  assert.equal(store.read(), KEY);
});

test("a store with areas that throw on construction still works", () => {
  const store = createSessionStore({
    session: fakeArea({ throwOn: "get" }),
    local: fakeArea({ throwOn: "get" }),
  });
  assert.doesNotThrow(() => store.has());
  assert.equal(store.has(), false);
});
