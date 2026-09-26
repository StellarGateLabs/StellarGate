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

// ── Focus management in the drawer (issue #714) ──────────────────────────────

check(
  /id="detail"[^>]*tabindex="-1"/.test(html),
  'the drawer must carry tabindex="-1" so focus can be moved into it; a positive ' +
    "tabindex would instead add a phantom stop in front of the controls it contains",
);
check(
  js.includes("function trapDetailFocus("),
  "the drawer needs a focus trap, or Tab walks into the page behind it",
);
check(
  js.includes('addEventListener("keydown", trapDetailFocus)'),
  "the trap must actually be bound to the drawer",
);
check(
  js.includes('addEventListener("focusin"'),
  "focus landing outside the open drawer by a non-keyboard route (a click on the " +
    "page behind it) must be pulled back",
);
check(
  js.includes("trigger.isConnected"),
  "closing the drawer must return focus to the row that opened it, guarded on " +
    "isConnected so a re-render mid-drawer cannot drop focus to <body>",
);
check(
  js.includes("tr.tabIndex = 0;"),
  "payment rows must be focusable or there is nowhere to return focus to",
);
check(
  js.includes('ev.key === "Escape"'),
  "the drawer must close on Escape",
);

if (failures.length > 0) {
  console.error(failures.join("\n"));
  process.exit(1);
}

console.log("dashboard accessibility checks passed");
