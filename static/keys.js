/* Keyboard shortcuts for the dashboard (issue #721).
 *
 * The decision *which* shortcut a keystroke means is pure and lives here; the
 * DOM wiring lives in `dashboard.js`. Keeping the matcher DOM-free means the
 * awkward cases — a `/` typed into the date picker, a `j` typed into the search
 * box, Cmd+R the browser already owns — are unit tested with plain objects under
 * `node --test` (issue #723) instead of only being exercised by a browser.
 *
 * `SHORTCUTS` is the single source of truth for both the matcher and the `?`
 * help overlay, so a shortcut cannot exist without being documented, and a
 * documented shortcut cannot drift from the one that actually fires.
 */

/**
 * Every shortcut, in the order the help overlay lists them.
 *
 * `keys` are matched against `KeyboardEvent.key` with no modifier. `hint` is
 * what the overlay renders in the key column.
 */
export const SHORTCUTS = [
  { action: "refresh", keys: ["r"], hint: "r", label: "Reload payments" },
  { action: "focusSearch", keys: ["/"], hint: "/", label: "Focus the search box" },
  { action: "nextRow", keys: ["j"], hint: "j", label: "Select the next payment" },
  { action: "prevRow", keys: ["k"], hint: "k", label: "Select the previous payment" },
  { action: "closeDrawer", keys: ["Escape"], hint: "Esc", label: "Close the detail panel" },
  { action: "toggleHelp", keys: ["?"], hint: "?", label: "Show or hide this help" },
];

/**
 * Element types that swallow plain character keys.
 *
 * `select` is included because a date input renders as one in some browsers and
 * because arrow-key selection there is the browser's business, not ours.
 */
const TEXT_INPUT_TYPES = [
  "text",
  "password",
  "search",
  "email",
  "url",
  "tel",
  "number",
  "date",
  "datetime-local",
  "month",
  "time",
  "week",
];

/**
 * True when a keystroke is destined for a text entry, so the single-letter
 * shortcuts must stand down.
 *
 * Only the no-modifier case is consulted: the character shortcuts are the ones
 * suppressed, and the caller is responsible for the rest.
 *
 * Takes a node-like object rather than reaching for `document.activeElement`, so
 * the test can pass a bare `{ tagName, type, isContentEditable }`.
 */
export function isTypingTarget(node) {
  if (!node) return false;
  if (node.isContentEditable) return true;
  var tag = String(node.tagName || "").toUpperCase();
  if (tag === "TEXTAREA" || tag === "SELECT") return true;
  if (tag !== "INPUT") return false;
  var type = String(node.type || "text").toLowerCase();
  /* Checkboxes and radios answer to Space, not to letters, and pressing a
     letter while one is focused is a deliberate act, not an accidental one. */
  if (type === "checkbox" || type === "radio" || type === "button" || type === "submit" || type === "reset") {
    return false;
  }
  return TEXT_INPUT_TYPES.indexOf(type) >= 0;
}

/**
 * Resolve a keystroke to a shortcut action name, or null.
 *
 * Rules, in order:
 *   1. A keystroke carrying Ctrl/Meta/Alt belongs to the browser or the OS and
 *      is never intercepted — swallowing Cmd+R or Ctrl+/ would break reload and
 *      the browser's find-in-page.
 *   2. Character shortcuts stand down while a text field has focus, so typing
 *      "refund" into the search box does not reload the page three times.
 *   3. Escape is exempt from (2). It is the one shortcut whose whole job is to
 *      dismiss what is on screen, and a user who types a search term and then
 *      presses Escape expects the panel to go away, not the keystroke to be
 *      swallowed. It is a no-op when nothing is open.
 *
 * @param {object} ev      KeyboardEvent-like: `{ key, ctrlKey, metaKey, altKey }`.
 * @param {object} [ctx]   `{ activeElement }` — the focused node, if any.
 * @returns {string|null}  The action name, or null when unbound.
 */
export function matchShortcut(ev, ctx) {
  if (!ev || typeof ev.key !== "string" || ev.key === "") return null;
  if (ev.ctrlKey || ev.metaKey || ev.altKey) return null;

  var match = findByKey(ev.key);
  if (!match) return null;

  if (match.action === "closeDrawer") return match.action;

  if (isTypingTarget(ctx && ctx.activeElement)) return null;
  return match.action;
}

/** Look up a shortcut definition by its exact `KeyboardEvent.key` value. */
function findByKey(key) {
  for (var i = 0; i < SHORTCUTS.length; i++) {
    if (SHORTCUTS[i].keys.indexOf(key) >= 0) return SHORTCUTS[i];
  }
  return null;
}

/**
 * Move the highlighted row by `delta`, clamped to the available rows.
 *
 * Pure so the clamping at both ends is testable: moving past the last row must
 * land on the last row, not wrap or run off the end, and an empty list must
 * report -1 ("nothing selected") rather than 0.
 */
export function moveRow(current, count, delta) {
  if (count <= 0) return -1;
  var next = (current < 0 ? -1 : current) + delta;
  if (next < 0) return 0;
  if (next > count - 1) return count - 1;
  return next;
}
