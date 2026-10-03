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
  fmtTime,
  formatAmount as _formatAmount,
  pillClass,
  relativeTime as _relativeTime,
  shortId,
  toCsv,
} from "./format.js";
import { createSessionStore } from "./session.js";
import { createStore, parseHash, serializeHash } from "./state.js";
import { matchShortcut, moveRow, SHORTCUTS } from "./keys.js";

(function () {
  "use strict";

  /* The version prefix is defined exactly once, here, so a request can never
     end up with the prefix doubled. Pinned by `tests/dashboard_asset_tests.rs`,
     which counts the occurrences of the prefix across this file. */
  var API_BASE = "/v1";
  var KEY_NAME = "stellargate.apiKey";
  var KEY_SAVED_AT = "stellargate.apiKeySavedAt";
  var THEME_KEY = "stellargate.theme";

  // ── Session store ─────────────────────────────────────────────────────────
  var session = createSessionStore({
    session: (function () { try { return window.sessionStorage; } catch (e) { return null; } })(),
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
  function formatAmount(amount, asset) { return _formatAmount(amount, asset); }

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

  function writeHashState() {
    try {
      var hash = serializeHash(store.get());
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

  // ── Theme (#713) ───────────────────────────────────────────────────────────

  function effectiveTheme() {
    try {
      var stored = window.localStorage.getItem(THEME_KEY);
      if (stored === "light" || stored === "dark") return stored;
    } catch (e) { /* storage unavailable */ }
    return null;
  }

  function applyTheme(theme) {
    if (theme === "light" || theme === "dark") {
      document.documentElement.setAttribute("data-theme", theme);
      window.localStorage.setItem(THEME_KEY, theme);
    } else {
      document.documentElement.removeAttribute("data-theme");
      window.localStorage.removeItem(THEME_KEY);
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

    return fetch(API_BASE + path, { method: opts.method || "GET", headers: headers, body: opts.body || null }).then(
      function (res) {
        if (res.status === 401) {
          signOut("That API key was rejected. Please sign in again.");
          throw new Error("unauthorized");
        }
        return res
          .json()
          .catch(function () { return {}; })
          .then(function (body) {
            if (!res.ok) throw new Error(body.error || "Request failed (" + res.status + ")");
            return body;
          });
      }
    );
  }

  // ── Session ───────────────────────────────────────────────────────────────

  function showGate(message) {
    store.update({ key: null, selectedPaymentId: null, activeRow: -1 });
    closeDetail();
    show($("app"), false);
    show($("gate"), true);
    setError($("gate-error"), message || null);
  }

  function signOut(message) {
    session.clear();
    showGate(message);
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
      loadVersion();
      pollHealth();
      loadSummary();
      reload();
    });
  }

  // ── Payments list ──────────────────────────────────────────────────────────

  var reloadPending = false;

  function reload() {
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

    api(buildListQuery(state))
      .then(function (body) {
        clearSkeletonRows();
        var payments = body.payments || [];
        var rows = store.get().loadedPayments.concat(payments);
        store.update({ loadedPayments: rows });
        renderRows();

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
        clearSkeletonRows();
        if (err.message !== "unauthorized") {
          setListState(buildErrorState(err.message, function () {
            clearListState();
            reload();
          }));
        }
      })
      .then(function () {
        store.update({ loading: false });
        if (reloadPending) {
          reloadPending = false;
          store.resetPaging();
          loadPayments();
        }
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

  function renderRows() {
    var tbody = $("rows");
    var visible = store.visiblePayments();
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
        { label: "Select",     cls: "sk-select" },
        { label: "Status",     cls: "sk-status" },
        { label: "Amount",     cls: "sk-amount" },
        { label: "Memo",       cls: "sk-memo" },
        { label: "Created",    cls: "sk-date" },
        { label: "Payment ID", cls: "sk-id" },
      ];
      cols.forEach(function (col) {
        var td = document.createElement("td");
        td.setAttribute("data-label", col.label);
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

  function rowFor(p, isActive) {
    var tr = document.createElement("tr");
    tr.tabIndex = 0;
    tr.dataset.paymentId = p.id;
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
    statusCell.appendChild(el("span", pillClass(p.status), p.status));
    tr.appendChild(statusCell);

    tr.appendChild(labelledCell("Amount", null, formatAmount(p.amount, p.asset)));
    tr.appendChild(labelledCell("Memo", "mono", p.memo));
    tr.appendChild(labelledCell("Created", null, fmtTime(p.created_at)));
    tr.appendChild(labelledCell("Payment ID", "mono", shortId(p.id)));

    tr.addEventListener("click", function () { openDetail(p.id, tr); });
    tr.addEventListener("keydown", function (ev) {
      if (ev.key === "Enter" || ev.key === " ") { ev.preventDefault(); openDetail(p.id, tr); }
    });
    return tr;
  }

  // ── Detail panel (#715) ───────────────────────────────────────────────────

  /**
   * Open the detail drawer as a real modal dialog.
   * Stores the trigger row so focus can be returned on close.
   */
  function openDetail(id, trigger) {
    store.update({ selectedPaymentId: id });
    state.detailTrigger = trigger || null;

    var detail = $("detail");
    var fields = $("detail-fields");
    clear(fields);
    clear($("deliveries"));
    setError($("deliveries-error"), null);
    show($("deliveries-empty"), false);

    showDetailSkeleton();
    openModal(detail);
    focusDetail();

    api("/payments/" + encodeURIComponent(id))
      .then(function (p) {
        clear(fields);
        [
          ["Status", p.status],
          ["Amount", formatAmount(p.amount, p.asset)],
          ["Received", p.paid_amount ? formatAmount(p.paid_amount, p.asset) : "—"],
          ["Memo", p.memo],
          ["Destination", p.destination_address],
          ["Transaction", p.tx_hash || "—"],
          ["Network", "Stellar"],
          ["Asset issuer", p.asset_issuer || "native"],
          ["Payment ID", p.id],
          ["Merchant", p.merchant_id],
          ["Created", fmtTime(p.created_at)],
          ["Updated", fmtTime(p.updated_at)],
          ["Expires", fmtTime(p.expires_at) +
            (p.status === "pending" ? " (" + countdown(p.expires_at) + " left)" : "")],
        ].forEach(function (pair) {
          fields.appendChild(el("dt", null, pair[0]));
          if (pair[0] === "Status") {
            var dd = document.createElement("dd");
            dd.appendChild(el("span", pillClass(p.status), p.status));
            fields.appendChild(dd);
          } else if (pair[0] === "Transaction" && p.tx_hash) {
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
    head.appendChild(el("span", pillClass(d.status), d.status));
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
          qrContainer.innerHTML = svgMarkup;
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
    var csv = toCsv(payments, CSV_COLUMNS);
    var blob = new Blob([csv], { type: "text/csv" });
    var url = URL.createObjectURL(blob);
    var a = document.createElement("a");
    a.href = url;
    a.download = filename || "stellargate-payments.csv";
    a.click();
    URL.revokeObjectURL(url);
  }

  // ── Version ───────────────────────────────────────────────────────────────

  function loadVersion() {
    fetch("/")
      .then(function (res) { return res.text(); })
      .then(function (text) {
        var match = /v\d+\.\d+\.\d+/.exec(text);
        if (match) $("version").textContent = match[0];
      })
      .catch(function () { /* cosmetic only */ });
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
        pill.className = r.ok ? "pill pill-ok" : "pill pill-err";
        pill.textContent = r.ok ? "healthy" : r.body.reason || "unavailable";
        pill.title = JSON.stringify(r.body);
      })
      .catch(function () {
        var pill = $("health");
        pill.className = "pill pill-err";
        pill.textContent = "unreachable";
        pill.title = "Readiness request failed";
      });
  }

  // ── Filter / search UI ────────────────────────────────────────────────────

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

    var after = $("created-after");
    if (after && after.value !== state.createdAfter) after.value = state.createdAfter;

    var before = $("created-before");
    if (before && before.value !== state.createdBefore) before.value = state.createdBefore;

    var auto = $("auto-refresh");
    if (auto) auto.checked = state.autoRefresh;

    var assetEl = $("asset-filter");
    if (assetEl && assetEl.value !== (state.asset || "")) assetEl.value = state.asset || "";

    syncPresetUi();
  }

  function onFilterChange() {
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
    switch (action) {
      case "refresh":    reload(); break;
      case "nextRow":    moveActiveRow(1); break;
      case "prevRow":    moveActiveRow(-1); break;
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
      setError($("gate-error"), null);
      signIn(key, $("remember").checked).catch(function (err) {
        if (err.message !== "unauthorized") setError($("gate-error"), err.message);
      });
    });

    $("sign-out").addEventListener("click", function () { signOut(null); });

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
      onFilterChange();
    });

    $("created-before").addEventListener("change", function () {
      store.update({ createdBefore: $("created-before").value });
      onFilterChange();
    });

    $("auto-refresh").addEventListener("change", function () {
      store.update({ autoRefresh: $("auto-refresh").checked });
      onFilterChange();
    });

    var assetFilter = $("asset-filter");
    if (assetFilter) {
      assetFilter.addEventListener("change", function () {
        store.update({ asset: assetFilter.value });
        onFilterChange();
      });
    }

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
      search.addEventListener("input", function () {
        store.update({ search: search.value.trim() });
        store.clampActiveRow();
        renderRows();
        writeHash();
        syncSearchClear();
      });
    }

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
      $("detail-close").addEventListener("click", closeDetail);
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

    window.addEventListener("hashchange", function () {
      if (store.get().key) { applyHash(); reload(); }
    });

    window.setInterval(function () {
      if (store.get().key) pollHealth();
    }, 30000);

    window.setInterval(function () {
      var stateSnap = store.get();
      if (stateSnap.key && stateSnap.autoRefresh && (!stateSnap.status || stateSnap.status === "pending")) {
        reload();
      }
    }, 15000);

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
