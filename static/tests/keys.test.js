/* Unit tests for static/dashboard-keys.js (issue #723, module from #721).
 *
 * The awkward cases of the shortcut matcher live here rather than only in a
 * browser: a `/` typed into a date field, a `j` typed into the search box, and
 * the modifier combinations the browser already owns. Each of those is a plain
 * object here, which is why this file needs no DOM.
 */

import { test } from "node:test";
import assert from "node:assert/strict";

import { isTypingTarget, matchShortcut, moveRow, SHORTCUTS } from "../keys.js";

/** A KeyboardEvent-like object; only the fields the matcher reads. */
const press = (key, mods = {}) => ({
  key,
  ctrlKey: false,
  metaKey: false,
  altKey: false,
  ...mods,
});

const input = (type) => ({ tagName: "INPUT", type });
const el = (tagName, extra = {}) => ({ tagName, ...extra });

/* ── the documented shortcut set ───────────────────────────────────────── */

test("every issue-721 shortcut is bound", () => {
  const actions = SHORTCUTS.map((s) => s.action);
  for (const expected of [
    "refresh",
    "focusSearch",
    "nextRow",
    "prevRow",
    "closeDrawer",
    "toggleHelp",
  ]) {
    assert.ok(actions.includes(expected), `missing shortcut: ${expected}`);
  }
});

test("every shortcut has a hint and a label for the help overlay", () => {
  // The overlay is rendered from this table, so an entry without a label would
  // produce a blank row rather than fail loudly.
  for (const s of SHORTCUTS) {
    assert.ok(s.label && s.label.length > 0, `${s.action} has no label`);
    assert.ok(s.hint && s.hint.length > 0, `${s.action} has no hint`);
    assert.ok(Array.isArray(s.keys) && s.keys.length > 0, `${s.action} has no key`);
  }
});

test("no two shortcuts claim the same key", () => {
  const seen = new Set();
  for (const s of SHORTCUTS) {
    for (const k of s.keys) {
      assert.ok(!seen.has(k), `key "${k}" is bound twice`);
      seen.add(k);
    }
  }
});

/* ── basic matching ────────────────────────────────────────────────────── */

test("r refreshes", () => {
  assert.equal(matchShortcut(press("r")), "refresh");
});

test("/ focuses the search box", () => {
  assert.equal(matchShortcut(press("/")), "focusSearch");
});

test("j and k move between rows", () => {
  assert.equal(matchShortcut(press("j")), "nextRow");
  assert.equal(matchShortcut(press("k")), "prevRow");
});

test("Escape closes the drawer", () => {
  assert.equal(matchShortcut(press("Escape")), "closeDrawer");
});

test("? toggles the help overlay", () => {
  assert.equal(matchShortcut(press("?")), "toggleHelp");
});

test("an unbound key matches nothing", () => {
  for (const key of ["a", "z", "1", "F5", "Tab", "Enter", "ArrowDown"]) {
    assert.equal(matchShortcut(press(key)), null, `key ${key} should be unbound`);
  }
});

test("an uppercase letter does not trigger its shortcut", () => {
  // Shift+R is a different key value; binding it would make the shortcut fire
  // on every capitalised letter typed anywhere.
  assert.equal(matchShortcut(press("R")), null);
});

test("a malformed event matches nothing", () => {
  assert.equal(matchShortcut(null), null);
  assert.equal(matchShortcut({}), null);
  assert.equal(matchShortcut({ key: "" }), null);
  assert.equal(matchShortcut({ key: 42 }), null);
});

/* ── ignored while typing ──────────────────────────────────────────────── */

test("character shortcuts stand down in a text input", () => {
  const ctx = { activeElement: input("search") };
  assert.equal(matchShortcut(press("r"), ctx), null);
  assert.equal(matchShortcut(press("j"), ctx), null);
  assert.equal(matchShortcut(press("k"), ctx), null);
  assert.equal(matchShortcut(press("?"), ctx), null);
  assert.equal(matchShortcut(press("/"), ctx), null);
});

test("typing a word containing shortcut letters does not fire them", () => {
  // The realistic case: typing "refund" into the search box must not reload the
  // list three times on the way.
  const ctx = { activeElement: input("search") };
  for (const key of ["r", "e", "f", "u", "n", "d"]) {
    assert.equal(matchShortcut(press(key), ctx), null, `"${key}" leaked through`);
  }
});

test("the API key field swallows the character shortcuts", () => {
  const ctx = { activeElement: input("password") };
  assert.equal(matchShortcut(press("r"), ctx), null);
});

test("a date field swallows the character shortcuts", () => {
  for (const type of ["date", "datetime-local", "month", "week", "time"]) {
    const ctx = { activeElement: input(type) };
    assert.equal(matchShortcut(press("/"), ctx), null, `type ${type} leaked`);
    assert.equal(matchShortcut(press("j"), ctx), null, `type ${type} leaked`);
  }
});

test("textarea, select and contenteditable swallow the character shortcuts", () => {
  for (const node of [
    el("TEXTAREA"),
    el("SELECT"),
    el("DIV", { isContentEditable: true }),
  ]) {
    assert.equal(matchShortcut(press("j"), { activeElement: node }), null);
  }
});

test("a checkbox does not swallow the character shortcuts", () => {
  // Letters are not how a checkbox is operated, so `j` there is deliberate and
  // should still navigate the list.
  for (const type of ["checkbox", "radio", "button", "submit", "reset"]) {
    const ctx = { activeElement: input(type) };
    assert.equal(matchShortcut(press("j"), ctx), "nextRow", `type ${type}`);
  }
});

test("a body or table row does not swallow the shortcuts", () => {
  for (const node of [el("BODY"), el("TR"), el("DIV"), null, undefined]) {
    assert.equal(matchShortcut(press("j"), { activeElement: node }), "nextRow");
  }
});

test("isTypingTarget handles an absent node", () => {
  assert.equal(isTypingTarget(null), false);
  assert.equal(isTypingTarget(undefined), false);
});

test("isTypingTarget defaults an input with no type to text", () => {
  assert.equal(isTypingTarget({ tagName: "INPUT" }), true);
});

/* ── Escape is exempt ──────────────────────────────────────────────────── */

test("Escape still closes the drawer while typing", () => {
  // Escape's whole job is to dismiss what is on screen. A user who types a
  // search term and presses Escape expects the panel to go away.
  const ctx = { activeElement: input("search") };
  assert.equal(matchShortcut(press("Escape"), ctx), "closeDrawer");
  assert.equal(matchShortcut(press("Escape"), { activeElement: input("password") }), "closeDrawer");
});

/* ── modifiers belong to the browser ───────────────────────────────────── */

test("no shortcut fires with a modifier held", () => {
  // Cmd+R is reload and Ctrl+/ is find-in-page in Firefox; intercepting either
  // would break the browser for the operator.
  const mods = [{ ctrlKey: true }, { metaKey: true }, { altKey: true }];
  for (const mod of mods) {
    for (const key of ["r", "/", "j", "k", "?", "Escape"]) {
      assert.equal(
        matchShortcut(press(key, mod)),
        null,
        `${key} with ${JSON.stringify(mod)} must not be intercepted`
      );
    }
  }
});

/* ── moveRow ───────────────────────────────────────────────────────────── */

test("moveRow advances and retreats", () => {
  assert.equal(moveRow(0, 5, 1), 1);
  assert.equal(moveRow(2, 5, 1), 3);
  assert.equal(moveRow(3, 5, -1), 2);
});

test("moveRow starts at the first row from the unselected state", () => {
  assert.equal(moveRow(-1, 5, 1), 0);
});

test("moveRow clamps at the last row instead of running off the end", () => {
  assert.equal(moveRow(4, 5, 1), 4);
  assert.equal(moveRow(0, 5, -1), 0, "clamps at the first row, does not wrap");
});

test("moveRow reports -1 for an empty list", () => {
  // "Nothing selected", not row 0 of nothing.
  assert.equal(moveRow(-1, 0, 1), -1);
  assert.equal(moveRow(3, 0, -1), -1);
  assert.equal(moveRow(0, 0, 1), -1);
});
