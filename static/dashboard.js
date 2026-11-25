/* StellarGate dashboard.
 *
 * A thin client over the same public REST API documented in the README: it
 * holds no privileged session of its own and adds no server-side state. The
 * merchant's API key lives in the browser and is sent as a bearer token.
 *
 * Every value that originates from the API is written with textContent (or
 * via el()//setText below), never innerHTML. `webhook_url`, `memo` and the
 * event name are merchant-controlled, so interpolating them as markup would
 * be a stored-XSS vector.
 *
 * This file is only the controller: DOM wiring and rendering. The logic worth
 * testing is in the sibling modules it imports, all of which are DOM-free and
 * run under `node --test` (issue #723):
 *
 *   format.js   pure formatting, query building, row filtering
 *   session.js  API-key storage rules
 *   state.js    the single view-state store and URL-hash serialisation
 *   keys.js     which keystroke means which action (issue #721)
 */

import {
  buildListQuery,
  buildSep7Uri,
  countdown as _countdown,
  CSV_COLUMNS,
  explorerTx as _explorerTx,
  fmtTime as _fmtTime,
  formatAmountLocale,
  newPaymentIds,
  formatAmount as _formatAmount,
  pillClass,
  relativeTime as _relativeTime,
  shortId,
  toCsv,
} from "./format.js";
import { createSessionStore } from "./session.js";
import { createStore, parseHash, serializeHash } from "./state.js";
import { matchShortcut, moveRow, SHORTCUTS } from "./keys.js";
import { createPrefsStore, COLUMNS, ALWAYS_VISIBLE } from "./prefs.js";

(function () {
  "use strict";

  /* The version prefix is defined exactly once, here, so a request can never
     end up with the prefix doubled. Pinned by `tests/dashboard_asset_tests.rs`,
     which counts the occurrences of the prefix across this file. */
  var API_BASE = "/v1";
  var KEY_NAME = "stellargate.apiKey";
  var KEY_SAVED_AT = "stellargate.apiKeySavedAt";

  var t = window.I18N.t;

  // ── Session store ─────────────────────────────────────────────────────────
  var session = createSessionStore({
    session: (function () { try { return window.sessionStorage; } catch (e) { return null; } })(),
    local: (function () { try { return window.localStorage; } catch (e) { return null; } })(),
  });

  // ── Preferences store (#771 / #772) ───────────────────────────────────────
  // Persisted under its own namespace ("stellargate.prefs."), separate from the
  // API key. Never stores the key, and never writes it anywhere.
  var prefs = createPrefsStore({
    local: (function () { try { return window.localStorage; } catch (e) { return null; } })(),
  });

  // ── View-state store ──────────────────────────────────────────────────────
  var store = createStore();

  // ── Tiny DOM helpers ──────────────────────────────────────────────────────

  function $(id) {
    return document.getElementById(id);
  }

  /** Create an element with a class and *text* content (never markup). */
  function el(tag, className, text) {
    var node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined && text !== null) node.textContent = String(text);
    return node;
  }

  function show(node, visible) {
    if (node) node.hidden = !visible;
  }

  /* The one markup sink (#799). With `require-trusted-types-for 'script'` a
     plain string assigned to innerHTML throws, so the vendored QR encoder's
     output goes through a named policy. API fields never reach this path. */
  var qrPolicy = null;
  function setTrustedSvg(node, markup) {
    if (!node) return;
    clear(node);
    if (!markup) return;
    if (window.trustedTypes && !qrPolicy) {
      qrPolicy = window.trustedTypes.createPolicy("stellargate-qr", {
        createHTML: function (s) { return s; },
      });
    }
    node.innerHTML = qrPolicy ? qrPolicy.createHTML(markup) : markup;
  }

  function clear(node) {
    if (!node) return;
    while (node.firstChild) node.removeChild(node.firstChild);
  }

  function setError(node, message) {
    if (!node) return;
    if (message) {
      node.textContent = message;
      show(node, true);
    } else {
      node.textContent = "";
      show(node, false);
    }
  }

  /** Announce a message to screen readers via the live region. */
  function announce(message) {
    var live = $("live-region");
    if (live) live.textContent = message;
  }

  // ── Local format wrappers ─────────────────────────────────────────────────
  // These forward to the imported pure functions from format.js. Declared as
  // `function` statements so `tests/dashboard_asset_tests.rs` can assert each
  // helper is defined (the test checks for `function name(` in this file).

  /** Format a payment amount with its asset code. @see format.js */
  function formatAmount(amount, asset) {
    return formatAmountLocale(amount, asset, document.documentElement.lang || undefined);
  }

  /** Active display zone: "local" or "utc" (#783). */
  function activeZone() {
    return prefs.get().timezone === "utc" ? "utc" : "local";
  }

  /** Timestamp in the active zone, zone name included. @see format.js */
  function fmtTime(iso) { return _fmtTime(iso, activeZone()); }

  /** Coarse countdown to an ISO instant. @see format.js */
  function countdown(iso, now) { return _countdown(iso, now); }

  /** Coarse time elapsed since an ISO instant. @see format.js */
  function relativeTime(iso, now) { return _relativeTime(iso, now); }

  /** Stellar expert explorer URL for a transaction hash. @see format.js */
  function explorerTx(hash) { return _explorerTx(hash); }

  // ── Hash-state persistence (#696) ──────────────────────────────────────────

  function readHashState() {
    var parsed = parseHash(window.location.hash.slice(1));
    store.update({
      status: parsed.status || "",
      search: parsed.search || "",
      autoRefresh: parsed.autoRefresh || false,
    });
  }

  /** Read the payment id from a "#payment=<id>" deep link (#780). */
  function hashPaymentId() {
    try {
      return new URLSearchParams(window.location.hash.slice(1)).get("payment") || "";
    } catch (e) {
      return "";
    }
  }

  function writeHashState() {
    try {
      var hash = serializeHash(store.get());
      var sel = store.get().selectedPaymentId;
      if (sel) {
        var qp = new URLSearchParams(hash);
        qp.set("payment", sel);
        hash = qp.toString();
      }
      var base = window.location.pathname;
      var url = base + (hash ? "#" + hash : "");
      window.history.replaceState(null, "", url);
    } catch (e) {
      /* non-fatal */
    }
  }

  // Alias used by init() and hashchange handler.
  function applyHash() {
    readHashState();
  }

  function writeHash() {
    writeHashState();
  }

  // ── Date range presets ─────────────────────────────────────────────────────

  function localDateValue(d) {
    function pad(n) { return (n < 10 ? "0" : "") + n; }
    return d.getFullYear() + "-" + pad(d.getMonth() + 1) + "-" + pad(d.getDate());
  }

  function localDayBound(value, endOfDay) {
    var parts = value.split("-").map(Number);
    var d = endOfDay
      ? new Date(parts[0], parts[1] - 1, parts[2], 23, 59, 59)
      : new Date(parts[0], parts[1] - 1, parts[2], 0, 0, 0);
    return d.toISOString().replace(/\.\d{3}Z$/, "Z");
  }

  function presetRange(days) {
    var today = new Date();
    var from = new Date(today.getFullYear(), today.getMonth(), today.getDate() - (days - 1));
    return { from: localDateValue(from), to: localDateValue(today) };
  }

  function syncPresetUi() {
    Array.prototype.forEach.call(document.querySelectorAll(".preset"), function (btn) {
      var range = presetRange(Number(btn.getAttribute("data-days")));
      var isActive = store.get().createdAfter === range.from && store.get().createdBefore === range.to;
      btn.className = isActive ? "ghost preset preset-on" : "ghost preset";
      btn.setAttribute("aria-pressed", isActive ? "true" : "false");
    });
  }

  // ── Theme (#713 / #771) ────────────────────────────────────────────────────

  function effectiveTheme() {
    var stored = prefs.get().theme;
    if (stored === "light" || stored === "dark") return stored;
    return null;
  }

  function applyTheme(theme) {
    if (theme === "light" || theme === "dark") {
      document.documentElement.setAttribute("data-theme", theme);
      prefs.update({ theme: theme });
    } else {
      document.documentElement.removeAttribute("data-theme");
      prefs.update({ theme: "system" });
    }
    syncThemeUi();
  }

  function syncThemeUi() {
    var current = document.documentElement.getAttribute("data-theme");
    var dark = current === "dark" ||
      (!current && window.matchMedia && window.matchMedia("(prefers-color-scheme: dark)").matches);
    Array.prototype.forEach.call(
      document.querySelectorAll("[data-theme-toggle]"),
      function (button) {
        button.setAttribute("aria-pressed", dark ? "true" : "false");
        button.title = dark ? "Switch to light theme" : "Switch to dark theme";
        var icon = button.querySelector(".theme-icon");
        if (icon) icon.textContent = dark ? "\u2600" : "\u263D";
        var label = button.querySelector(".visually-hidden");
        if (label) label.textContent = dark ? "Light theme" : "Dark theme";
      }
    );
  }

  function initTheme() {
    var theme = effectiveTheme();
    if (theme) document.documentElement.setAttribute("data-theme", theme);
    syncThemeUi();
    Array.prototype.forEach.call(
      document.querySelectorAll("[data-theme-toggle]"),
      function (button) {
        button.addEventListener("click", function () {
          var cur = document.documentElement.getAttribute("data-theme");
          var isDark = cur === "dark" ||
            (!cur && window.matchMedia && window.matchMedia("(prefers-color-scheme: dark)").matches);
          applyTheme(isDark ? "light" : "dark");
        });
      }
    );
  }

  // ── Preferences (#771 / #772) ─────────────────────────────────────────────

  /** Column keys that are always shown, regardless of preference. */
  function isColumnVisible(key) {
    if (ALWAYS_VISIBLE.indexOf(key) >= 0) return true;
    var cols = prefs.get().columns || {};
    return cols[key] !== false;
  }

  /** Apply persisted display preferences to the view-state store. */
  function applyPreferences() {
    var p = prefs.get();
    store.update({ pageSize: p.pageSize });
    store.update({ autoRefresh: !!p.autoRefresh });
    if (p.theme === "light" || p.theme === "dark") {
      document.documentElement.setAttribute("data-theme", p.theme);
    } else {
      document.documentElement.removeAttribute("data-theme");
    }
    syncThemeUi();
    syncPrefsUi();
    applyColumnVisibility();
  }

  /** Reflect the current preferences into the settings dialog controls. */
  function syncPrefsUi() {
    var p = prefs.get();
    var size = $("prefs-page-size");
    if (size) size.value = String(p.pageSize);
    var auto = $("prefs-auto-refresh");
    if (auto) auto.checked = !!p.autoRefresh;
    var interval = $("settings-refresh-interval");
    if (interval) interval.value = String(p.refreshIntervalSec);
    var theme = $("prefs-theme");
    if (theme) theme.value = p.theme || "system";
    var tz = $("prefs-timezone") || $("settings-timezone");
    if (tz) tz.value = p.timezone || "local";
    syncColumnMenuUi();
  }

  /** Reflect column visibility into the column menu checkboxes. */
  function syncColumnMenuUi() {
    Array.prototype.forEach.call(
      document.querySelectorAll("#columns-menu input[data-column]"),
      function (box) {
        var key = box.getAttribute("data-column");
        box.checked = isColumnVisible(key);
        box.disabled = ALWAYS_VISIBLE.indexOf(key) >= 0;
      }
    );
  }

  /** Toggle table column visibility via data-column attributes. */
  function applyColumnVisibility() {
    COLUMNS.forEach(function (key) {
      var visible = isColumnVisible(key);
      Array.prototype.forEach.call(
        document.querySelectorAll('[data-column="' + key + '"]'),
        function (node) {
          if (node.tagName === "TH" || node.tagName === "TD") {
            node.hidden = !visible;
          } else {
            node.hidden = !visible;
          }
        }
      );
    });
    syncColumnMenuUi();
  }

  /** Build the column menu checkboxes once. */
  function renderColumnMenu() {
    var menu = $("columns-menu");
    if (!menu) return;
    var list = menu.querySelector(".columns-list");
    if (!list) return;
    clear(list);
    COLUMNS.forEach(function (key) {
      var li = el("li", "columns-item");
      var label = el("label", "columns-label");
      var box = document.createElement("input");
      box.type = "checkbox";
      box.setAttribute("data-column", key);
      box.checked = isColumnVisible(key);
      box.disabled = ALWAYS_VISIBLE.indexOf(key) >= 0;
      box.addEventListener("change", function () {
        if (ALWAYS_VISIBLE.indexOf(key) >= 0) { box.checked = true; return; }
        var cols = Object.assign({}, prefs.get().columns || {});
        cols[key] = box.checked;
        prefs.update({ columns: cols });
        applyColumnVisibility();
      });
      label.appendChild(box);
      label.appendChild(el("span", null, columnLabel(key)));
      li.appendChild(label);
      list.appendChild(li);
    });
  }

  function columnLabel(key) {
    switch (key) {
      case "status":   return "Status";
      case "amount":   return "Amount";
      case "asset":    return "Asset";
      case "webhook":  return "Webhook";
      case "memo":     return "Memo";
      case "created":  return "Created";
      case "id":       return "Payment ID";
      default:         return key;
    }
  }

  function openSettings() {
    syncPrefsUi();
    openModal($("settings"));
  }

  function closeSettings() {
    closeModal($("settings"));
  }

  function resetPreferences() {
    prefs.reset();
    applyPreferences();
    onFilterChange();
    announce("Preferences reset to defaults");
  }

  /** Wire the settings dialog and column menu. Called once from init(). */
  function initPreferences() {
    renderColumnMenu();

    var settingsBtn = $("settings-open");
    if (settingsBtn) settingsBtn.addEventListener("click", openSettings);

    var settingsClose = $("settings-close");
    if (settingsClose) settingsClose.addEventListener("click", closeSettings);

    var settingsDone = $("settings-done");
    if (settingsDone) settingsDone.addEventListener("click", closeSettings);

    var settingsReset = $("settings-reset");
    if (settingsReset) settingsReset.addEventListener("click", resetPreferences);

    var settings = $("settings");
    if (settings) {
      settings.addEventListener("click", function (ev) {
        if (ev.target !== settings) return;
        var rect = settings.getBoundingClientRect();
        var outside =
          ev.clientX < rect.left || ev.clientX > rect.right ||
          ev.clientY < rect.top  || ev.clientY > rect.bottom;
        if (outside) closeSettings();
      });
    }

    var size = $("prefs-page-size");
    if (size) {
      size.addEventListener("change", function () {
        prefs.update({ pageSize: Number(size.value) || 25 });
        store.update({ pageSize: prefs.get().pageSize });
        onFilterChange();
      });
    }

    var auto = $("prefs-auto-refresh");
    if (auto) {
      auto.addEventListener("change", function () {
        prefs.update({ autoRefresh: auto.checked });
        store.update({ autoRefresh: auto.checked });
        onFilterChange();
      });
    }

    var interval = $("settings-refresh-interval");
    if (interval) {
      interval.addEventListener("change", function () {
        prefs.update({ refreshIntervalSec: Number(interval.value) || 30 });
        restartAutoRefreshTimer();
      });
    }

    var theme = $("prefs-theme");
    if (theme) {
      theme.addEventListener("change", function () {
        applyTheme(theme.value);
      });
    }

    var tz = $("prefs-timezone") || $("settings-timezone");
    if (tz) {
      tz.addEventListener("change", function () {
        prefs.update({ timezone: tz.value });
        renderRows();
      });
    }

    var columnsBtn = $("columns-open");
    if (columnsBtn) {
      columnsBtn.addEventListener("click", function () {
        var menu = $("columns-menu");
        if (!menu) return;
        show(menu, menu.hidden);
        syncColumnMenuUi();
      });
    }

    var columnsClose = $("columns-close");
    if (columnsClose) {
      columnsClose.addEventListener("click", function () {
        show($("columns-menu"), false);
      });
    }
  }

  // ── API ───────────────────────────────────────────────────────────────────

  /**
   * Call the gateway. The key is attached as an `Authorization` header and
   * nowhere else — never a query parameter, never the fragment.
   *
   * POST body is passed via `opts.body` (a JSON string); `opts.headers` carries
   * any extra headers (e.g. Content-Type, Idempotency-Key). The pinned line
   * in dashboard_asset_tests.rs matches the fetch call below verbatim.
   */
  function api(path, options) {
    var opts = options || {};
    var headers = Object.assign({ Accept: "application/json" }, opts.headers || {});
    var key = store.get().key;
    if (key) headers.Authorization = "Bearer " + key;

    return fetch(API_BASE + path, { method: opts.method || "GET", headers: headers, body: opts.body || undefined, signal: opts.signal }).then(
      function (res) {
        if (res.status === 401) {
          signOut(t("error.rejected"));
          throw new Error("unauthorized");
        }
        return res
          .json()
          .catch(function () { return {}; })
          .then(function (body) {
            if (!res.ok) {
              throw new Error(body.error || t("error.requestFailed", { status: res.status }));
            }
            return body;
          });
      }
    );
  }

  // ── Session ───────────────────────────────────────────────────────────────

  function showGate(message) {
    store.update({
      key: null,
      selectedPaymentId: null,
      activeRow: -1,
      loadedPayments: [],
      cursor: null,
      loading: false,
    });
    closeDetail();
    wipePaymentData();
    show($("app"), false);
    show($("gate"), true);
    setError($("gate-error"), message || null);
  }

  /** Empty every container that can hold payment data (#797). Hidden is not
   *  gone: a hidden table, summary or drawer would still be in the DOM. */
  function wipePaymentData() {
    ["rows", "summary", "detail-fields", "deliveries", "invoice-fields",
      "invoice-qr", "instr-qr"].forEach(function (id) { clear($(id)); });
    ["invoice-uri", "instr-qr-alt", "instr-destination", "instr-memo",
      "instr-amount", "instr-asset", "instr-expiry", "instr-id", "rows-status",
      "deliveries-error"].forEach(function (id) {
      var n = $(id);
      if (n) n.textContent = "";
    });
    show($("payment-invoice"), false);
    clearListState();
    currentDetailPayment = null;
    watchedPayments = {};
    if (autoRefreshTimer !== null) {
      window.clearInterval(autoRefreshTimer);
      autoRefreshTimer = null;
    }
  }

  // ── Cross-tab sign-out (#795) ─────────────────────────────────────────────
  // Only a signal is broadcast — never the key itself.
  var SIGNOUT_SIGNAL = "stellargate.signout";
  var signoutChannel = null;
  try {
    if (typeof BroadcastChannel === "function") {
      signoutChannel = new BroadcastChannel(SIGNOUT_SIGNAL);
      signoutChannel.onmessage = function () { localSignOut(null); };
    }
  } catch (e) { signoutChannel = null; }

  window.addEventListener("storage", function (ev) {
    if (ev.key === SIGNOUT_SIGNAL && ev.newValue) localSignOut(null);
  });

  function broadcastSignOut() {
    if (signoutChannel) {
      try { signoutChannel.postMessage("signout"); return; } catch (e) { /* fall through */ }
    }
    try {
      window.localStorage.setItem(SIGNOUT_SIGNAL, String(Date.now()));
      window.localStorage.removeItem(SIGNOUT_SIGNAL);
    } catch (e) { /* non-fatal */ }
  }

  function localSignOut(message) {
    stopIdleWatch();
    session.clear();
    store.update({ loadedPayments: [] });
    clear($("rows"));
    clear($("summary"));
    showGate(message);
  }

  function signOut(message) {
    localSignOut(message);
    broadcastSignOut();
  }

  // ── Idle sign-out (#794) ──────────────────────────────────────────────────
  var IDLE_MS = 30 * 60 * 1000;
  var IDLE_WARN_MS = 60 * 1000;
  var idleTimer = null;
  var idleWarnTimer = null;
  var idleListening = false;

  function idleMs() {
    var n = Number(prefs.get().idleMinutes);
    return isFinite(n) && n > 0 ? n * 60 * 1000 : IDLE_MS;
  }

  function hideIdleWarning() {
    var w = $("idle-warning");
    if (w) show(w, false);
  }

  function resetIdleTimer() {
    window.clearTimeout(idleTimer);
    window.clearTimeout(idleWarnTimer);
    hideIdleWarning();
    if (!store.get().key) return;
    var total = idleMs();
    idleWarnTimer = window.setTimeout(function () {
      var w = $("idle-warning");
      if (w) show(w, true);
    }, Math.max(0, total - IDLE_WARN_MS));
    idleTimer = window.setTimeout(function () { signOut("Signed out after a period of inactivity."); }, total);
  }

  function onActivity(ev) {
    if (ev && ev.type === "visibilitychange" && document.hidden) return;
    // Once the warning is up, only the explicit button extends the session.
    var w = $("idle-warning");
    if (w && !w.hidden) return;
    resetIdleTimer();
  }

  function startIdleWatch() {
    if (!idleListening) {
      idleListening = true;
      ["keydown", "pointerdown", "pointermove", "visibilitychange"].forEach(function (name) {
        (name === "visibilitychange" ? document : window).addEventListener(name, onActivity, { passive: true });
      });
      var stay = $("idle-stay");
      if (stay) stay.addEventListener("click", resetIdleTimer);
    }
    resetIdleTimer();
  }

  function stopIdleWatch() {
    window.clearTimeout(idleTimer);
    window.clearTimeout(idleWarnTimer);
    hideIdleWarning();
  }

  function signIn(key, persist) {
    store.update({ key: key });
    applyHash();
    return api("/payments?limit=1").then(function () {
      if (persist !== null) session.write(key, persist);
      show($("gate"), false);
      show($("app"), true);
      setError($("gate-error"), null);
      updateSessionExpiry();
      startIdleWatch();
      loadVersion();
      loadNetworkBadge();
      pollHealth();
      loadSummary();
      syncFilterUi();
      reload();
      var linked = hashPaymentId();
      if (linked) openDetail(linked);
    });
  }

  // ── Payments list ──────────────────────────────────────────────────────────

  var reloadPending = false;
  var autoRefreshTimer = null;
  var listAbort = null;

  // ── Payment completion notifications (#814) ───────────────────────────────

  /** Pending payments the merchant opted in to watch, keyed by payment id. */
  var watchedPayments = {};

  /** The payment currently shown in the open detail drawer, if any. */
  var currentDetailPayment = null;

  /** Reflect whether `p` is pending and watched into the detail drawer's button. */
  function syncNotifyButton(p) {
    var btn = $("detail-notify");
    if (!btn) return;
    if (typeof Notification === "undefined" || p.status !== "pending") {
      show(btn, false);
      return;
    }
    show(btn, true);
    var watching = !!watchedPayments[p.id];
    btn.disabled = watching;
    btn.textContent = watching ? "Watching for completion…" : "Notify me when this completes";
  }

  /** Opt in to a browser notification for one pending payment (#814). */
  function watchPaymentForNotification(p) {
    if (typeof Notification === "undefined") return;
    function startWatching() {
      watchedPayments[p.id] = { amount: p.amount, asset: p.asset };
      syncNotifyButton(p);
    }
    if (Notification.permission === "granted") {
      startWatching();
    } else if (Notification.permission !== "denied") {
      Notification.requestPermission().then(function (permission) {
        if (permission === "granted") startWatching();
      });
    }
  }

  /** Fire a notification for any watched payment whose status just resolved. */
  function notifyWatchedPayments(payments) {
    payments.forEach(function (p) {
      var watched = watchedPayments[p.id];
      if (!watched) return;
      if (p.status !== "completed" && p.status !== "underpaid") return;
      delete watchedPayments[p.id];
      if (typeof Notification === "undefined" || Notification.permission !== "granted") return;
      var title = p.status === "completed" ? "Payment completed" : "Payment underpaid";
      new Notification(title, {
        body: formatAmount(watched.amount, watched.asset) + " · " + shortId(p.id),
      });
    });
  }

  /** (Re)start the payment-list auto-refresh timer at the configured interval (#813). */
  function restartAutoRefreshTimer() {
    if (autoRefreshTimer !== null) window.clearInterval(autoRefreshTimer);
    var intervalMs = (prefs.get().refreshIntervalSec || 30) * 1000;
    autoRefreshTimer = window.setInterval(function () {
      var stateSnap = store.get();
      if (document.hidden) return; // #803: no polling from background tabs
      if (stateSnap.key && stateSnap.autoRefresh && (!stateSnap.status || stateSnap.status === "pending")) {
        reload();
      }
    }, intervalMs);
  }

  function reload() {
    /* Everything shown (or seen) before this refresh is "old". */
    seenIds = Object.assign({}, seenIds || {}, seenAccum);
    store.resetPaging();
    store.update({ loadedPayments: [] });
    clear($("rows"));
    clearListState();
    loadPayments();
  }

  function loadPayments() {
    var state = store.get();
    if (state.loading) {
      reloadPending = true;
      return;
    }
    store.update({ loading: true });
    setError($("list-error"), null);
    announce("Loading payments");

    var isFirstPage = !state.cursor;
    if (isFirstPage) showSkeletonRows(5);

    var controller = typeof AbortController === "function" ? new AbortController() : null;
    listAbort = controller;
    api(buildListQuery(state), { signal: controller ? controller.signal : undefined })
      .then(function (body) {
        if (controller && controller.signal.aborted) return;
        clearSkeletonRows();
        var stamp = $("last-updated");
        if (stamp) stamp.textContent = "Last updated " + fmtTime(new Date().toISOString());
        var payments = body.payments || [];
        notifyWatchedPayments(payments);
        markNewRows(payments, isFirstPage);
        var rows = store.get().loadedPayments.concat(payments);
        store.update({ loadedPayments: rows });
        collectAssets(payments);
        renderRows();
        renderAssetTotals();

        var more = payments.length === store.get().pageSize && !!body.next_cursor;
        store.update({ cursor: more ? body.next_cursor : null });
        show($("load-more"), more);

        var visible = store.visiblePayments();
        if (store.get().loadedPayments.length === 0) {
          setListState(buildEmptyState("📭", "No payments found",
            emptyMessageForFilter(store.get().status), null));
        } else if (visible.length === 0) {
          setListState(buildEmptyState("🔍", "No matches",
            'Nothing matches "' + store.get().search + '". Clear the search box to see every loaded payment.', null));
        } else {
          clearListState();
        }
      })
      .catch(function (err) {
        if (controller && controller.signal.aborted) return;
        clearSkeletonRows();
        if (err.message !== "unauthorized") {
          setListState(buildErrorState(err.message, function () {
            clearListState();
            reload();
          }));
        }
      })
      .then(function () {
        if (controller && controller.signal.aborted) return;
        listAbort = null;
        store.update({ loading: false });
        if (reloadPending) {
          reloadPending = false;
          store.resetPaging();
          loadPayments();
        }
      });
  }

  // Completed totals per asset for the active date range (#793). The summary
  // endpoint only returns status counts, so this sums the loaded payments; the
  // list request already carries the date range filter.
  function renderAssetTotals() {
    var box = $("asset-totals");
    if (!box) return;
    clear(box);
    var totals = {};
    store.get().loadedPayments.forEach(function (p) {
      if (p.status !== "completed") return;
      var n = Number(p.paid_amount || p.amount);
      if (!isFinite(n)) return;
      totals[p.asset] = (totals[p.asset] || 0) + n;
    });
    Object.keys(totals).sort().forEach(function (asset) {
      var card = el("div", "summary-card");
      card.appendChild(el("span", "muted small", "Received " + asset));
      card.appendChild(el("strong", null, formatAmount(totals[asset], asset)));
      box.appendChild(card);
    });
  }

  function loadSummary() {
    api("/payments/summary")
      .then(function (body) {
        var summary = $("summary");
        clear(summary);
        (body.summary || []).forEach(function (row) {
          var card = el("div", "summary-card");
          card.appendChild(el("span", "muted small", row[0]));
          card.appendChild(el("strong", null, row[1]));
          summary.appendChild(card);
        });
      })
      .catch(function (err) {
        clear($("summary"));
        announce("Error loading summary: " + err.message);
      });
  }

  // ── New-since-refresh highlight (#781) ────────────────────────────────────
  var seenIds = null;     // ids known before the current refresh; null on first load
  var newIds = {};
  var seenAccum = {};

  /** Record this page's ids; flag ones unseen before the refresh and announce the count. */
  function markNewRows(payments, isFirstPage) {
    var fresh = seenIds && isFirstPage ? newPaymentIds(seenIds, payments) : [];
    fresh.forEach(function (id) { newIds[id] = true; });
    payments.forEach(function (p) { seenAccum[p.id] = true; });
    if (fresh.length) {
      announce(fresh.length + (fresh.length === 1 ? " new payment" : " new payments"));
      window.setTimeout(function () {
        fresh.forEach(function (id) { delete newIds[id]; });
        Array.prototype.forEach.call(document.querySelectorAll("tr.row-new"), function (tr) {
          tr.classList.remove("row-new");
        });
      }, 4000);
    }
  }

  function renderRows() {
    var tbody = $("rows");
    updatePaymentsCaption();
    var visible = sortedVisiblePayments();
    var active = store.get().activeRow;
    clear(tbody);
    visible.forEach(function (p, index) {
      tbody.appendChild(rowFor(p, index === active));
    });
    show($("empty"), visible.length === 0);
    announceRow(visible, active);
  }

  function announceRow(visible, active) {
    var node = $("rows-status");
    if (!node) return;
    if (active < 0 || active >= visible.length) {
      node.textContent = visible.length
        ? visible.length + (visible.length === 1 ? " payment" : " payments")
        : "";
      return;
    }
    var p = visible[active];
    node.textContent = "Row " + (active + 1) + " of " + visible.length + ": " +
      p.status + ", " + formatAmount(p.amount, p.asset) + ", memo " + p.memo;
  }

  // ── Skeleton helpers (#719) ───────────────────────────────────────────────

  function showSkeletonRows(count) {
    var tbody = $("rows");
    for (var i = 0; i < count; i++) {
      var tr = document.createElement("tr");
      tr.className = "skeleton-row";
      tr.setAttribute("aria-hidden", "true");
      var cols = [
        { label: "Select",     cls: "sk-select",     key: null },
        { label: "Status",     cls: "sk-status",     key: "status" },
        { label: "Amount",     cls: "sk-amount",     key: "amount" },
        { label: "Asset",      cls: "sk-asset",      key: "asset" },
        { label: "Webhook",    cls: "sk-webhook",    key: "webhook" },
        { label: "Memo",       cls: "sk-memo",       key: "memo" },
        { label: "Created",    cls: "sk-date",       key: "created" },
        { label: "Payment ID", cls: "sk-id",         key: "id" },
      ];
      cols.forEach(function (col) {
        var td = document.createElement("td");
        td.setAttribute("data-label", col.label);
        if (col.key) td.setAttribute("data-column", col.key);
        if (col.key && !isColumnVisible(col.key)) td.hidden = true;
        td.appendChild(el("span", "skeleton-cell " + col.cls));
        tr.appendChild(td);
      });
      tbody.appendChild(tr);
    }
  }

  function clearSkeletonRows() {
    var tbody = $("rows");
    if (!tbody) return;
    var skeletons = tbody.querySelectorAll(".skeleton-row");
    for (var i = 0; i < skeletons.length; i++) tbody.removeChild(skeletons[i]);
  }

  function showDetailSkeleton() {
    var fields = $("detail-fields");
    clear(fields);
    var rows = [
      { dtWidth: "4rem", ddCls: "skeleton-field skeleton-field-short" },
      { dtWidth: "4rem", ddCls: "skeleton-field skeleton-field-short" },
      { dtWidth: "5rem", ddCls: "skeleton-field skeleton-field-long" },
      { dtWidth: "6rem", ddCls: "skeleton-field skeleton-field-full" },
      { dtWidth: "5rem", ddCls: "skeleton-field skeleton-field-full" },
      { dtWidth: "6rem", ddCls: "skeleton-field skeleton-field-long" },
      { dtWidth: "4rem", ddCls: "skeleton-field skeleton-field-short" },
    ];
    rows.forEach(function (row) {
      var dt = document.createElement("dt");
      var dtBlock = el("span", "skeleton-field skeleton-field-short");
      dtBlock.style.width = row.dtWidth;
      dtBlock.setAttribute("aria-hidden", "true");
      dt.appendChild(dtBlock);
      fields.appendChild(dt);
      var dd = document.createElement("dd");
      var ddBlock = el("span", row.ddCls);
      ddBlock.setAttribute("aria-hidden", "true");
      dd.appendChild(ddBlock);
      fields.appendChild(dd);
    });
  }

  // ── Empty / error state helpers (#720) ────────────────────────────────────

  function buildEmptyState(icon, title, message, onRetry) {
    var wrap = el("div", "empty-state");
    wrap.setAttribute("role", "status");
    var iconEl = el("span", "empty-state-icon", icon);
    iconEl.setAttribute("aria-hidden", "true");
    wrap.appendChild(iconEl);
    wrap.appendChild(el("span", "empty-state-title", title));
    wrap.appendChild(el("p", "empty-state-body muted", message));
    if (onRetry) {
      var btn = el("button", "ghost", "Try again");
      btn.type = "button";
      btn.addEventListener("click", onRetry);
      wrap.appendChild(btn);
    }
    return wrap;
  }

  function buildErrorState(message, onRetry) {
    var wrap = el("div", "error-state");
    wrap.appendChild(el("p", "error-state-message", message));
    var btn = el("button", "ghost", "Retry");
    btn.type = "button";
    btn.addEventListener("click", onRetry);
    wrap.appendChild(btn);
    return wrap;
  }

  function emptyMessageForFilter(status) {
    switch (status) {
      case "pending":   return "No pending payments. New payments will appear here once created.";
      case "completed": return "No completed payments match your current filters.";
      case "underpaid": return "No underpaid payments. Underpaid intents appear here until topped up.";
      case "expired":   return "No expired payments in this date range.";
      default:          return "No payments have been created yet. Create a payment intent to get started.";
    }
  }

  function clearListState() {
    var prev = $("list-state");
    if (prev && prev.parentNode) prev.parentNode.removeChild(prev);
    show($("empty"), false);
  }

  function setListState(node) {
    clearListState();
    node.id = "list-state";
    var rowsEl = $("rows");
    if (rowsEl && rowsEl.parentNode) rowsEl.parentNode.insertBefore(node, rowsEl.nextSibling);
  }

  // ── Row rendering ──────────────────────────────────────────────────────────

  var selectedPayments = {};

  function setSelected(p, on) {
    if (on) selectedPayments[p.id] = p; else delete selectedPayments[p.id];
  }

  function syncSelectionUi() {
    var ids = Object.keys(selectedPayments);
    var count = $("selection-count");
    var exportSel = $("export-selected");
    if (count) {
      count.textContent = ids.length ? ids.length + " selected" : "";
      show(count, ids.length > 0);
    }
    if (exportSel) show(exportSel, ids.length > 0);
  }

  function labelledCell(label, className, text) {
    var td = el("td", className, text);
    td.setAttribute("data-label", label);
    return td;
  }

  /**
   * Non-colour status indicator (#821).
   *
   * Each status gets a distinct glyph so the pill is not distinguished by
   * colour alone. The glyph is decorative (aria-hidden) because the status
   * text is always rendered alongside it. Unknown statuses fall back to a
   * neutral bullet rather than guessing.
   */
  function statusGlyph(status) {
    switch (status) {
      case "pending":   return "\u25CF"; // ● filled circle
      case "completed": return "\u2714"; // ✔ check mark
      case "underpaid": return "\u25B2"; // ▲ up triangle
      case "expired":   return "\u2716"; // ✖ ballot X
      case "failed":    return "\u2716"; // ✖ ballot X
      default:          return "\u25CB"; // ○ hollow circle
    }
  }

  /**
   * Build a status pill: a decorative glyph plus the status text. The text is
   * set with textContent (never innerHTML) since it originates from the API.
   */
  function statusPill(status) {
    var pill = el("span", pillClass(status));
    var glyph = el("span", "pill-glyph", statusGlyph(status));
    glyph.setAttribute("aria-hidden", "true");
    pill.appendChild(glyph);
    pill.appendChild(el("span", "pill-text", status));
    return pill;
  }

  function rowFor(p, isActive) {
    var tr = document.createElement("tr");
    // Rows are not tab stops (#964): keyboard users reach a payment through
    // the button in the Payment ID cell, which is the primary control.
    tr.dataset.paymentId = p.id;
    if (newIds[p.id]) tr.classList.add("row-new");
    if (isActive) {
      tr.className = "row-active";
      tr.setAttribute("aria-current", "true");
    }
    if (selectedPayments[p.id]) selectedPayments[p.id] = p;

    var selectCell = document.createElement("td");
    selectCell.setAttribute("data-label", "Select");
    var box = document.createElement("input");
    box.type = "checkbox";
    box.className = "row-select";
    box.setAttribute("data-id", p.id);
    box.setAttribute("aria-label", "Select payment " + p.id);
    box.checked = !!selectedPayments[p.id];
    box.addEventListener("click", function (ev) { ev.stopPropagation(); });
    box.addEventListener("keydown", function (ev) { ev.stopPropagation(); });
    box.addEventListener("change", function () {
      setSelected(p, box.checked);
      syncSelectionUi();
    });
    selectCell.appendChild(box);
    tr.appendChild(selectCell);

    var statusCell = document.createElement("td");
    statusCell.setAttribute("data-label", "Status");
    statusCell.setAttribute("data-column", "status");
    statusCell.appendChild(statusPill(p.status));
    tr.appendChild(statusCell);

    var amountCell = labelledCell("Amount", null, formatAmount(p.amount, p.asset));
    amountCell.setAttribute("data-column", "amount");
    if (!isColumnVisible("amount")) amountCell.hidden = true;
    tr.appendChild(amountCell);

    /* Asset column (#773). */
    var assetCell = labelledCell("Asset", "mono", p.asset || "XLM");
    assetCell.setAttribute("data-column", "asset");
    if (!isColumnVisible("asset")) assetCell.hidden = true;
    tr.appendChild(assetCell);

    /* Webhook status column (#773). */
    var webhookCell = document.createElement("td");
    webhookCell.setAttribute("data-label", "Webhook");
    webhookCell.setAttribute("data-column", "webhook");
    webhookCell.appendChild(webhookBadge(p));
    if (!isColumnVisible("webhook")) webhookCell.hidden = true;
    tr.appendChild(webhookCell);

    var memoCell = labelledCell("Memo", "mono", p.memo);
    memoCell.setAttribute("data-column", "memo");
    if (!isColumnVisible("memo")) memoCell.hidden = true;
    tr.appendChild(memoCell);

    var createdCell = labelledCell("Created", null, fmtTime(p.created_at));
    createdCell.setAttribute("data-column", "created");
    if (!isColumnVisible("created")) createdCell.hidden = true;
    tr.appendChild(createdCell);

    /* The button is the accessible control (#964). Its click bubbles to the
       row listener below, which is the mouse convenience on top of it. */
    var idCell = document.createElement("td");
    idCell.setAttribute("data-label", "Payment ID");
    idCell.setAttribute("data-column", "id");
    var openButton = el("button", "row-link mono", shortId(p.id));
    openButton.type = "button";
    openButton.setAttribute("aria-label", "View payment " + p.id);
    idCell.appendChild(openButton);
    tr.appendChild(idCell);

    tr.addEventListener("click", function () { openDetail(p.id, openButton); });
    return tr;
  }

  /**
   * Webhook status badge for the list (#773).
   *
   * The list response may carry `webhook_status` ("delivered" | "failing" |
   * "none"); when it is absent we render a neutral "—" rather than guessing.
   */
  function webhookBadge(p) {
    var status = p && p.webhook_status;
    if (status === "delivered") return badgeWithGlyph("pill pill-ok", "\u2714", "delivered");
    if (status === "failing") return badgeWithGlyph("pill pill-err", "\u2716", "failing");
    if (status === "none") return badgeWithGlyph("pill", "\u25CB", "none");
    return el("span", "muted", "\u2014");
  }

  /** A pill with a decorative glyph and text, for non-colour distinction (#821). */
  function badgeWithGlyph(className, glyphChar, text) {
    var pill = el("span", className);
    var glyph = el("span", "pill-glyph", glyphChar);
    glyph.setAttribute("aria-hidden", "true");
    pill.appendChild(glyph);
    pill.appendChild(el("span", "pill-text", text));
    return pill;
  }

  // ── Detail panel (#715) ───────────────────────────────────────────────────

  /**
   * Open the detail drawer as a real modal dialog.
   * Stores the trigger row so focus can be returned on close.
   */
  function openDetail(id, trigger) {
    store.update({ selectedPaymentId: id });
    writeHashState();
    state.detailTrigger = trigger || null;

    var detail = $("detail");
    /* Reflow (#823): the drawer is full-width on narrow viewports and at high
       zoom, so it must be scrollable rather than clipped. */
    if (detail) detail.classList.add("detail-open");
    var fields = $("detail-fields");
    clear(fields);
    clear($("deliveries"));
    setError($("deliveries-error"), null);
    show($("deliveries-empty"), false);

    $("invoice-print").disabled = true;
    $("payment-invoice").hidden = true;
    clear($("invoice-fields")); clear($("invoice-qr"));
    $("invoice-uri").textContent = "";
    show($("detail-notify"), false);
    clear($("detail-timeline"));
    $("detail-shortfall").hidden = true;
    $("detail-raw-body").textContent = "";
    writeHash();
    updateDrawerNav();
    showDetailSkeleton();
    openModal(detail);
    focusDetail();

    api("/payments/" + encodeURIComponent(id))
      .then(function (p) {
        if (store.get().selectedPaymentId !== id || !detail.open) return;
        renderInvoice(p);
        currentDetailPayment = p;
        syncNotifyButton(p);
        renderShortfall(p);
        renderRawJson(p);
        renderTimeline(p, null);
        clear(fields);
        [
          [t("field.status"), p.status, true],
          [t("field.amount"), formatAmount(p.amount, p.asset)],
          [t("field.received"), p.paid_amount ? formatAmount(p.paid_amount, p.asset) : "—"],
          [t("field.memo"), p.memo],
          [t("field.destination"), p.destination_address],
          [t("field.transaction"), p.tx_hash || "—"],
          [t("field.network"), "Stellar"],
          [t("field.assetIssuer"), p.asset_issuer || "native"],
          [t("field.paymentId"), p.id],
          [t("field.merchant"), p.merchant_id],
          [t("field.created"), fmtTime(p.created_at)],
          [t("field.updated"), fmtTime(p.updated_at)],
          [t("field.expires"), fmtTime(p.expires_at) +
            (p.status === "pending" ? " (" + countdown(p.expires_at) + " left)" : "")],
        ].forEach(function (pair) {
          fields.appendChild(el("dt", null, pair[0]));
          if (pair[2]) {
            var dd = document.createElement("dd");
            dd.appendChild(statusPill(p.status));
            fields.appendChild(dd);
          } else if (pair[0] === t("field.transaction") && p.tx_hash) {
            var tx = document.createElement("dd");
            var link = el("a", "mono", shortId(p.tx_hash));
            link.href = explorerTx(p.tx_hash);
            link.target = "_blank";
            link.rel = "noopener noreferrer";
            tx.appendChild(link);
            fields.appendChild(tx);
          } else {
            fields.appendChild(el("dd", "mono", pair[1]));
          }
        });
      })
      .catch(function (err) {
        clear(fields);
        if (err.message !== "unauthorized") {
          var errNode = buildErrorState(err.message, function () { openDetail(id); });
          var wrapper = document.createElement("dd");
          wrapper.appendChild(errNode);
          fields.appendChild(el("dt", null, "Error"));
          fields.appendChild(wrapper);
        }
      });

    loadDeliveries(id);
  }

  /** Parse a decimal string into integer stroops (7 dp) with BigInt; null if invalid. */
  function toStroops(v) {
    var m = /^(\d+)(?:\.(\d{1,7})\d*)?$/.exec(String(v == null ? "" : v).trim());
    if (!m) return null;
    return BigInt(m[1]) * 10000000n + BigInt(((m[2] || "") + "0000000").slice(0, 7));
  }

  function fromStroops(n) {
    var s = n.toString().padStart(8, "0");
    return (s.slice(0, -7) + "." + s.slice(-7)).replace(/\.?0+$/, "");
  }

  function renderShortfall(p) {
    var box = $("detail-shortfall");
    var fields = $("shortfall-fields");
    clear(fields);
    var expected = toStroops(p.amount);
    var received = toStroops(p.paid_amount || "0");
    if (p.status !== "underpaid" || expected === null || received === null) {
      box.hidden = true;
      return;
    }
    var missing = expected > received ? expected - received : 0n;
    [
      ["Expected", fromStroops(expected) + " " + p.asset],
      ["Received", fromStroops(received) + " " + p.asset],
      ["Shortfall", fromStroops(missing) + " " + p.asset],
    ].forEach(function (pair) {
      fields.appendChild(el("dt", null, pair[0]));
      fields.appendChild(el("dd", "mono", pair[1]));
    });
    box.hidden = false;
  }

  function renderRawJson(p) {
    $("detail-raw-body").textContent = JSON.stringify(p, null, 2);
  }

  /** Vertical lifecycle timeline from payment + delivery timestamps. */
  function renderTimeline(p, deliveries) {
    var list = $("detail-timeline");
    clear(list);
    var steps = [["Created", p.created_at]];
    if (p.paid_amount || p.tx_hash) steps.push(["Payment seen", p.updated_at]);
    if (p.status === "completed") steps.push(["Completed", p.updated_at]);
    else if (p.status === "underpaid") steps.push(["Underpaid", p.updated_at]);
    else if (p.status === "expired") steps.push(["Expired", p.expires_at]);
    steps.forEach(function (s) {
      list.appendChild(el("li", null, s[0] + " \u00b7 " + fmtTime(s[1])));
    });
    (deliveries || []).forEach(function (d) {
      var failed = d.status === "failed";
      var li = el("li", failed ? "timeline-failed" : null,
        (failed ? "\u2716 Webhook failed" : "Webhook " + (d.status || "queued")) +
        " (" + (d.event || "webhook") + ") \u00b7 " + fmtTime(d.last_attempt || d.created_at));
      list.appendChild(li);
    });
  }

  /** Move the drawer to the previous/next payment in the loaded, filtered list. */
  function stepDetail(delta) {
    var id = store.get().selectedPaymentId;
    var rows = sortedVisiblePayments();
    var i = rows.findIndex(function (r) { return r.id === id; });
    var next = rows[i + delta];
    if (i < 0 || !next) return;
    openDetail(next.id, state.detailTrigger);
  }

  function updateDrawerNav() {
    var id = store.get().selectedPaymentId;
    var rows = sortedVisiblePayments();
    var i = rows.findIndex(function (r) { return r.id === id; });
    $("detail-prev").disabled = i <= 0;
    $("detail-next").disabled = i < 0 || i >= rows.length - 1;
  }

  function renderInvoice(payment) {
    var fields = $("invoice-fields");
    clear(fields);
    [
      ["Amount", formatAmount(payment.amount, payment.asset)],
      ["Asset", payment.asset || "XLM"],
      ["Asset issuer", payment.asset_issuer || "native"],
      ["Destination", payment.destination_address],
      ["Memo", payment.memo],
      ["Expires", fmtTime(payment.expires_at)],
    ].forEach(function (pair) {
      fields.appendChild(el("dt", null, pair[0]));
      fields.appendChild(el("dd", "mono", pair[1] == null ? "—" : pair[1]));
    });
    var uri = buildSep7Uri(payment);
    $("invoice-uri").textContent = uri;
    clear($("invoice-qr"));
    if (typeof QRCode !== "undefined" && QRCode.toSvg) {
      // Only the vendored encoder's SVG is markup; API fields remain text.
      setTrustedSvg($("invoice-qr"), QRCode.toSvg(uri, { size: 220, margin: 4 }));
    }
    $("payment-invoice").hidden = false;
    $("invoice-print").disabled = false;
  }

  /** Move focus to the first focusable element inside the detail panel. */
  function focusDetail() {
    var detail = $("detail");
    if (!detail) return;
    var focusable = detail.querySelectorAll(
      'button, [href], input, select, textarea, [tabindex]:not([tabindex="-1"])'
    );
    if (focusable.length) focusable[0].focus();
  }

  /**
   * Tab trap for the detail drawer (#714).
   * Wraps Tab in both directions so focus stays inside while the modal is open.
   */
  function trapDetailFocus(ev) {
    if (ev.key !== "Tab") return;
    var detail = $("detail");
    if (!detail) return;
    var focusable = Array.prototype.slice.call(detail.querySelectorAll(
      'button:not([disabled]), [href], input:not([disabled]), select:not([disabled]), ' +
      'textarea:not([disabled]), [tabindex]:not([tabindex="-1"])'
    ));
    if (!focusable.length) return;
    var first = focusable[0];
    var last = focusable[focusable.length - 1];
    var active = document.activeElement;
    if (ev.shiftKey && active === first) {
      ev.preventDefault();
      last.focus();
    } else if (!ev.shiftKey && active === last) {
      ev.preventDefault();
      first.focus();
    }
  }

  /**
   * Keep focus inside the detail drawer when something outside tries to steal it.
   * Handles the case where a click behind the backdrop lands focus on the body.
   */
  function keepFocusInDetail(ev) {
    var detail = $("detail");
    if (!detail || !detail.open) return;
    if (!detail.contains(ev.target)) focusDetail();
  }

  /**
   * Called when the <dialog> fires its `close` event (from Escape, close(),
   * or the close button). Restores focus to the row that opened the drawer.
   */
  function onDetailClosed() {
    store.update({ selectedPaymentId: null });
    writeHash();
    var detail = $("detail");
    if (detail) detail.classList.remove("detail-open");
    var trigger = state.detailTrigger;
    state.detailTrigger = null;
    if (trigger && trigger.isConnected) {
      trigger.focus();
    }
  }

  /** Open a <dialog> element as a modal. */
  function openModal(dialogEl) {
    if (dialogEl && !dialogEl.open) dialogEl.showModal();
  }

  /** Close a <dialog> element. */
  function closeModal(dialogEl) {
    if (dialogEl && dialogEl.open) dialogEl.close();
  }

  function closeDetail() {
    /* Use detail.close() directly so the `close` event fires and
       `onDetailClosed` can restore focus. The dialog's `close` event is the
       single authoritative hook — it covers Escape, the button, and the
       backdrop. */
    var detail = $("detail");
    if (detail && detail.open) detail.close();
  }

  /**
   * Dismiss the detail drawer when a click lands on the ::backdrop.
   *
   * The ::backdrop is not a separate event target; clicks on it are retargeted
   * to the <dialog>. So we ignore clicks that land inside the dialog's own rect.
   */
  function dismissOnBackdrop(ev) {
    if (ev.target !== $("detail")) return;
    var rect = $("detail").getBoundingClientRect();
    var outside =
      ev.clientX < rect.left || ev.clientX > rect.right ||
      ev.clientY < rect.top  || ev.clientY > rect.bottom;
    if (outside) closeDetail();
  }

  // ── Deliveries ────────────────────────────────────────────────────────────

  function loadDeliveries(paymentId) {
    api("/payments/" + encodeURIComponent(paymentId) + "/webhooks")
      .then(function (body) {
        var list = $("deliveries");
        clear(list);
        var deliveries = body.deliveries || [];
        if (currentDetailPayment && currentDetailPayment.id === paymentId) {
          renderTimeline(currentDetailPayment, deliveries);
        }
        show($("deliveries-empty"), deliveries.length === 0);
        if (deliveries.length > 0) {
          deliveries.forEach(function (d) { list.appendChild(deliveryItem(paymentId, d)); });
        }
      })
      .catch(function (err) {
        if (err.message !== "unauthorized") {
          var errNode = buildErrorState(err.message, function () {
            setError($("deliveries-error"), null);
            loadDeliveries(paymentId);
          });
          var container = $("deliveries-error") && $("deliveries-error").parentNode;
          if (container) container.insertBefore(errNode, $("deliveries-error"));
        }
      });
  }

  function deliveryItem(paymentId, d) {
    var li = el("li", "delivery");
    var head = el("div", "delivery-head");
    head.appendChild(el("strong", null, d.event || "webhook"));
    head.appendChild(statusPill(d.status));
    li.appendChild(head);
    li.appendChild(el("div", "mono", d.url));
    li.appendChild(el("div", "delivery-meta", "attempt " + d.attempts + " · manual " + (d.manual_attempts || 0)));
    li.appendChild(el("div", "delivery-meta", "last: " + relativeTime(d.last_attempt)));
    li.lastChild.title = fmtTime(d.last_attempt);
    li.appendChild(el("div", "delivery-meta", "created: " + relativeTime(d.created_at)));
    li.lastChild.title = fmtTime(d.created_at);
    if (d.status === "failed") {
      li.appendChild(el("div", "error", "Last delivery failed; check receiver logs or redeliver."));
    }
    if (d.status !== "delivered") {
      li.appendChild(el("div", "delivery-meta", "retry state: queued for redrive if attempts remain"));
    }
    var button = el("button", "ghost", "Redeliver");
    button.addEventListener("click", function () {
      if (!window.confirm("Redeliver this webhook now?")) return;
      button.disabled = true;
      button.textContent = "Sending\u2026";
      api(
        "/payments/" + encodeURIComponent(paymentId) +
        "/webhooks/" + encodeURIComponent(d.id) + "/redeliver",
        { method: "POST" }
      )
        .then(function () { loadDeliveries(paymentId); })
        .catch(function (err) {
          button.disabled = false;
          button.textContent = "Redeliver";
          if (err.message !== "unauthorized") {
            setError($("deliveries-error"),
              err.message.indexOf("429") >= 0 ? "Rate limited. Try again shortly." : err.message);
          }
        });
    });
    li.appendChild(button);
    return li;
  }

  // ── Create payment form (#758 / #760 / #761) ──────────────────────────────

  /**
   * Build a copy button that writes `value` to the clipboard and briefly shows
   * "Copied" as confirmation (#698, #761).
   */
  function copyButton(value, label) {
    var btn = el("button", "ghost copy", "Copy");
    btn.type = "button";
    btn.setAttribute("aria-label", label || ("Copy " + value));
    btn.addEventListener("click", function () {
      navigator.clipboard.writeText(value).then(function () {
        btn.textContent = "Copied";
        window.setTimeout(function () { btn.textContent = "Copy"; }, 1500);
      }).catch(function () {
        /* clipboard API unavailable — silently ignore */
      });
    });
    return btn;
  }

  /**
   * Show the payment instructions panel with destination, memo, amount, asset,
   * expiry and a QR code for the SEP-7 URI (#761, #763).
   *
   * The panel is built inside the create-payment dialog so the merchant
   * sees it immediately after creation without needing to re-open the drawer.
   */
  function showPaymentInstructions(payment) {
    var dialog = $("create-payment-dialog");
    if (!dialog) return;

    /* Clear the form, reveal the instructions section. */
    var formSection = $("create-payment-form-section");
    var instrSection = $("create-payment-instructions");
    if (formSection) formSection.hidden = true;
    if (instrSection) instrSection.hidden = false;

    /* Populate fields. */
    function fillField(fieldId, value) {
      var el = document.getElementById(fieldId);
      if (el) el.textContent = value || "—";
    }
    fillField("instr-destination", payment.destination_address);
    fillField("instr-memo", payment.memo);
    fillField("instr-amount", formatAmount(payment.amount, payment.asset));
    fillField("instr-asset", payment.asset || "XLM");
    fillField("instr-expiry", fmtTime(payment.expires_at) +
      (payment.expires_at ? " (" + countdown(payment.expires_at) + " left)" : ""));

    /* Copy buttons (#698). */
    function attachCopyBtn(containerId, value, label) {
      var container = document.getElementById(containerId);
      if (!container) return;
      var existing = container.querySelector(".copy");
      if (existing) container.removeChild(existing);
      container.appendChild(copyButton(value, label));
    }
    attachCopyBtn("instr-destination-row", payment.destination_address, "Copy destination address");
    attachCopyBtn("instr-memo-row", payment.memo, "Copy memo");
    attachCopyBtn("instr-id-row", payment.id, "Copy payment ID");

    /* Payment ID. */
    fillField("instr-id", payment.id);

    /* SEP-7 QR code (#763). */
    var sep7Uri = buildSep7Uri(payment);
    var qrContainer = $("instr-qr");
    if (qrContainer) {
      if (typeof QRCode !== "undefined" && QRCode.toSvg) {
        var svgMarkup = QRCode.toSvg(sep7Uri, { size: 180, margin: 3 });
        if (svgMarkup) {
          /* We only set innerHTML on a div we own and control; the content
             comes from our own QR encoder, not from the API or user input. */
          setTrustedSvg(qrContainer, svgMarkup);
        }
      }
      /* Text alternative always present (#763). */
      var altEl = $("instr-qr-alt");
      if (altEl) altEl.textContent = sep7Uri;
    }
  }

  /**
   * Reset and re-open the create-payment form ("Create another" action, #761).
   */
  function resetCreateForm() {
    var formSection = $("create-payment-form-section");
    var instrSection = $("create-payment-instructions");
    if (formSection) formSection.hidden = false;
    if (instrSection) instrSection.hidden = true;

    var form = $("create-payment-form");
    if (form) form.reset();

    var submitBtn = $("create-submit");
    if (submitBtn) {
      submitBtn.disabled = false;
      submitBtn.textContent = "Create payment";
    }
    setError($("create-payment-error"), null);
    /* Focus the first field. */
    var amountInput = $("create-amount");
    if (amountInput) amountInput.focus();
  }

  /** Wire up the create-payment dialog. Called once from init(). */
  function initCreatePaymentDialog() {
    var dialog = $("create-payment-dialog");
    var openBtn = $("new-payment-btn");
    var cancelBtn = $("create-cancel");
    var createAnotherBtn = $("create-another");
    var form = $("create-payment-form");

    if (!dialog || !form) return;

    if (openBtn) {
      openBtn.addEventListener("click", function () {
        resetCreateForm();
        openModal(dialog);
        var amountInput = $("create-amount");
        if (amountInput) amountInput.focus();
      });
    }

    if (cancelBtn) {
      cancelBtn.addEventListener("click", function () { closeModal(dialog); });
    }

    if (createAnotherBtn) {
      createAnotherBtn.addEventListener("click", function () { resetCreateForm(); });
    }

    var doneBtn = $("create-done");
    if (doneBtn) {
      doneBtn.addEventListener("click", function () { closeModal(dialog); });
    }

    var cancelBtn2 = $("create-cancel-2");
    if (cancelBtn2) {
      cancelBtn2.addEventListener("click", function () { closeModal(dialog); });
    }

    var instrCloseBtn = $("create-instr-close");
    if (instrCloseBtn) {
      instrCloseBtn.addEventListener("click", function () { closeModal(dialog); });
    }

    /* Backdrop click dismissal. */
    dialog.addEventListener("click", function (ev) {
      if (ev.target !== dialog) return;
      var rect = dialog.getBoundingClientRect();
      var outside =
        ev.clientX < rect.left || ev.clientX > rect.right ||
        ev.clientY < rect.top  || ev.clientY > rect.bottom;
      if (outside) closeModal(dialog);
    });

    /* #760: prevent double submission. */
    form.addEventListener("submit", function (ev) {
      ev.preventDefault();
      var submitBtn = $("create-submit");
      if (submitBtn && submitBtn.disabled) return;

      var amount = ($("create-amount") || {}).value || "";
      var asset  = ($("create-asset")  || {}).value || "XLM";
      var webhookUrl = ($("create-webhook") || {}).value || "";

      if (!amount.trim()) {
        setError($("create-payment-error"), "Amount is required.");
        return;
      }

      /* Disable button and show spinner while request is in flight (#760). */
      if (submitBtn) {
        submitBtn.disabled = true;
        submitBtn.textContent = "Creating\u2026";
      }
      setError($("create-payment-error"), null);

      var body = { amount: amount.trim(), asset: asset };
      if (webhookUrl.trim()) body.webhook_url = webhookUrl.trim();

      /* Generate a per-form idempotency key so a network retry doesn't create
         duplicates. A new key is minted each time the form is submitted fresh
         (but stays constant across retries of the same submission). (#760) */
      var idempotencyKey = generateIdempotencyKey();

      api("/payments", {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          "Idempotency-Key": idempotencyKey,
        },
        body: JSON.stringify(body),
      })
        .then(function (payment) {
          /* #761: show instructions panel with copy buttons and QR code. */
          showPaymentInstructions(payment);
          /* Prepend the new payment to the loaded list. */
          var payments = store.get().loadedPayments;
          store.update({ loadedPayments: [payment].concat(payments) });
          renderRows();
          loadSummary();
        })
        .catch(function (err) {
          /* Re-enable on error (#760). */
          if (submitBtn) {
            submitBtn.disabled = false;
            submitBtn.textContent = "Create payment";
          }
          if (err.message !== "unauthorized") {
            setError($("create-payment-error"), err.message);
          }
        });
    });
  }

  /**
   * Generate a random idempotency key for the current form submission.
   * Uses crypto.randomUUID when available, falls back to Math.random.
   */
  function generateIdempotencyKey() {
    if (typeof crypto !== "undefined" && crypto.randomUUID) {
      return crypto.randomUUID();
    }
    var arr = [];
    for (var i = 0; i < 32; i++) arr.push(Math.floor(Math.random() * 16).toString(16));
    return arr.join("");
  }

  // ── Export ────────────────────────────────────────────────────────────────

  function exportCsv(payments, filename) {
    var csv = toCsv(payments, CSV_COLUMNS, activeZone());
    var blob = new Blob([csv], { type: "text/csv" });
    var url = URL.createObjectURL(blob);
    var a = document.createElement("a");
    a.href = url;
    a.download = filename || "stellargate-payments.csv";
    a.click();
    URL.revokeObjectURL(url);
  }

  // ── Version ───────────────────────────────────────────────────────────────

  var loadedVersion = null;

  function versionFrom(text) {
    var match = /v\d+\.\d+\.\d+/.exec(text);
    return match ? match[0] : null;
  }

  function showReloadBanner() {
    var banner = $("version-banner");
    if (banner) banner.hidden = false;
  }

  function loadVersion() {
    fetch("/")
      .then(function (res) { return res.text(); })
      .then(function (text) {
        var version = versionFrom(text);
        if (!version || loadedVersion) return;
        loadedVersion = version;
        var node = $("version");
        if (node) node.textContent = version;
      })
      .catch(function () { /* cosmetic only */ });
  }

  /** Show the testnet badge when the gateway isn't on the public network (#812). */
  function loadNetworkBadge() {
    fetch("/network")
      .then(function (res) { return res.json(); })
      .then(function (body) {
        var badge = $("network-badge");
        if (badge) badge.hidden = body.network === "public";
      })
      .catch(function () { /* cosmetic only */ });
  }

  /* Compare the version served by / with the one captured at start. */
  function checkDeployedVersion() {
    fetch("/")
      .then(function (res) { return res.text(); })
      .then(function (text) {
        var version = versionFrom(text);
        if (!version) return;
        if (!loadedVersion) {
          loadedVersion = version;
          var node = $("version");
          if (node) node.textContent = version;
          return;
        }
        if (version !== loadedVersion) showReloadBanner();
      })
      .catch(function () { /* health poll still owns the pill */ });
  }

  // ── Health ────────────────────────────────────────────────────────────────

  function updateSessionExpiry() {
    var expiresAt = session.expiresAt();
    var node = $("session-expiry");
    if (!node) return;
    if (expiresAt === null) { node.textContent = ""; return; }
    node.textContent = "session " + countdown(new Date(expiresAt).toISOString());
    node.title = "Saved " + fmtTime(new Date(session.savedAt()).toISOString());
  }

  function pollHealth() {
    fetch("/ready", { headers: { Accept: "application/json" } })
      .then(function (res) {
        return res.json().then(function (body) { return { ok: res.ok, body: body }; });
      })
      .then(function (r) {
        var pill = $("health");
        if (!pill) return;
        pill.className = r.ok ? "pill pill-ok" : "pill pill-err";
        clear(pill);
        var glyph = el("span", "pill-glyph", r.ok ? "\u2714" : "\u2716");
        glyph.setAttribute("aria-hidden", "true");
        pill.appendChild(glyph);
        pill.appendChild(el("span", "pill-text", r.ok ? "healthy" : r.body.reason || "unavailable"));
        pill.title = JSON.stringify(r.body);
      })
      .catch(function () {
        var pill = $("health");
        if (!pill) return;
        pill.className = "pill pill-err";
        clear(pill);
        var glyph = el("span", "pill-glyph", "\u2716");
        glyph.setAttribute("aria-hidden", "true");
        pill.appendChild(glyph);
        pill.appendChild(el("span", "pill-text", "unreachable"));
        pill.title = "Readiness request failed";
      });
  }

  // ── Filter / search UI ────────────────────────────────────────────────────

  /**
   * Keep the payments table caption in step with the active filters (#964).
   * Screen-reader users hear the caption when they enter the table, so it
   * must say what the rows currently represent.
   */
  function updatePaymentsCaption() {
    var node = $("payments-caption");
    if (!node) return;
    var state = store.get();
    var text = "Payments: " + (state.status || "all") + " statuses";
    if (state.search) text += ', matching "' + state.search + '"';
    node.textContent = text;
  }

  /** Add asset codes from `payments` to the dropdown's known set (#689). */
  function collectAssets(payments) {
    var known = store.get().knownAssets.slice();
    var changed = false;
    payments.forEach(function (p) {
      var a = p.asset || "XLM";
      if (known.indexOf(a) < 0) { known.push(a); changed = true; }
    });
    if (!changed) return;
    known.sort();
    store.update({ knownAssets: known });
    syncAssetOptions();
  }

  function syncAssetOptions() {
    var sel = $("asset-filter");
    if (!sel) return;
    var st = store.get();
    var codes = st.knownAssets.slice();
    if (st.asset && codes.indexOf(st.asset) < 0) codes.push(st.asset);
    clear(sel);
    var all = document.createElement("option");
    all.value = "";
    all.textContent = "All assets";
    sel.appendChild(all);
    codes.forEach(function (c) {
      var o = document.createElement("option");
      o.value = c;
      o.textContent = c;
      sel.appendChild(o);
    });
    sel.value = st.asset;
  }

  function hasActiveFilters() {
    var s = store.get();
    return !!(s.status || s.asset || s.search || s.createdAfter || s.createdBefore);
  }

  /** Reset every filter, the hash and the inputs, then reload (#779). */
  function clearFilters() {
    store.update({ status: "", asset: "", search: "", createdAfter: "", createdBefore: "" });
    onFilterChange();
  }

  function syncFilterUi() {
    var state = store.get();
    Array.prototype.forEach.call(document.querySelectorAll(".chip"), function (chip) {
      var status = chip.getAttribute("data-status") || "";
      var isActive = status ? state.statuses
        ? state.statuses.indexOf(status) >= 0
        : status === state.status
      : (state.statuses ? state.statuses.length === 0 : !state.status);
      chip.className = isActive ? "chip chip-on" : "chip";
      chip.setAttribute("aria-pressed", isActive ? "true" : "false");
    });

    var search = $("search");
    if (search && search.value !== state.search) search.value = state.search;

    var size = $("page-size");
    if (size) size.value = String(state.pageSize);

    updatePaymentsCaption();

    var after = $("created-after");
    if (after && after.value !== state.createdAfter) after.value = state.createdAfter;

    var before = $("created-before");
    if (before && before.value !== state.createdBefore) before.value = state.createdBefore;

    var auto = $("auto-refresh");
    if (auto) auto.checked = state.autoRefresh;

    syncAssetOptions();
    var clr = $("clear-filters");
    if (clr) clr.hidden = !hasActiveFilters();
    syncSearchClear();
    var minA = $("min-amount");
    if (minA && minA.value !== state.minAmount) minA.value = state.minAmount;
    var maxA = $("max-amount");
    if (maxA && maxA.value !== state.maxAmount) maxA.value = state.maxAmount;
    syncPresetUi();
  }

  var FILTER_DEBOUNCE_MS = 300;
  var filterTimer = null;

  /** Coalesce rapid date edits into a single request (#804). */
  function onFilterChangeDebounced() {
    window.clearTimeout(filterTimer);
    filterTimer = window.setTimeout(onFilterChange, FILTER_DEBOUNCE_MS);
  }

  function onFilterChange() {
    window.clearTimeout(filterTimer);
    syncFilterUi();
    writeHash();
    reload();
  }

  function syncSearchClear() {
    var button = $("search-clear");
    if (button) button.hidden = !store.get().search;
  }

  // ── Keyboard navigation ───────────────────────────────────────────────────

  function moveActiveRow(delta) {
    var count = store.visiblePayments().length;
    store.update({ activeRow: moveRow(store.get().activeRow, count, delta) });
    renderRows();
    var active = document.querySelector("#rows tr.row-active");
    if (active && typeof active.scrollIntoView === "function") {
      active.scrollIntoView({ block: "nearest" });
    }
  }

  function openActiveRow() {
    var stateSnap = store.get();
    var visible = store.visiblePayments();
    if (stateSnap.activeRow < 0 || stateSnap.activeRow >= visible.length) return;
    openDetail(visible[stateSnap.activeRow].id);
  }

  function onKeydown(ev) {
    if (store.get().helpOpen) {
      if (ev.key === "Escape" || ev.key === "?") { ev.preventDefault(); closeHelp(); }
      return;
    }
    var action = matchShortcut(ev, { activeElement: document.activeElement });
    if (!action) return;
    if (action === "focusSearch") {
      var search = $("search");
      if (!search) return;
      ev.preventDefault();
      search.focus();
      search.select();
      return;
    }
    ev.preventDefault();
    var drawerOpen = $("detail") && $("detail").open;
    switch (action) {
      case "refresh":    reload(); break;
      case "nextRow":    if (drawerOpen) stepDetail(1); else moveActiveRow(1); break;
      case "prevRow":    if (drawerOpen) stepDetail(-1); else moveActiveRow(-1); break;
      case "closeDrawer": closeDetail(); break;
      case "toggleHelp": toggleHelp(); break;
    }
  }

  // ── Shortcut help overlay (#721) ──────────────────────────────────────────

  function renderHelp() {
    var list = $("help-list");
    if (!list) return;
    clear(list);
    SHORTCUTS.forEach(function (s) {
      var li = el("li", "help-row");
      var kbd = el("kbd", null, s.hint);
      var span = el("span", null, s.label);
      li.appendChild(kbd);
      li.appendChild(span);
      list.appendChild(li);
    });
  }

  function openHelp() {
    store.update({ helpOpen: true });
    show($("help"), true);
    var closeBtn = $("help-close");
    if (closeBtn) closeBtn.focus();
  }

  function closeHelp() {
    store.update({ helpOpen: false });
    show($("help"), false);
    var openBtn = $("help-open");
    if (openBtn) openBtn.focus();
  }

  function toggleHelp() {
    if (store.get().helpOpen) closeHelp(); else openHelp();
  }

  // ── Internal mutable state not in store ───────────────────────────────────
  // (The store covers serialisable filter/pagination state; non-serialisable
  //  focus-restoration state lives here.)
  var state = {
    detailTrigger: null,
  };

  // ── Init ──────────────────────────────────────────────────────────────────

  function init() {
    initTheme();
    renderHelp();

    $("gate-form").addEventListener("submit", function (ev) {
      ev.preventDefault();
      var key = $("api-key").value.trim();
      if (!key) return;
      $("api-key").type = "password";
      $("api-key-toggle").setAttribute("aria-pressed", "false");
      $("api-key-toggle").textContent = "Show";
      setError($("gate-error"), null);
      signIn(key, $("remember").checked).catch(function (err) {
        if (err.message !== "unauthorized") setError($("gate-error"), err.message);
      });
    });

    $("sign-out").addEventListener("click", function () { signOut(null); });

    var keyToggle = $("api-key-toggle");
    if (keyToggle) {
      keyToggle.addEventListener("click", function () {
        var input = $("api-key");
        var reveal = input.type === "password";
        input.type = reveal ? "text" : "password";
        keyToggle.setAttribute("aria-pressed", reveal ? "true" : "false");
        keyToggle.textContent = reveal ? "Hide" : "Show";
      });
    }

    var versionReload = $("version-reload");
    if (versionReload) {
      versionReload.addEventListener("click", function () { window.location.reload(); });
    }

    $("refresh").addEventListener("click", function () { loadSummary(); reload(); });

    $("export-csv").addEventListener("click", function () {
      exportCsv(store.get().loadedPayments, "stellargate-payments.csv");
    });

    var exportSel = $("export-selected");
    if (exportSel) {
      exportSel.addEventListener("click", function () {
        exportCsv(Object.values(selectedPayments), "stellargate-payments-selected.csv");
      });
    }

    $("select-all").addEventListener("change", function () {
      var on = $("select-all").checked;
      store.get().loadedPayments.forEach(function (p) { setSelected(p, on); });
      syncSelectionUi();
    });

    $("page-size").addEventListener("change", function () {
      store.update({ pageSize: Number($("page-size").value) || 25 });
      onFilterChange();
    });

    $("created-after").addEventListener("change", function () {
      store.update({ createdAfter: $("created-after").value });
      onFilterChangeDebounced();
    });

    $("created-before").addEventListener("change", function () {
      store.update({ createdBefore: $("created-before").value });
      onFilterChangeDebounced();
    });

    $("auto-refresh").addEventListener("change", function () {
      store.update({ autoRefresh: $("auto-refresh").checked });
      onFilterChange();
    });

    Array.prototype.forEach.call(document.querySelectorAll(".preset"), function (btn) {
      btn.addEventListener("click", function () {
        var range = presetRange(Number(btn.getAttribute("data-days")));
        store.update({ createdAfter: range.from, createdBefore: range.to });
        var afterEl = $("created-after"), beforeEl = $("created-before");
        if (afterEl) afterEl.value = range.from;
        if (beforeEl) beforeEl.value = range.to;
        onFilterChange();
      });
    });

    var search = $("search");
    if (search) {
      var searchTimer = null;
      search.addEventListener("input", function () {
        window.clearTimeout(searchTimer);
        searchTimer = window.setTimeout(function () {
          store.update({ search: search.value.trim() });
          store.clampActiveRow();
          renderRows();
          writeHash();
          syncSearchClear();
        }, FILTER_DEBOUNCE_MS);
      });
    }

    ["min-amount", "max-amount"].forEach(function (id) {
      var input = $(id);
      if (!input) return;
      input.addEventListener("input", function () {
        store.update({ minAmount: $("min-amount").value.trim(), maxAmount: $("max-amount").value.trim() });
        store.clampActiveRow();
        renderRows();
        var note = $("amount-filter-note");
        if (note) note.hidden = !(store.get().minAmount || store.get().maxAmount);
      });
    });

    $("load-more").addEventListener("click", function () {
      store.update({ activeRow: -1 });
      loadPayments();
    });

    var searchClear = $("search-clear");
    if (searchClear) {
      searchClear.addEventListener("click", function () {
        store.update({ search: "" });
        renderRows();
        writeHash();
        syncSearchClear();
        var box = $("search");
        if (box) box.focus();
      });
    }

    /* Detail drawer wiring. */
    var detail = $("detail");
    if (detail) {
      $("invoice-print").addEventListener("click", function () {
        if (!$("invoice-print").disabled && detail.open) window.print();
      });
      var notifyBtn = $("detail-notify");
      if (notifyBtn) {
        notifyBtn.addEventListener("click", function () {
          if (currentDetailPayment) watchPaymentForNotification(currentDetailPayment);
        });
      }
      $("detail-close").addEventListener("click", closeDetail);
      $("detail-prev").addEventListener("click", function () { stepDetail(-1); });
      $("detail-next").addEventListener("click", function () { stepDetail(1); });
      $("detail-raw-copy").addEventListener("click", function () {
        var btn = $("detail-raw-copy");
        navigator.clipboard.writeText($("detail-raw-body").textContent).then(function () {
          btn.textContent = "Copied";
          window.setTimeout(function () { btn.textContent = "Copy"; }, 1500);
        }).catch(function () {});
      });
      $("detail").addEventListener("click", dismissOnBackdrop);
      $("detail").addEventListener("keydown", trapDetailFocus);
      $("detail").addEventListener("close", onDetailClosed);
      document.addEventListener("focusin", function (ev) { keepFocusInDetail(ev); });
    }

    $("help-close").addEventListener("click", closeHelp);
    $("help").addEventListener("click", function (ev) {
      if (ev.target === $("help")) closeHelp();
    });
    $("help-open").addEventListener("click", openHelp);

    document.addEventListener("keydown", function (ev) {
      if (ev.key !== "Enter") return;
      if (store.get().helpOpen) return;
      if (document.activeElement && document.activeElement.tagName === "TR") return;
      if (ev.ctrlKey || ev.metaKey || ev.altKey) return;
      if (store.get().activeRow < 0) return;
      ev.preventDefault();
      openActiveRow();
    });

    document.addEventListener("keydown", onKeydown);

    Array.prototype.forEach.call(document.querySelectorAll(".chip"), function (chip) {
      chip.addEventListener("click", function () {
        store.update({ status: chip.getAttribute("data-status") || "" });
        onFilterChange();
      });
    });

    $("asset-filter").addEventListener("change", function () {
      store.update({ asset: $("asset-filter").value });
      onFilterChange();
    });

    $("clear-filters").addEventListener("click", clearFilters);

    window.addEventListener("hashchange", function () {
      if (store.get().key) { applyHash(); reload(); }
    });

    window.setInterval(function () {
      if (document.hidden) return; // #803: pause health polling while hidden
      if (store.get().key) { pollHealth(); checkDeployedVersion(); }
    }, 30000);

    // #803: refresh once as soon as the tab becomes visible again.
    document.addEventListener("visibilitychange", function () {
      if (document.hidden || !store.get().key) return;
      pollHealth();
      var snap = store.get();
      if (snap.autoRefresh && (!snap.status || snap.status === "pending")) reload();
    });

    restartAutoRefreshTimer();

    /* Offline banner (#808): show it while the network is down, and refresh
    once the connection returns so the list isn't left stale. */
    function syncOnlineState() {
      show($("offline-banner"), !navigator.onLine);
    }
    window.addEventListener("offline", syncOnlineState);
    window.addEventListener("online", function () {
      syncOnlineState();
      if (store.get().key) { loadSummary(); reload(); }
    });
    syncOnlineState();

    initCreatePaymentDialog();

    var existing = session.read();
    if (existing) {
      signIn(existing, null).catch(function (err) {
        if (err.message !== "unauthorized") showGate("Could not restore your session: " + err.message);
      });
    }
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", init);
  } else {
    init();
  }
})();
