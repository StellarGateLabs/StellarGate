// WCAG 2.1 AA contrast audit for the dashboard palettes (issue #718).
//
// Dependency-free on purpose: the dashboard has no npm, no bundler and no
// node_modules, and a colour audit that needs `npm install` to run is one that
// stops being run. The maths is short enough to own — sRGB → linear → relative
// luminance → contrast ratio, per WCAG 2.1 and the sRGB transfer function in
// IEC 61966-2-1.
//
// It reads the real `static/dashboard.css` rather than a copy, so a token
// cannot pass here and fail on the page. Every pair the page actually paints is
// declared below with the role it plays, in both themes.
//
// Thresholds:
//   1.4.3 Contrast (Minimum), AA — 4.5:1 for normal text, 3:1 for large text
//        (>= 18.66px bold or >= 24px). Every text pair here is normal text.
//   1.4.11 Non-text Contrast, AA — 3:1 for the boundary of a user-interface
//        component and for anything needed to identify a state. That is what
//        the control edges and the focus ring are judged against.
//
// Text is not the only thing that fails AA quietly: a 1.3:1 border on a white
// card is invisible to a sighted user and unusable to a low-vision one, and it
// is exactly the sort of value that survives a review because it looks fine.

import { readFileSync } from "node:fs";

const css = readFileSync("static/dashboard.css", "utf8");

// ── colour maths ─────────────────────────────────────────────────────────────

/** Parse `#rgb`, `#rrggbb` into `[r, g, b]`, 0-255. */
function parseHex(value) {
  const hex = value.trim().replace(/^#/, "");
  if (/^[0-9a-f]{3}$/i.test(hex)) {
    return [0, 1, 2].map((i) => parseInt(hex[i] + hex[i], 16));
  }
  if (/^[0-9a-f]{6}$/i.test(hex)) {
    return [0, 2, 4].map((i) => parseInt(hex.slice(i, i + 2), 16));
  }
  throw new Error(`not a hex colour: ${value}`);
}

/** One sRGB channel, 0-255, to linear light. */
function channel(value) {
  const c = value / 255;
  return c <= 0.04045 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
}

/** WCAG relative luminance. */
function luminance(rgb) {
  const [r, g, b] = rgb.map(channel);
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}

/** WCAG contrast ratio between two opaque colours: (L1 + 0.05) / (L2 + 0.05). */
function contrast(a, b) {
  const la = luminance(parseHex(a));
  const lb = luminance(parseHex(b));
  const [hi, lo] = la > lb ? [la, lb] : [lb, la];
  return (hi + 0.05) / (lo + 0.05);
}

// Self-test. A contrast checker whose own maths is wrong is worse than none: it
// reports a green tick on colours nobody can read. These are the worked examples
// from the WCAG 2.x definitions of relative luminance and contrast ratio, so a
// refactor of the maths above cannot quietly change what "4.5:1" means here.
{
  const selfTest = [
    ["#ffffff", "#ffffff", 1, "white on white"],
    ["#000000", "#ffffff", 21, "black on white, the 21:1 maximum"],
    ["#777777", "#ffffff", 4.48, "the canonical mid-grey boundary case"],
    ["#0000ff", "#ffffff", 8.59, "pure blue on white"],
    ["#ffff00", "#ffffff", 1.07, "yellow on white, which cannot pass at any size"],
  ];
  for (const [fg, bg, expected, label] of selfTest) {
    const got = contrast(fg, bg);
    if (Math.abs(got - expected) > 0.02) {
      console.error(
        `contrast self-test failed: ${label} — ${fg} on ${bg} computed as ` +
          `${got.toFixed(2)}:1, expected about ${expected}:1. The maths is wrong, ` +
          `so nothing below it can be trusted.`,
      );
      process.exit(1);
    }
  }
}

// ── reading the stylesheet ───────────────────────────────────────────────────

/**
 * The custom properties each theme block declares.
 *
 * Blocks are found by selector rather than by position so adding a component
 * cannot shift the parse. `:root` is the light palette; `:root:not([data-theme])`
 * is the OS dark palette (#712); the two `html[data-theme]` blocks are the
 * explicit overrides (#713). All four are audited, which is what catches a
 * colour edited in one and not another — the failure mode that leaves a
 * palette with exactly one accessible theme.
 */
function block(selector) {
  const escaped = selector.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  const re = new RegExp(`(?:^|[\\s,}])${escaped}\\s*\\{([\\s\\S]*?)\\n\\}`, "m");
  const m = css.match(re);
  if (!m) throw new Error(`no block found for selector: ${selector}`);
  const props = {};
  for (const line of m[1].split("\n")) {
    const kv = line.match(/^\s*(--[a-z-]+)\s*:\s*(#[0-9a-fA-F]{3,6})\s*;/);
    if (kv) props[kv[1]] = kv[2];
  }
  return props;
}

const THEMES = {
  light: block(":root"),
  dark: block(":root:not([data-theme])"),
  "light (explicit override)": block('html[data-theme="light"]'),
  "dark (explicit override)": block('html[data-theme="dark"]'),
};

// ── the audit ────────────────────────────────────────────────────────────────

/**
 * Every text pair the page paints, in both themes.
 *
 * `role` is carried into the failure message because that is what tells a
 * reader whether a miss is a text-legibility problem (1.4.3) or a
 * component-boundary one (1.4.11) — different thresholds, different fixes.
 */
const PAIRS = [
  // Body and surface text.
  { fg: "--text", bg: "--bg", min: 4.5, role: "body text on the page" },
  { fg: "--text", bg: "--surface", min: 4.5, role: "text on a card, the table and the drawer" },
  { fg: "--muted", bg: "--bg", min: 4.5, role: "muted text on the page" },
  { fg: "--muted", bg: "--surface", min: 4.5, role: "column headers, field labels, delivery metadata" },
  { fg: "--accent", bg: "--bg", min: 4.5, role: "the brand star in the top bar" },
  { fg: "--accent", bg: "--surface", min: 4.5, role: "the transaction explorer link" },

  // Buttons.
  { fg: "--text", bg: "--surface", min: 4.5, role: "label on a default button and a text input" },
  { fg: "--accent-text", bg: "--accent", min: 4.5, role: "label on the primary button and the active filter chip" },

  // Status pills. These are the ones most likely to fail: a light tinted
  // background with a mid-tone foreground is the classic AA miss, and status
  // colour is exactly the thing that must not be hard to read.
  { fg: "--ok", bg: "--ok-bg", min: 4.5, role: "completed / delivered pill" },
  { fg: "--warn", bg: "--warn-bg", min: 4.5, role: "pending / underpaid pill" },
  { fg: "--err", bg: "--err-bg", min: 4.5, role: "expired / failed pill and the error banner" },
  { fg: "--idle", bg: "--idle-bg", min: 4.5, role: "idle / unknown-status pill" },
  { fg: "--ok", bg: "--surface", min: 4.5, role: "a pill against the card it sits on" },
  { fg: "--warn", bg: "--surface", min: 4.5, role: "a pill against the card it sits on" },
  { fg: "--err", bg: "--surface", min: 4.5, role: "a pill against the card it sits on" },
  { fg: "--idle", bg: "--surface", min: 4.5, role: "a pill against the card it sits on" },
];

/**
 * Non-text pairs, judged at 3:1 (WCAG 1.4.11).
 *
 * A control's boundary has to be distinguishable from what is behind it, or the
 * control is not identifiable. That covers the button and input edges, and —
 * the one the issue calls out by name — the focus ring, which until now was
 * whatever the user agent drew: usually a hairline in a colour chosen by the
 * browser, not by this stylesheet and not checked against either theme.
 *
 * `--border` is deliberately absent. It draws decorative separators — table row
 * rules, delivery card edges, the top bar's hairline — and 1.4.11 is not about
 * those: nothing about a control is identified by the line between two table
 * rows, and requiring 3:1 there would put a heavy rule through every row of the
 * payments table. The token that *is* gated is `--control-border`, which draws
 * the boundary of something interactive.
 *
 * `--focus` is not paired against `--accent` either, and that follows from
 * `outline-offset: 2px` rather than from an oversight: the offset puts a gap of
 * the surrounding background between the ring and the control's own fill, so
 * the ring's adjacent colour is the page or the card, never the accent. Drop
 * the offset and that becomes a real gap in the audit;
 * `tests/dashboard_asset_tests.rs` pins it in place.
 */
const NON_TEXT = [
  { fg: "--control-border", bg: "--surface", min: 3, role: "button and input edges on a card, the table and the drawer" },
  { fg: "--control-border", bg: "--bg", min: 3, role: "a control edge against the page" },
  { fg: "--focus", bg: "--surface", min: 3, role: "focus ring on a card, the table and the drawer" },
  { fg: "--focus", bg: "--bg", min: 3, role: "focus ring on the page" },
  { fg: "--focus", bg: "--ok-bg", min: 3, role: "focus ring beside a completed pill" },
  { fg: "--focus", bg: "--warn-bg", min: 3, role: "focus ring beside a pending pill" },
  { fg: "--focus", bg: "--err-bg", min: 3, role: "focus ring beside an expired pill" },
  { fg: "--focus", bg: "--idle-bg", min: 3, role: "focus ring beside an idle pill" },
];

const failures = [];
const rows = [];

for (const [theme, tokens] of Object.entries(THEMES)) {
  for (const pair of [...PAIRS, ...NON_TEXT]) {
    const fg = tokens[pair.fg];
    const bg = tokens[pair.bg];
    if (!fg || !bg) {
      failures.push(
        `${theme}: ${pair.fg} / ${pair.bg} — token not defined in this theme ` +
          `(fg=${fg ?? "missing"}, bg=${bg ?? "missing"}); ${pair.role}`,
      );
      continue;
    }
    const ratio = contrast(fg, bg);
    const ok = ratio >= pair.min;
    rows.push({ theme, label: pair.role, fg, bg, ratio, min: pair.min, ok });
    if (!ok) {
      failures.push(
        `${theme}: ${pair.role} — ${fg} on ${bg} is ${ratio.toFixed(2)}:1, ` +
          `needs ${pair.min}:1 (${pair.fg} / ${pair.bg})`,
      );
    }
  }
}

// Every theme must declare the same tokens, or a colour added to one palette
// silently falls back to another theme's value — which is how a token ends up
// unreadable in exactly the theme nobody was looking at. This is also what would
// catch `--skeleton-base` and `--skeleton-shine` (#719) being added to `:root`
// and forgotten in the dark blocks.
{
  const base = Object.keys(THEMES.light).sort();
  for (const [theme, tokens] of Object.entries(THEMES)) {
    const missing = base.filter((t) => !(t in tokens));
    if (missing.length) {
      failures.push(
        `${theme}: does not define ${missing.join(", ")}; every theme must declare ` +
          `the same tokens or a colour resolves to another theme's value`,
      );
    }
  }
}

if (process.argv.includes("--verbose")) {
  for (const r of rows) {
    console.log(
      `${r.ok ? "ok  " : "FAIL"}  ${r.theme.padEnd(26)} ${r.ratio.toFixed(2).padStart(6)}:1 ` +
        `(min ${r.min})  ${r.fg} on ${r.bg} — ${r.label}`,
    );
  }
  console.log("");
}

if (failures.length > 0) {
  console.error(
    `${failures.length} contrast failure(s) against WCAG 2.1 AA:\n\n${failures.join("\n\n")}`,
  );
  process.exit(1);
}

console.log(
  `dashboard contrast checks passed (${rows.length} pairs across ${Object.keys(THEMES).length} theme blocks)`,
);
