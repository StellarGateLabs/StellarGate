/* StellarGate dashboard - theme bootstrap.
 *
 * A classic (non-module, non-deferred) script in <head>, so it runs before the
 * first paint. That is the whole point of it being separate from app.js: a
 * module is deferred by definition, so the page would paint once with the
 * `prefers-color-scheme` palette and then repaint in the stored one — a visible
 * flash on every load for anyone who chose a theme that differs from their
 * operating system's. (Issue #713.)
 *
 * It deliberately does not touch the API key. Nothing here reads or writes
 * storage other than the single theme preference below, and no value from it
 * ever reaches a URL, so a key stored in the same origin's localStorage cannot
 * leak through this path.
 *
 * CSP note: `script-src 'self'` allows this file (it is served from the same
 * origin by the gateway) and an inline script would not be, so the
 * `data-theme` attribute is set from a real file rather than a <script> block.
 *
 * Design tokens (issue #685): the colour and spacing custom properties live in
 * dashboard.css on `:root`, and the dark palette is selected by the
 * `data-theme="dark"` attribute this file sets. The attribute is therefore the
 * single switch that flips every token, which is why the resolved theme is
 * pinned here rather than left to `prefers-color-scheme` alone.
 *
 * Status tokens (issues #821, #822): status pills must not rely on colour
 * alone. Each status therefore has a matching icon/shape (rendered from CSS
 * masks in dashboard.css, so no extra markup or network request is needed) and
 * a per-status icon colour token. The token set is identical in the light and
 * dark palettes — only the values differ — so the same markup renders in both:
 *
 *   --status-ok-fg / --status-ok-icon
 *   --status-warn-fg / --status-warn-icon
 *   --status-error-fg / --status-error-icon
 *   --status-info-fg / --status-info-icon
 *   --status-neutral-fg / --status-neutral-icon
 *
 * The icon tokens are chosen to stay distinguishable under the common forms of
 * colour blindness (deuteranopia, protanopia, tritanopia) because the shape,
 * not the hue, carries the meaning. In `forced-colors: active` (Windows High
 * Contrast and equivalents) dashboard.css overrides these with system colours
 * (`CanvasText`, `Highlight`, `LinkText`, ...) and keeps focus rings visible
 * via `outline-color: Highlight`, so nothing here needs to detect that mode.
 *
 * None of these tokens are read or written by this file; they are documented
 * here only so the token set stays discoverable next to the theme switch.
 */
(function () {
  "use strict";

  // The settings panel (#771) persists the theme through state.js savePrefs,
  // i.e. under PREFS_NAMESPACE + "theme". Reading any other key here means the
  // stored choice is never seen before first paint and the #713 flash returns.
  var KEY = "stellargate.prefs.theme";
  var root = document.documentElement;

  /**
   * The stored preference, or null when the operator has not chosen one.
   *
   * Anything that is not exactly "light" or "dark" is treated as absent, so a
   * value left behind by an older build, or hand-edited in devtools, falls back
   * to following the operating system instead of pinning the page to a
   * half-understood theme.
   */
  function stored() {
    try {
      var value = window.localStorage.getItem(KEY);
      return value === "light" || value === "dark" ? value : null;
    } catch (e) {
      // Storage can be entirely unavailable (Safari private browsing, a strict
      // cookie policy, a file:// origin). The page still works; it just follows
      // the OS for this load.
      return null;
    }
  }

  var theme = stored();

  /* No attribute at all means "no choice made", and the CSS falls through to
   * `@media (prefers-color-scheme: dark)`. Pinning a resolved value here
   * instead would freeze today's OS setting into a permanent override the
   * operator never asked for. */
  if (theme) {
    root.setAttribute("data-theme", theme);
  }
})();