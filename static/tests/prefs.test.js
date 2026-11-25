import { test } from "node:test";
import assert from "node:assert/strict";

import { ALWAYS_VISIBLE, COLUMNS, createPrefsStore } from "../prefs.js";
import { memoryStorage } from "../session.js";
import { COLUMNS as COLUMN_DEFS, defaultPrefs, PREFS_NAMESPACE } from "../state.js";

test("COLUMNS and ALWAYS_VISIBLE are the ids from state.js", () => {
  assert.deepEqual(COLUMNS, COLUMN_DEFS.map((c) => c.id));
  assert.deepEqual(ALWAYS_VISIBLE, ["id", "status"]);
});

test("a fresh store returns the defaults", () => {
  assert.deepEqual(createPrefsStore({ local: memoryStorage() }).get(), defaultPrefs());
});

test("update merges, validates and persists across stores", () => {
  const local = memoryStorage();
  const a = createPrefsStore({ local });
  a.update({ pageSize: 50, theme: "dark" });
  a.update({ timezone: "utc" });
  assert.equal(a.get().pageSize, 50, "a partial update keeps the other fields");
  assert.equal(a.get().theme, "dark");
  assert.equal(a.get().timezone, "utc");
  assert.equal(a.update({ pageSize: 7 }).pageSize, 25, "an invalid page size falls back to the default");
  a.update({ pageSize: 50 });

  const b = createPrefsStore({ local });
  assert.deepEqual(b.get(), a.get(), "a reload sees the same preferences");
  assert.equal(local.getItem(PREFS_NAMESPACE + "pageSize"), "50");
});

test("reset clears preferences but not the API key", () => {
  const local = memoryStorage();
  local.setItem("stellargate.apiKey", "sk_test");
  const store = createPrefsStore({ local });
  store.update({ pageSize: 100, autoRefresh: true });
  assert.deepEqual(store.reset(), defaultPrefs());
  assert.deepEqual(createPrefsStore({ local }).get(), defaultPrefs());
  assert.equal(local.getItem("stellargate.apiKey"), "sk_test");
});

test("without storage the store still works for the page", () => {
  const store = createPrefsStore({ local: null });
  store.update({ pageSize: 10 });
  assert.equal(store.get().pageSize, 10);
  assert.deepEqual(store.reset(), defaultPrefs());
});
