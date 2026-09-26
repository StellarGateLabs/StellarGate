import { readFileSync } from "node:fs";

const html = readFileSync("static/dashboard.html", "utf8");
const failures = [];

for (const id of ["api-key", "gate-form", "detail-close", "rows"]) {
  if (!html.includes(`id="${id}"`)) failures.push(`missing #${id}`);
}

if (!/<html[^>]+lang=/.test(html)) failures.push("html element must declare lang");
if (!/<meta[^>]+name="viewport"/.test(html)) failures.push("missing responsive viewport");
if (!/<label[^>]+for="api-key"/.test(html)) failures.push("API key input needs a label");
if (!/aria-label="Payment detail"/.test(html)) failures.push("detail panel needs an aria-label");
if (!/aria-label="Close"/.test(html)) failures.push("close button needs an aria-label");

if (failures.length > 0) {
  console.error(failures.join("\n"));
  process.exit(1);
}

console.log("dashboard accessibility checks passed");
