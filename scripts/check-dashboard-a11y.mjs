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

check(/<html[^>]+lang=/.test(html), "html element must declare lang");
check(/<meta[^>]+name="viewport"/.test(html), "missing responsive viewport");
check(/<label[^>]+for="api-key"/.test(html), "API key input needs a label");
check(/aria-label="Close"/.test(html), "close button needs an aria-label");

// ── The detail drawer is a real modal dialog (issue #715) ────────────────────

check(
  /<dialog[^>]*id="detail"/.test(html),
  "the detail drawer must be a <dialog> element, not an <aside> — only <dialog> " +
    "gets top-layer rendering, which is what makes the page behind it unreachable " +
    "rather than merely covered by a scrim",
);
check(
  /<dialog[^>]*aria-modal="true"/s.test(html),
  'a modal dialog must declare aria-modal="true"',
);
check(
  /<dialog[^>]*aria-labelledby="detail-title"/s.test(html) &&
    /<h2 id="detail-title">/.test(html),
  "the dialog must be named by its heading via aria-labelledby, and that heading " +
    "must exist — a dangling id announces a bare \"dialog\"",
);
check(
  !/id="scrim"/.test(html),
  "a modal <dialog> paints its own ::backdrop; the separate #scrim element is " +
    "dead weight and, being a sibling of the dialog, is not inert while it is open",
);
check(
  js.includes("showModal()"),
  "the drawer must be opened with showModal() — un-hiding a <dialog> leaves the " +
    "page behind it fully reachable by Tab and to the accessibility tree",
);
check(
  js.includes("detail.close()"),
  "the drawer must be closed with close(), which is what fires the `close` event " +
    "the focus restoration is bound to",
);
check(
  !/ev\.key === "Escape"/.test(js),
  "a modal <dialog> handles Escape itself and fires `cancel`; a document-level " +
    "Escape handler here would only ever be a second close() on a closed dialog",
);
check(
  js.includes('ev.target !== $("detail")'),
  "dismissing on a backdrop click keys off the click being retargeted to the " +
    "dialog element itself; without that check every click inside the panel would " +
    "close the drawer",
);

// ── Focus management in the drawer (issue #714) ──────────────────────────────

check(
  js.includes("function trapDetailFocus("),
  "the drawer needs a focus trap: a modal dialog does not wrap Tab at the ends " +
    "in any current browser",
);
check(
  js.includes('addEventListener("keydown", trapDetailFocus)'),
  "the trap must actually be bound to the drawer",
);
check(
  js.includes('addEventListener("close", onDetailClosed)'),
  "focus must be restored from the dialog's `close` event, so it happens for " +
    "every route out — button, backdrop, Escape and a re-open alike",
);
check(
  js.includes('addEventListener("focusin"'),
  "focus landing outside the open drawer by a non-keyboard route (a click on the " +
    "page behind it) must be pulled back",
);
check(
  js.includes("trigger.isConnected"),
  "restoring focus must be guarded on isConnected, or a re-render mid-drawer " +
    "drops focus to <body> — the loss this exists to prevent",
);
check(
  js.includes("tr.tabIndex = 0;"),
  "payment rows must be focusable or there is nowhere to return focus to",
);

// ── Theme override (issue #713) ──────────────────────────────────────────────

// Comments name these attributes and selectors, so strip them before matching.
const markup = html.replace(/<!--[\s\S]*?-->/g, "");
const style = readFileSync("static/dashboard.css", "utf8");

check(
  (markup.match(/data-theme-toggle/g) || []).length === 2,
  "the theme override needs a control on both the sign-in gate and the top bar — " +
    "whichever panel is on screen should offer it",
);
check(
  /aria-pressed=/.test(markup),
  "the theme toggle is a toggle button, so its state must be exposed with " +
    "aria-pressed; its name has to stay constant for that to mean anything",
);
check(
  /class="visually-hidden"/.test(markup),
  "an icon-only control needs a visually-hidden text label or it has no " +
    "accessible name at all",
);
check(
  /<script[^>]+src="\/dashboard\/theme\.js"/.test(markup) &&
    html.indexOf("/dashboard/theme.js") < html.indexOf("</head>"),
  "the stored theme must be applied by a classic script in <head>; a module is " +
    "deferred and so runs after the first paint, flashing the wrong theme",
);
check(
  /html\[data-theme="dark"\]/.test(style),
  "the dark palette must be reachable from the data-theme attribute, not only " +
    "from prefers-color-scheme — otherwise the override does nothing",
);
check(
  /prefers-color-scheme: dark/.test(style),
  "with no stored preference the OS setting must still decide",
);
check(
  /:root:not\(\[data-theme\]\)/.test(style),
  'the OS palette must be scoped to :root:not([data-theme]), or a stored "light" ' +
    "choice loses to an OS preference of dark",
);
check(
  /color-scheme:\s*dark/.test(style) && /color-scheme:\s*light/.test(style),
  "an explicit override must also set color-scheme, or the native controls (the " +
    "date pickers, the <select>, the scrollbar) keep the OS palette",
);

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
