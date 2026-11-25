import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";

import { createPrefsStore } from "../prefs.js";
import { memoryStorage } from "../session.js";

const SOURCE = readFileSync(new URL("../dashboard-theme.js", import.meta.url), "utf8");

/** Run the classic bootstrap script against `local` and return data-theme. */
function bootstrap(local) {
  const attrs = {};
  const documentElement = {
    setAttribute: (name, value) => { attrs[name] = value; },
  };
  vm.runInNewContext(SOURCE, { window: { localStorage: local }, document: { documentElement } });
  return attrs["data-theme"] ?? null;
}

test("a theme chosen in the settings panel is applied before first paint", () => {
  for (const theme of ["light", "dark"]) {
    const local = memoryStorage();
    createPrefsStore({ local }).update({ theme });
    assert.equal(bootstrap(local), theme);
  }
});

test("following the system theme leaves data-theme unset", () => {
  const local = memoryStorage();
  createPrefsStore({ local }).update({ theme: "system" });
  assert.equal(bootstrap(local), null);
  assert.equal(bootstrap(memoryStorage()), null, "no stored preference");
});

test("unavailable storage falls back to the OS theme without throwing", () => {
  const broken = { getItem() { throw new Error("SecurityError"); } };
  assert.equal(bootstrap(broken), null);
});
