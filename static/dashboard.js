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
  countdown,
  CSV_COLUMNS,
  explorerTx,
  fmtTime,
  formatAmount,
  pillClass,
  relativeTime,
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

  var store = createStore();
  var session = createSessionStore({
    session: safeArea(function () {
      return window.sessionStorage;
    }),
    local: safeArea(function () {
      return window.localStorage;
    }),
  });

  /* ── Tiny DOM helpers ────────────────────────────────────────────────── */

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

  /**
   * A Web Storage area, or null when the browser refuses to hand one over.
   *
   * Merely *reading* `window.localStorage` throws in Safari private mode and
   * wherever storage is blocked by policy, so it is touched lazily and the
   * session store is told to fall back to memory (issue #678). Losing
   * persistence must degrade to "re-enter the key on reload", never to a blank
   * page.
   */
  function safeArea(get) {
    try {
      return get();
    } catch (e) {
      return null;
    }
  }

  /* ── API ─────────────────────────────────────────────────────────────── */

  /**
   * Call the gateway. Resolves with the parsed body, or rejects with an Error
   * carrying the API's `error` message when one is present. A 401 drops the
   * stored key and returns to the sign-in gate, since it means the key was
   * revoked or is wrong.
   *
   * The key is attached as an `Authorization` header and nowhere else — never a
   * query parameter, never the fragment. #726 asserts this over every request
   * the browser actually makes.
   */
  function api(path, options) {
    var opts = options || {};
    var headers = { Accept: "application/json" };
    var key = store.get().key;
    if (key) headers.Authorization = "Bearer " + key;

    return fetch(API_BASE + path, { method: opts.method || "GET", headers: headers }).then(
      function (res) {
        if (res.status === 401) {
          signOut("That API key was rejected. Please sign in again.");
          throw new Error("unauthorized");
        }
        return res
          .json()
          .catch(function () {
            return {};
          })
          .then(function (body) {
            if (!res.ok) {
              throw new Error(
                body.error || "Request failed (" + res.status + ")"
              );
            }
            return body;
          });
      }
    );
  }

  /* ── Session ─────────────────────────────────────────────────────────── */

  /** Return to the sign-in form, keeping any stored key so a reload retries. */
  function showGate(message) {
    store.update({ key: null, selectedPaymentId: null, activeRow: -1 });
    closeDetail();
    show($("app"), false);
    show($("gate"), true);
    setError($("gate-error"), message || null);
  }

  /**
   * Return to the sign-in form AND discard the stored key.
   *
   * Only for cases where the key itself is the problem (a 401, or an explicit
   * sign-out). A transient failure must use showGate() instead: discarding a
   * perfectly good key because the network blinked forces the user to dig it
   * out again.
   */
  function signOut(message) {
    session.clear();
    showGate(message);
  }

  function signIn(key, persist) {
    store.update({ key: key });
    applyHash();
    // Validate by making the cheapest authenticated call available.
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

  /* ── Payments list ───────────────────────────────────────────────────── */

  function reload() {
    store.resetPaging();
    store.update({ loadedPayments: [] });
    clear($("rows"));
    loadPayments();
  }

  /* Set while a request is in flight, so a refresh arriving mid-flight is
     deferred rather than dropped. Dropping it would leave the table the reload
     just cleared permanently empty; letting it run concurrently would let two
     responses both append, doubling the list. */
  var reloadPending = false;

  function loadPayments() {
    var state = store.get();
    if (state.loading) {
      /* Remember the request and issue it once the current one finishes. The
         list is already cleared by reload(), so this is a replace, not an
         append — hence resetPaging() here too. */
      reloadPending = true;
      return;
    }
    store.update({ loading: true });
    setError($("list-error"), null);

    api(buildListQuery(state))
      .then(function (body) {
        var payments = body.payments || [];
        var rows = store.get().loadedPayments.concat(payments);
        store.update({ loadedPayments: rows });
        renderRows();

        /* The offset-mode response returns a cursor even on the final page, so
           a short page is what actually signals the end. */
        var more = payments.length === store.get().pageSize && !!body.next_cursor;
        store.update({ cursor: more ? body.next_cursor : null });
        show($("load-more"), more);
        show($("empty"), store.visiblePayments().length === 0);
      })
      .catch(function (err) {
        if (err.message !== "unauthorized") {
          setError($("list-error"), err.message);
        }
      })
      .then(function () {
        store.update({ loading: false });
        if (reloadPending) {
          reloadPending = false;
          /* The response just applied is now stale, and its cursor points into
             a result set the operator has already moved past, so the deferred
             refresh starts from a clean paging state. */
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
      .catch(function () {
        clear($("summary"));
      });
  }

  /** Draw the currently visible rows, honouring the search box and `j`/`k`. */
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

  /**
   * Announce the highlighted row to assistive technology.
   *
   * The `j`/`k` highlight is otherwise a purely visual change: a sighted user
   * sees the row move, a screen-reader user would hear nothing at all. This
   * runs on every render, not just on keypress, so a filter change that moves
   * the highlight is announced too.
   */
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
    node.textContent =
      "Row " +
      (active + 1) +
      " of " +
      visible.length +
      ": " +
      p.status +
      ", " +
      formatAmount(p.amount, p.asset) +
      ", memo " +
      p.memo;
  }

  /** A table cell carrying the column name the mobile card layout shows. */
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
      /* Roving tabindex: the highlighted row is the one the keyboard lands on,
         so tabbing into the table does not restart at row 1. */
      tr.setAttribute("aria-current", "true");
    }

    var statusCell = document.createElement("td");
    statusCell.setAttribute("data-label", "Status");
    statusCell.appendChild(el("span", pillClass(p.status), p.status));
    tr.appendChild(statusCell);

    /* `data-label` is what the ≤720px card layout renders as each row's
       heading (see `.payments td::before` in dashboard.css). The table header
       cells are hidden at that width, so without it the cards degrade to an
       unlabelled list of values. */
    tr.appendChild(labelledCell("Amount", null, formatAmount(p.amount, p.asset)));
    tr.appendChild(labelledCell("Memo", "mono", p.memo));
    tr.appendChild(labelledCell("Created", null, fmtTime(p.created_at)));
    tr.appendChild(labelledCell("Payment ID", "mono", shortId(p.id)));

    tr.addEventListener("click", function () {
      openDetail(p.id);
    });
    tr.addEventListener("keydown", function (ev) {
      if (ev.key === "Enter" || ev.key === " ") {
        ev.preventDefault();
        openDetail(p.id);
      }
    });

    return tr;
  }

  /* ── Detail panel ────────────────────────────────────────────────────── */

  function openDetail(id) {
    store.update({ selectedPaymentId: id });
    show($("detail"), true);
    show($("scrim"), true);
    /* Move focus into the panel so the keyboard user is inside the thing that
       just opened, and so Escape is meaningful without a pointer. */
    var close = $("detail-close");
    if (close) close.focus();

    var fields = $("detail-fields");
    clear(fields);
    clear($("deliveries"));
    setError($("deliveries-error"), null);
    show($("deliveries-empty"), false);

    api("/payments/" + encodeURIComponent(id))
      .then(function (p) {
        [
          ["Status", p.status],
          ["Amount", formatAmount(p.amount, p.asset)],
          [
            "Received",
            p.paid_amount ? formatAmount(p.paid_amount, p.asset) : "—",
          ],
          ["Memo", p.memo],
          ["Destination", p.destination_address],
          ["Transaction", p.tx_hash || "—"],
          ["Network", "Stellar"],
          ["Asset issuer", p.asset_issuer || "native"],
          ["Payment ID", p.id],
          ["Merchant", p.merchant_id],
          ["Created", fmtTime(p.created_at)],
          ["Updated", fmtTime(p.updated_at)],
          [
            "Expires",
            fmtTime(p.expires_at) +
              (p.status === "pending"
                ? " (" + countdown(p.expires_at) + " left)"
                : ""),
          ],
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
        if (err.message !== "unauthorized") {
          fields.appendChild(el("dd", "error", err.message));
        }
      });

    loadDeliveries(id);
  }

  function closeDetail() {
    store.update({ selectedPaymentId: null });
    show($("detail"), false);
    show($("scrim"), false);
    /* Return focus to the list so a keyboard user is not dropped at the top of
       the document after dismissing the panel. */
    var rows = document.querySelector("#rows tr");
    if (rows && typeof rows.focus === "function") rows.focus();
  }

  function loadDeliveries(paymentId) {
    api("/payments/" + encodeURIComponent(paymentId) + "/webhooks")
      .then(function (body) {
        var list = $("deliveries");
        clear(list);
        var deliveries = body.deliveries || [];
        show($("deliveries-empty"), deliveries.length === 0);
        deliveries.forEach(function (d) {
          list.appendChild(deliveryItem(paymentId, d));
        });
      })
      .catch(function (err) {
        if (err.message !== "unauthorized") {
          setError($("deliveries-error"), err.message);
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
    li.appendChild(
      el(
        "div",
        "delivery-meta",
        "attempt " + d.attempts + " · manual " + (d.manual_attempts || 0)
      )
    );
    li.appendChild(el("div", "delivery-meta", "last: " + relativeTime(d.last_attempt)));
    li.lastChild.title = fmtTime(d.last_attempt);
    li.appendChild(el("div", "delivery-meta", "created: " + relativeTime(d.created_at)));
    li.lastChild.title = fmtTime(d.created_at);
    if (d.status === "failed") {
      li.appendChild(
        el(
          "div",
          "error",
          "Last delivery failed; check receiver logs or redeliver."
        )
      );
    }
    if (d.status !== "delivered") {
      li.appendChild(
        el(
          "div",
          "delivery-meta",
          "retry state: queued for redrive if attempts remain"
        )
      );
    }

    var button = el("button", "ghost", "Redeliver");
    button.addEventListener("click", function () {
      if (!window.confirm("Redeliver this webhook now?")) return;
      button.disabled = true;
      button.textContent = "Sending…";
      api(
        "/payments/" +
          encodeURIComponent(paymentId) +
          "/webhooks/" +
          encodeURIComponent(d.id) +
          "/redeliver",
        { method: "POST" }
      )
        .then(function () {
          loadDeliveries(paymentId);
        })
        .catch(function (err) {
          button.disabled = false;
          button.textContent = "Redeliver";
          if (err.message !== "unauthorized") {
            setError(
              $("deliveries-error"),
              err.message.indexOf("429") >= 0
                ? "Rate limited. Try again shortly."
                : err.message
            );
          }
        });
    });
    li.appendChild(button);

    return li;
  }

  function exportCsv() {
    var csv = toCsv(store.get().loadedPayments, CSV_COLUMNS);
    var blob = new Blob([csv], { type: "text/csv" });
    var url = URL.createObjectURL(blob);
    var a = document.createElement("a");
    a.href = url;
    a.download = "stellargate-payments.csv";
    a.click();
    URL.revokeObjectURL(url);
  }

  /* ── Version ─────────────────────────────────────────────────────────── */

  /** The root route answers with "StellarGate API vX.Y.Z". */
  function loadVersion() {
    fetch("/")
      .then(function (res) {
        return res.text();
      })
      .then(function (text) {
        var match = /v\d+\.\d+\.\d+/.exec(text);
        if (match) $("version").textContent = match[0];
      })
      .catch(function () {
        /* cosmetic only */
      });
  }

  /* ── Health ──────────────────────────────────────────────────────────── */

  function updateSessionExpiry() {
    var expiresAt = session.expiresAt();
    var node = $("session-expiry");
    if (!node) return;
    if (expiresAt === null) {
      node.textContent = "";
      return;
    }
    node.textContent = "session " + countdown(new Date(expiresAt).toISOString());
    node.title = "Saved " + fmtTime(new Date(session.savedAt()).toISOString());
  }

  function pollHealth() {
    fetch("/ready", { headers: { Accept: "application/json" } })
      .then(function (res) {
        return res.json().then(function (body) {
          return { ok: res.ok, body: body };
        });
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

  /* ── URL hash (filters, never credentials) ───────────────────────────── */

  function applyHash() {
    store.update(parseHash(window.location.hash));
    syncFilterUi();
  }

  function writeHash() {
    var hash = serializeHash(store.get());
    /* replaceState, not assign: replace keeps the back button meaningful for
       navigation, and the hash is written from a fixed key allow-list so a
       filter value can never smuggle anything into the URL. */
    window.history.replaceState(
      null,
      "",
      hash ? "#" + hash : window.location.pathname
    );
  }

  /* ── Keyboard shortcuts (#721) ───────────────────────────────────────── */

  function openHelp() {
    show($("help"), true);
    store.update({ helpOpen: true });
    var close = $("help-close");
    if (close) close.focus();
  }

  function closeHelp() {
    show($("help"), false);
    store.update({ helpOpen: false });
    /* Hand focus back to whatever opened the overlay, so a keyboard user is
       not dropped at the top of the document. */
    var opener = $("help-open");
    if (opener && typeof opener.focus === "function") opener.focus();
  }

  function toggleHelp() {
    if (store.get().helpOpen) closeHelp();
    else openHelp();
  }

  /** Build the `?` overlay from SHORTCUTS so docs cannot drift from behaviour. */
  function renderHelp() {
    var list = $("help-list");
    if (!list) return;
    clear(list);
    SHORTCUTS.forEach(function (s) {
      var li = el("li", "help-row");
      var key = el("kbd", null, s.hint);
      key.setAttribute("data-shortcut", s.keys[0]);
      li.appendChild(key);
      li.appendChild(el("span", null, s.label));
      list.appendChild(li);
    });
  }

  function moveActiveRow(delta) {
    var count = store.visiblePayments().length;
    store.update({ activeRow: moveRow(store.get().activeRow, count, delta) });
    renderRows();
    var active = document.querySelector("#rows tr.row-active");
    if (active && typeof active.scrollIntoView === "function") {
      active.scrollIntoView({ block: "nearest" });
    }
  }

  /** Open the highlighted row, or do nothing when no row is highlighted. */
  function openActiveRow() {
    var state = store.get();
    var visible = store.visiblePayments();
    if (state.activeRow < 0 || state.activeRow >= visible.length) return;
    openDetail(visible[state.activeRow].id);
  }

  function onKeydown(ev) {
    /* The help overlay is modal over the app: only its own dismiss keys are
       honoured while it is open, so a stray `j` cannot move rows behind it. */
    if (store.get().helpOpen) {
      if (ev.key === "Escape" || ev.key === "?") {
        ev.preventDefault();
        closeHelp();
      }
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

    /* Everything below is an in-app action and must not also reach the
       browser's own defaults (space scrolls, `?` opens quick find in some
       browsers, `/` opens quick find in Firefox). */
    ev.preventDefault();

    switch (action) {
      case "refresh":
        reload();
        break;
      case "nextRow":
        moveActiveRow(1);
        break;
      case "prevRow":
        moveActiveRow(-1);
        break;
      case "closeDrawer":
        closeDetail();
        break;
      case "toggleHelp":
        toggleHelp();
        break;
      default:
        break;
    }
  }

  /* ── Wiring ──────────────────────────────────────────────────────────── */

  function syncFilterUi() {
    var state = store.get();
    Array.prototype.forEach.call(
      document.querySelectorAll(".chip"),
      function (chip) {
        chip.className =
          (chip.getAttribute("data-status") || "") === state.status
            ? "chip chip-on"
            : "chip";
      }
    );

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
  }

  /** Re-render on a filter change: rows, chips, and the URL hash together. */
  function onFilterChange() {
    syncFilterUi();
    writeHash();
    reload();
  }

  /**
   * Show the clear button only when there is something to clear.
   *
   * `type="search"` gives some browsers a native clear affordance, but not all,
   * and it is invisible to keyboard users when it is not rendered — an explicit
   * button keeps "get rid of this filter" reachable everywhere.
   */
  function syncSearchClear() {
    var button = $("search-clear");
    if (button) button.hidden = !store.get().search;
  }

  function init() {
    renderHelp();

    $("gate-form").addEventListener("submit", function (ev) {
      ev.preventDefault();
      var key = $("api-key").value.trim();
      if (!key) return;
      setError($("gate-error"), null);
      signIn(key, $("remember").checked).catch(function (err) {
        if (err.message !== "unauthorized") {
          setError($("gate-error"), err.message);
        }
      });
    });

    $("sign-out").addEventListener("click", function () {
      signOut(null);
    });

    $("refresh").addEventListener("click", reload);
    $("export-csv").addEventListener("click", exportCsv);
    $("page-size").addEventListener("change", function () {
      var n = Number($("page-size").value) || 25;
      store.update({ pageSize: n });
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

    /* Search filters the rows already loaded (#693), so it filters on input
       with no debounce needed: there is no request to batch. */
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

    /* A new page of rows invalidates both the highlighted row and the CSV
       export, which is built from the loaded set. */
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

    $("detail-close").addEventListener("click", closeDetail);
    $("scrim").addEventListener("click", closeDetail);
    $("help-close").addEventListener("click", closeHelp);
    /* The overlay element is its own full-viewport backdrop, so a click that
       lands on the overlay rather than the panel is a click outside it. There is
       no separate scrim: a second, lower-z layer would sit permanently behind
       the overlay and never receive the click. */
    $("help").addEventListener("click", function (ev) {
      if (ev.target === $("help")) closeHelp();
    });
    $("help-open").addEventListener("click", openHelp);

    /* Enter on the highlighted row opens it, so `j`/`k` then Enter is a
       complete keyboard path through the list. */
    document.addEventListener("keydown", function (ev) {
      if (ev.key !== "Enter") return;
      if (store.get().helpOpen) return;
      if (document.activeElement && document.activeElement.tagName === "TR") {
        return;
      }
      if (ev.ctrlKey || ev.metaKey || ev.altKey) return;
      if (store.get().activeRow < 0) return;
      ev.preventDefault();
      openActiveRow();
    });

    document.addEventListener("keydown", onKeydown);

    Array.prototype.forEach.call(
      document.querySelectorAll(".chip"),
      function (chip) {
        chip.addEventListener("click", function () {
          store.update({ status: chip.getAttribute("data-status") || "" });
          onFilterChange();
        });
      }
    );

    /* Back/forward must move the filters, or a shared URL is a lie. */
    window.addEventListener("hashchange", function () {
      if (store.get().key) {
        applyHash();
        reload();
      }
    });

    window.setInterval(function () {
      if (store.get().key) pollHealth();
    }, 30000);
    window.setInterval(function () {
      var state = store.get();
      if (state.key && state.autoRefresh && (!state.status || state.status === "pending")) {
        reload();
      }
    }, 15000);

    /* Resume an existing session when a key is already stored. The gate is
       visible until this succeeds, so any failure here simply leaves the user
       looking at the sign-in form rather than at nothing. */
    var existing = session.read();
    if (existing) {
      signIn(existing, null).catch(function (err) {
        /* A 401 already returned to the gate via signOut() inside api(). Every
           other failure — server down, network dropped, a proxy returning a
           login page — must land there too. Swallowing it would leave both
           panels hidden and render a blank page with no way forward. */
        if (err.message !== "unauthorized") {
          showGate("Could not restore your session: " + err.message);
        }
      });
    }
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", init);
  } else {
    init();
  }
})();
