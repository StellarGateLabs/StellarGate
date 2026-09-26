/* Accessibility smoke check for static/dashboard.html.
 *
 * A deliberately small, dependency-free guard rather than a full audit: it
 * catches the regressions that are easy to make while editing this file by
 * hand (an input losing its label, a dialog losing its role, the mobile
 * `data-label` hooks the card layout depends on disappearing).
 *
 * Run with: node scripts/check-dashboard-a11y.mjs
 */

import { readFileSync } from "node:fs";

const html = readFileSync("static/dashboard.html", "utf8");
const css = readFileSync("static/dashboard.css", "utf8");
const js = readFileSync("static/dashboard.js", "utf8");
const keys = readFileSync("static/keys.js", "utf8");
const failures = [];

/** Every id the controller looks up with `$()`; a missing one is a null deref. */
for (const id of [
  // Sign-in gate
  "api-key",
  "remember",
  "gate-form",
  "gate-error",
  // Chrome
  "version",
  "health",
  "session-expiry",
  "sign-out",
  // Toolbar
  "search",
  "search-clear",
  "created-after",
  "created-before",
  "page-size",
  "auto-refresh",
  "refresh",
  "export-csv",
  "load-more",
  // List
  "rows",
  "empty",
  "list-error",
  "rows-status",
  "summary",
  // Detail drawer
  "detail",
  "detail-fields",
  "detail-close",
  "deliveries",
  "deliveries-empty",
  "deliveries-error",
  "scrim",
  // Shortcut help (#721)
  "help",
  "help-list",
  "help-close",
  "help-open",
]) {
  if (!html.includes(`id="${id}"`)) failures.push(`missing #${id}`);
}

if (!/<html[^>]+lang=/.test(html)) failures.push("html element must declare lang");
if (!/<meta[^>]+name="viewport"/.test(html)) failures.push("missing responsive viewport");
if (!/<label[^>]+for="api-key"/.test(html)) failures.push("API key input needs a label");
if (!/aria-label="Payment detail"/.test(html)) failures.push("detail panel needs an aria-label");
if (!/aria-label="Close"/.test(html)) failures.push("close button needs an aria-label");

/* ── The search box (#693, focused by `/` in #721) ─────────────────────── */

if (!/<label[^>]+for="search"/.test(html)) {
  failures.push("search input needs a <label for=\"search\">");
}
if (!/id="search-clear"[\s\S]*?aria-label="Clear search"/.test(html)) {
  failures.push("the search clear button needs an aria-label (it renders as a bare ×)");
}

/* ── The shortcut help overlay (#721) ──────────────────────────────────── */

if (!/<div[^>]+id="help"[\s\S]*?role="dialog"/.test(html)) {
  failures.push("#help must be a role=\"dialog\" so it is announced as one");
}
if (!/aria-modal="true"/.test(html)) failures.push("#help needs aria-modal=\"true\"");
if (!/aria-labelledby="help-title"/.test(html)) {
  failures.push("#help needs aria-labelledby pointing at its heading");
}
if (!/id="help-title"/.test(html)) failures.push("#help needs an id on its heading");
if (!/id="help-open"[\s\S]*?aria-haspopup="dialog"/.test(html)) {
  failures.push("the shortcut button needs aria-haspopup=\"dialog\"");
}
if (!/id="rows-status"[\s\S]*?aria-live="polite"/.test(html)) {
  failures.push(
    "#rows-status needs aria-live=\"polite\": the j/k highlight is otherwise a " +
      "purely visual change a screen-reader user cannot perceive"
  );
}

/* ── Loading and empty states (#719, #720) ─────────────────────────────── */

if (!/id="live-region"[\s\S]*?aria-live="polite"/.test(html)) {
  failures.push("#live-region needs aria-live=\"polite\" to announce load progress");
}
if (!/\.sr-only\s*\{/.test(css)) {
  failures.push(
    "the stylesheet needs a .sr-only rule: the live region must be hidden from " +
      "sight but still present in the accessibility tree"
  );
}
if (!/prefers-reduced-motion/.test(css)) {
  failures.push(
    "the skeleton shimmer must be behind a prefers-reduced-motion guard (#722)"
  );
}
for (const id of ["live-region", "rows", "list-error", "detail-fields"]) {
  if (!html.includes(`id="${id}"`)) failures.push(`missing #${id}`);
}

/* The overlay lists its shortcuts from the SHORTCUTS table rather than from
 * hand-written markup, so a documented shortcut cannot go stale — but the
 * table itself must still declare the ones the issue asks for. */
for (const key of ["r", "/", "j", "k", "Escape", "?"]) {
  if (!keys.includes(`"${key}"`)) failures.push(`keys.js does not bind "${key}"`);
}

/* ── Mobile card layout (#711) ────────────────────────────────────────── */

if (!/@media \(max-width: 720px\)/.test(css)) {
  failures.push("the stylesheet must keep a mobile breakpoint");
}
if (!/\.payments td::before\s*\{\s*content: attr\(data-label\)/.test(css)) {
  failures.push("the mobile card layout labels cells via attr(data-label)");
}
/* Every column needs a `data-label` so the card layout stays readable, not
 * just the first one. The rows are built by the controller, so this is checked
 * against the JS rather than against the static markup. */
const declaredLabels = new Set([
  ...[...js.matchAll(/setAttribute\("data-label", "([^"]+)"\)/g)].map((m) => m[1]),
  ...[...js.matchAll(/labelledCell\(\s*"([^"]+)"/g)].map((m) => m[1]),
]);
for (const label of ["Status", "Amount", "Memo", "Created", "Payment ID"]) {
  if (!declaredLabels.has(label)) {
    failures.push(`row cells must declare data-label="${label}" for the mobile card layout`);
  }
}

if (failures.length > 0) {
  console.error(failures.join("\n"));
  process.exit(1);
}

console.log("dashboard accessibility checks passed");
