// Dependency-free accessibility smoke check for the embedded dashboard.
//
// It cannot replace a real audit — no browser, no screen reader, no colour
// maths — but it catches the class of regression that is easy to reintroduce
// and invisible in review: a control that lost its label, a landmark that
// stopped being a landmark, an id the script depends on that the markup
// dropped. `scripts/check-dashboard-contrast.mjs` covers the colour axis.

import { readFileSync } from "node:fs";

const html = readFileSync("static/dashboard.html", "utf8");
const js = readFileSync("static/dashboard.js", "utf8");
const failures = [];

const check = (ok, message) => {
  if (!ok) failures.push(message);
};

// ── Structure and labels ─────────────────────────────────────────────────────

for (const id of ["api-key", "gate-form", "detail-close", "rows", "detail"]) {
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

if (failures.length > 0) {
  console.error(failures.join("\n"));
  process.exit(1);
}

console.log("dashboard accessibility checks passed");
