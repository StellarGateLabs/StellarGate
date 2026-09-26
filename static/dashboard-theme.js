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
 */
(function () {
  "use strict";

  var KEY = "stellargate.theme";
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
