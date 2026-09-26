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
 */

import { fmtTime, shortId } from "/dashboard/format.js";

(function () {
  "use strict";

  var API_BASE = "/v1";
  var KEY_NAME = "stellargate.apiKey";
  var KEY_SAVED_AT = "stellargate.apiKeySavedAt";
  var STATUSES = ["pending", "completed", "underpaid", "expired"];

  var state = {
    key: null,
    // Pressed status chips, in STATUSES order. Empty means "All".
    statuses: [],
    pageSize: 25,
    createdAfter: "",
    createdBefore: "",
    cursor: null,
    loading: false,
    loadedPayments: [],
    // Selected payments keyed by id, holding the row's payment record so the
    // selection can be exported without re-fetching.
    selected: {},
    autoRefresh: false,
  };

  // ── Tiny DOM helpers ──────────────────────────────────────────────────

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
    node.hidden = !visible;
  }

  function clear(node) {
    while (node.firstChild) node.removeChild(node.firstChild);
  }

  function setError(node, message) {
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

  // ── Formatting ────────────────────────────────────────────────────────

  /** Map a payment or delivery status onto a pill style. */
  function pillClass(status) {
    switch (status) {
      case "completed":
      case "delivered":
        return "pill pill-ok";
      case "pending":
      case "underpaid":
        return "pill pill-warn";
      case "expired":
      case "failed":
        return "pill pill-err";
      default:
        return "pill pill-idle";
    }
  }

  /** Format a payment amount with its asset code. */
  function formatAmount(amount, asset) {
    if (!amount) return "—";
    return amount + " " + (asset || "XLM");
  }

  /** Return a Stellar expert explorer URL for a transaction hash. */
  function explorerTx(hash) {
    return "https://stellar.expert/explorer/public/tx/" + encodeURIComponent(hash);
  }

  /** Human-readable relative time (e.g. "2 min ago", "just now"). */
  function relativeTime(iso) {
    if (!iso) return "—";
    var d = new Date(iso);
    if (isNaN(d.getTime())) return iso;
    var diffMs = Date.now() - d.getTime();
    var diffSec = Math.round(diffMs / 1000);
    if (diffSec < 5) return "just now";
    if (diffSec < 60) return diffSec + "s ago";
    var diffMin = Math.round(diffSec / 60);
    if (diffMin < 60) return diffMin + " min ago";
    var diffHr = Math.round(diffMin / 60);
    if (diffHr < 24) return diffHr + "h ago";
    return Math.round(diffHr / 24) + "d ago";
  }

  /** Human-readable countdown to an ISO timestamp (e.g. "5m 32s"). */
  function countdown(iso) {
    if (!iso) return "";
    var d = new Date(iso);
    if (isNaN(d.getTime())) return "";
    var diffMs = d.getTime() - Date.now();
    if (diffMs <= 0) return "expired";
    var totalSec = Math.floor(diffMs / 1000);
    var h = Math.floor(totalSec / 3600);
    var m = Math.floor((totalSec % 3600) / 60);
    var s = totalSec % 60;
    if (h > 0) return h + "h " + m + "m";
    if (m > 0) return m + "m " + s + "s";
    return s + "s";
  }

  // ── Hash-state persistence ─────────────────────────────────────────────
  // Stores the active status filter and auto-refresh flag in the URL hash so
  // the user can bookmark or share a pre-filtered view.

  function readHashState() {
    try {
      var hash = window.location.hash.slice(1);
      if (!hash) return;
      var parts = hash.split("&");
      parts.forEach(function (part) {
        var kv = part.split("=");
        if (kv.length !== 2) return;
        var key = decodeURIComponent(kv[0]);
        var value = decodeURIComponent(kv[1]);
        if (key === "status") {
          state.statuses = STATUSES.filter(function (s) {
            return value.split(",").indexOf(s) >= 0;
          });
        }
        if (key === "autoRefresh") state.autoRefresh = value === "1";
      });
    } catch (e) {
      /* non-fatal */
    }
  }

  function writeHashState() {
    try {
      var parts = [];
      if (state.statuses.length) {
        parts.push("status=" + state.statuses.map(encodeURIComponent).join(","));
      }
      if (state.autoRefresh) parts.push("autoRefresh=1");
      var hash = parts.length ? "#" + parts.join("&") : "";
      window.history.replaceState(null, "", window.location.pathname + window.location.search + hash);
    } catch (e) {
      /* non-fatal */
    }
  }

  // ── API ───────────────────────────────────────────────────────────────

  /**
   * Call the gateway. Resolves with the parsed body, or rejects with an Error
   * carrying the API's `error` message when one is present. A 401 drops the
   * stored key and returns to the sign-in gate, since it means the key was
   * revoked or is wrong.
   */
  function api(path, options) {
    var opts = options || {};
    var headers = { Accept: "application/json" };
    if (state.key) headers.Authorization = "Bearer " + state.key;

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
              throw new Error(body.error || "Request failed (" + res.status + ")");
            }
            return body;
          });
      }
    );
  }

  // ── Session ───────────────────────────────────────────────────────────

  function storedKey() {
    try {
      return (
        window.sessionStorage.getItem(KEY_NAME) ||
        window.localStorage.getItem(KEY_NAME)
      );
    } catch (e) {
      return null; // storage blocked; fall back to in-memory only
    }
  }

  function storeKey(key, persist) {
    try {
      (persist ? window.localStorage : window.sessionStorage).setItem(
        KEY_NAME,
        key
      );
      (persist ? window.localStorage : window.sessionStorage).setItem(
        KEY_SAVED_AT,
        String(Date.now())
      );
    } catch (e) {
      /* non-fatal: the key still works for this page load */
    }
  }

  function forgetKey() {
    try {
      window.sessionStorage.removeItem(KEY_NAME);
      window.localStorage.removeItem(KEY_NAME);
    } catch (e) {
      /* nothing to do */
    }
  }

  /** Return to the sign-in form, keeping any stored key so a reload retries. */
  function showGate(message) {
    state.key = null;
    closeDetail();
    show($("app"), false);
    show($("gate"), true);
    setError($("gate-error"), message || null);
  }

  /** Return to the sign-in form AND discard the stored key.
   *
   * Only for cases where the key itself is the problem (a 401, or an explicit
   * sign-out). A transient failure must use showGate() instead: discarding a
   * perfectly good key because the network blinked forces the user to dig it
   * out again. */
  function signOut(message) {
    forgetKey();
    showGate(message);
  }

  function signIn(key, persist) {
    state.key = key;
    state.selected = {};
    readHashState();
    syncFilterUi();
    // Validate by making the cheapest authenticated call available.
    return api("/payments?limit=1").then(function () {
      if (persist !== null) storeKey(key, persist);
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

  // ── Skeleton helpers (#719) ───────────────────────────────────────────

  /**
   * Append `count` skeleton placeholder rows to the payments tbody.
   * The rows carry class `skeleton-row` and are removed by `clearSkeletons()`
   * once real data (or an error) arrives.
   */
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
    var skeletons = tbody.querySelectorAll(".skeleton-row");
    for (var i = 0; i < skeletons.length; i++) {
      tbody.removeChild(skeletons[i]);
    }
  }

  /**
   * Replace the detail panel fields with skeleton placeholders while the
   * payment record loads.
   */
  function showDetailSkeleton() {
    var fields = $("detail-fields");
    clear(fields);

    var rows = [
      { dtWidth: "4rem",  ddCls: "skeleton-field skeleton-field-short" },
      { dtWidth: "4rem",  ddCls: "skeleton-field skeleton-field-short" },
      { dtWidth: "5rem",  ddCls: "skeleton-field skeleton-field-long"  },
      { dtWidth: "6rem",  ddCls: "skeleton-field skeleton-field-full"  },
      { dtWidth: "5rem",  ddCls: "skeleton-field skeleton-field-full"  },
      { dtWidth: "6rem",  ddCls: "skeleton-field skeleton-field-long"  },
      { dtWidth: "4rem",  ddCls: "skeleton-field skeleton-field-short" },
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

  // ── Empty / error state helpers (#720) ────────────────────────────────

  /**
   * Build a structured empty-state node with an icon, title, message, and an
   * optional retry button.
   *
   * @param {string} icon      Emoji or symbol for the icon.
   * @param {string} title     Short heading.
   * @param {string} message   Longer explanation paragraph.
   * @param {Function|null} onRetry  Callback for the Retry button; omit for
   *                                  empty (not error) states.
   */
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

  /**
   * Build a structured error-state node with a message and a retry button.
   *
   * @param {string}   message  Human-readable error text.
   * @param {Function} onRetry  Callback for the Retry button.
   */
  function buildErrorState(message, onRetry) {
    var wrap = el("div", "error-state");

    var msg = el("p", "error-state-message", message);
    wrap.appendChild(msg);

    var btn = el("button", "ghost", "Retry");
    btn.type = "button";
    btn.addEventListener("click", onRetry);
    wrap.appendChild(btn);

    return wrap;
  }

  /**
   * Return an empty-state message tailored to the active status filter.
   */
  function emptyMessageForFilter(status) {
    switch (status) {
      case "pending":
        return "No pending payments. New payments will appear here once created.";
      case "completed":
        return "No completed payments match your current filters.";
      case "underpaid":
        return "No underpaid payments. Underpaid intents appear here until topped up.";
      case "expired":
        return "No expired payments in this date range.";
      default:
        return "No payments have been created yet. Create a payment intent to get started.";
    }
  }

  // ── Payments list ─────────────────────────────────────────────────────

  function reload() {
    state.cursor = null;
    state.loadedPayments = [];
    clear($("rows"));
    // Clear any previous empty/error state injected into the list area.
    var prev = $("list-state");
    if (prev) prev.parentNode.removeChild(prev);
    loadPayments();
  }

  function loadPayments() {
    if (state.loading) return;
    state.loading = true;
    setError($("list-error"), null);
    announce("Loading payments");

    var query = "/payments?limit=" + state.pageSize;
    // The list API accepts a single `status`. With one chip pressed the server
    // filters; with several, unfiltered pages are fetched and filtered below.
    // Known limitation: a page can then hold fewer matching rows than the page
    // size (even none) while "Load more" still has further pages to fetch.
    if (state.statuses.length === 1) query += "&status=" + encodeURIComponent(state.statuses[0]);
    if (state.createdAfter) query += "&created_after=" + encodeURIComponent(state.createdAfter + "T00:00:00Z");
    if (state.createdBefore) query += "&created_before=" + encodeURIComponent(state.createdBefore + "T23:59:59Z");
    if (state.cursor) query += "&cursor=" + encodeURIComponent(state.cursor);

    // Show skeleton rows only on the first page load (no cursor yet), so the
    // "Load more" path doesn't insert skeletons into an already-populated list.
    var isFirstPage = !state.cursor;
    if (isFirstPage) showSkeletonRows(5);

    api(query)
      .then(function (body) {
        clearSkeletonRows();

        var payments = body.payments || [];
        var shown = state.statuses.length > 1
          ? payments.filter(function (p) {
              return state.statuses.indexOf(p.status) >= 0;
            })
          : payments;
        state.loadedPayments = state.loadedPayments.concat(shown);
        shown.forEach(appendRow);
        syncSelectionUi();

        // Remove any previous inline state nodes before rendering new ones.
        var prev = $("list-state");
        if (prev) prev.parentNode.removeChild(prev);

        // The offset-mode response returns a cursor even on the final page, so
        // a short page is what actually signals the end.
        var more = payments.length === state.pageSize && !!body.next_cursor;
        state.cursor = more ? body.next_cursor : null;
        show($("load-more"), more);

        // #720: show filter-tailored empty state when the list is empty.
        if ($("rows").childElementCount === 0 && !more) {
          var emptyNode = buildEmptyState(
            "📭",
            "No payments found",
            state.statuses.length > 1
              ? "No payments match the selected statuses."
              : emptyMessageForFilter(state.statuses[0] || ""),
            null
          );
          emptyNode.id = "list-state";
          $("rows").parentNode.insertBefore(emptyNode, $("rows").nextSibling);
          show($("empty"), false);
        } else {
          show($("empty"), false);
        }
      })
      .catch(function (err) {
        clearSkeletonRows();

        // Remove any previous inline state nodes before rendering error.
        var prev = $("list-state");
        if (prev) prev.parentNode.removeChild(prev);

        if (err.message !== "unauthorized") {
          // #720: replace the inline error paragraph with a structured error
          // state that includes a Retry button.
          var errNode = buildErrorState(err.message, function () {
            var stateNode = $("list-state");
            if (stateNode) stateNode.parentNode.removeChild(stateNode);
            reload();
          });
          errNode.id = "list-state";
          $("rows").parentNode.insertBefore(errNode, $("rows").nextSibling);
          setError($("list-error"), null);
        }
      })
      .then(function () {
        state.loading = false;
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
        renderChipCounts(body.summary || []);
      })
      .catch(function (err) {
        clear($("summary"));
        announce("Error loading summary: " + err.message);
      });
  }

  /**
   * Show each status's count inside its filter chip, e.g. "Pending (4)". The
   * accessible name is set explicitly so screen readers hear "Pending, 4
   * payments" rather than the literal parentheses.
   */
  function renderChipCounts(rows) {
    var counts = {};
    var total = 0;
    rows.forEach(function (row) {
      counts[row[0]] = row[1];
      total += row[1];
    });
    Array.prototype.forEach.call(document.querySelectorAll(".chip"), function (chip) {
      var status = chip.getAttribute("data-status") || "";
      var label = chip.getAttribute("data-label");
      var count = status ? counts[status] || 0 : total;
      chip.textContent = label + " (" + count + ")";
      chip.setAttribute("aria-label", label + ", " + count + (count === 1 ? " payment" : " payments"));
    });
  }

  function appendRow(p) {
    var tr = document.createElement("tr");
    tr.tabIndex = 0;

    // Keep the stored record fresh when a selected row is reloaded.
    if (state.selected[p.id]) state.selected[p.id] = p;

    var selectCell = document.createElement("td");
    selectCell.setAttribute("data-label", "Select");
    var box = document.createElement("input");
    box.type = "checkbox";
    box.className = "row-select";
    box.setAttribute("data-id", p.id);
    box.setAttribute("aria-label", "Select payment " + p.id);
    box.checked = !!state.selected[p.id];
    // Stop the row's click and Enter/Space handlers from opening the detail
    // panel when the checkbox is toggled.
    box.addEventListener("click", function (ev) {
      ev.stopPropagation();
    });
    box.addEventListener("keydown", function (ev) {
      ev.stopPropagation();
    });
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

    tr.appendChild(el("td", null, formatAmount(p.amount, p.asset)));
    tr.appendChild(el("td", "mono", p.memo));
    tr.appendChild(el("td", null, fmtTime(p.created_at)));
    tr.appendChild(el("td", "mono", shortId(p.id)));

    tr.addEventListener("click", function () {
      openDetail(p.id);
    });
    tr.addEventListener("keydown", function (ev) {
      if (ev.key === "Enter" || ev.key === " ") {
        ev.preventDefault();
        openDetail(p.id);
      }
    });

    $("rows").appendChild(tr);
  }

  // ── Detail panel ──────────────────────────────────────────────────────

  function openDetail(id) {
    show($("detail"), true);
    show($("scrim"), true);

    var fields = $("detail-fields");
    clear(fields);
    clear($("deliveries"));
    setError($("deliveries-error"), null);
    show($("deliveries-empty"), false);

    // #719: show skeleton placeholders while the payment record loads.
    showDetailSkeleton();

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
          ["Expires", fmtTime(p.expires_at) + (p.status === "pending" ? " (" + countdown(p.expires_at) + " left)" : "")],
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
          // #720: show a structured error state with a retry button in the
          // detail panel instead of a bare error text node.
          var errNode = buildErrorState(err.message, function () {
            openDetail(id);
          });
          var wrapper = document.createElement("dd");
          wrapper.appendChild(errNode);
          fields.appendChild(el("dt", null, "Error"));
          fields.appendChild(wrapper);
        }
      });

    loadDeliveries(id);
  }

  function loadDeliveries(paymentId) {
    api("/payments/" + encodeURIComponent(paymentId) + "/webhooks")
      .then(function (body) {
        var list = $("deliveries");
        clear(list);
        var deliveries = body.deliveries || [];
        if (deliveries.length === 0) {
          // #720: tailored empty message for deliveries.
          show($("deliveries-empty"), true);
        } else {
          show($("deliveries-empty"), false);
          deliveries.forEach(function (d) {
            list.appendChild(deliveryItem(paymentId, d));
          });
        }
      })
      .catch(function (err) {
        if (err.message !== "unauthorized") {
          // #720: deliveries error with a retry button.
          var errNode = buildErrorState(err.message, function () {
            setError($("deliveries-error"), null);
            loadDeliveries(paymentId);
          });
          var container = $("deliveries-error").parentNode;
          // Reuse the existing deliveries-error node as an anchor for the
          // error state so we don't multiply error nodes on repeated retries.
          setError($("deliveries-error"), null);
          container.insertBefore(errNode, $("deliveries-error"));
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
      li.appendChild(el("div", "error", "Last delivery failed; check receiver logs or redeliver."));
    }
    if (d.status !== "delivered") {
      li.appendChild(el("div", "delivery-meta", "retry state: queued for redrive if attempts remain"));
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
            setError($("deliveries-error"), err.message.indexOf("429") >= 0 ? "Rate limited. Try again shortly." : err.message);
          }
        });
    });
    li.appendChild(button);

    return li;
  }

  // ── Selection (#775) ──────────────────────────────────────────────────

  function setSelected(p, on) {
    if (on) state.selected[p.id] = p;
    else delete state.selected[p.id];
  }

  function selectedPayments() {
    return Object.keys(state.selected).map(function (id) {
      return state.selected[id];
    });
  }

  /** Sync the row checkboxes, the select-all box, the count and the action. */
  function syncSelectionUi() {
    var count = Object.keys(state.selected).length;
    Array.prototype.forEach.call(document.querySelectorAll(".row-select"), function (box) {
      box.checked = !!state.selected[box.getAttribute("data-id")];
    });

    var loadedSelected = state.loadedPayments.filter(function (p) {
      return !!state.selected[p.id];
    }).length;
    var all = $("select-all");
    all.checked = state.loadedPayments.length > 0 && loadedSelected === state.loadedPayments.length;
    all.indeterminate = loadedSelected > 0 && !all.checked;

    $("selection-count").textContent = count + " selected";
    show($("selection-count"), count > 0);
    show($("export-selected"), count > 0);
  }

  function clearSelection() {
    state.selected = {};
    syncSelectionUi();
  }

  function exportCsv(payments, filename) {
    var header = ["id", "status", "amount", "asset", "asset_issuer", "memo", "destination_address", "created_at", "expires_at"];
    var lines = [header.join(",")].concat(payments.map(function (p) {
      return header.map(function (key) {
        return '"' + String(p[key] || "").replace(/"/g, '""') + '"';
      }).join(",");
    }));
    var blob = new Blob([lines.join("\n")], { type: "text/csv" });
    var url = URL.createObjectURL(blob);
    var a = document.createElement("a");
    a.href = url;
    a.download = filename;
    a.click();
    URL.revokeObjectURL(url);
  }

  function closeDetail() {
    show($("detail"), false);
    show($("scrim"), false);
  }

  // ── Version ───────────────────────────────────────────────────────────

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

  // ── Health ────────────────────────────────────────────────────────────

  function updateSessionExpiry() {
    var saved = window.localStorage.getItem(KEY_SAVED_AT) || window.sessionStorage.getItem(KEY_SAVED_AT);
    if (!saved) {
      $("session-expiry").textContent = "";
      return;
    }
    var savedAt = Number(saved);
    var expiresAt = savedAt + 30 * 24 * 60 * 60 * 1000;
    $("session-expiry").textContent = "session " + countdown(new Date(expiresAt).toISOString());
    $("session-expiry").title = "Saved " + fmtTime(new Date(savedAt).toISOString());
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

  // ── Wiring ────────────────────────────────────────────────────────────

  function syncFilterUi() {
    Array.prototype.forEach.call(document.querySelectorAll(".chip"), function (chip) {
      var status = chip.getAttribute("data-status") || "";
      var isActive = status ? state.statuses.indexOf(status) >= 0 : state.statuses.length === 0;
      chip.className = isActive ? "chip chip-on" : "chip";
      chip.setAttribute("aria-pressed", isActive ? "true" : "false");
    });
    $("auto-refresh").checked = state.autoRefresh;
  }

  function init() {
    $("gate-form").addEventListener("submit", function (ev) {
      ev.preventDefault();
      var key = $("api-key").value.trim();
      if (!key) return;
      setError($("gate-error"), null);
      signIn(key, $("remember").checked).catch(function (err) {
        if (err.message !== "unauthorized") setError($("gate-error"), err.message);
      });
    });

    $("sign-out").addEventListener("click", function () {
      signOut(null);
    });

    $("refresh").addEventListener("click", function () {
      loadSummary();
      reload();
    });
    $("export-csv").addEventListener("click", function () {
      exportCsv(state.loadedPayments, "stellargate-payments.csv");
    });
    $("export-selected").addEventListener("click", function () {
      exportCsv(selectedPayments(), "stellargate-payments-selected.csv");
    });
    $("select-all").addEventListener("change", function () {
      var on = $("select-all").checked;
      state.loadedPayments.forEach(function (p) {
        setSelected(p, on);
      });
      syncSelectionUi();
    });
    $("page-size").addEventListener("change", function () {
      state.pageSize = Number($("page-size").value) || 25;
      reload();
    });
    $("created-after").addEventListener("change", function () {
      state.createdAfter = $("created-after").value;
      clearSelection();
      reload();
    });
    $("created-before").addEventListener("change", function () {
      state.createdBefore = $("created-before").value;
      clearSelection();
      reload();
    });
    $("load-more").addEventListener("click", loadPayments);
    $("detail-close").addEventListener("click", closeDetail);
    $("scrim").addEventListener("click", closeDetail);
    $("auto-refresh").addEventListener("change", function () {
      state.autoRefresh = $("auto-refresh").checked;
      writeHashState();
    });

    document.addEventListener("keydown", function (ev) {
      if (ev.key === "Escape") closeDetail();
    });

    Array.prototype.forEach.call(
      document.querySelectorAll(".chip"),
      function (chip) {
        chip.addEventListener("click", function () {
          var status = chip.getAttribute("data-status") || "";
          var pressed = state.statuses;
          // "All" clears the selection; any other chip toggles itself.
          state.statuses = status
            ? STATUSES.filter(function (s) {
                return (s === status) !== (pressed.indexOf(s) >= 0);
              })
            : [];
          syncFilterUi();
          writeHashState();
          clearSelection();
          reload();
        });
      }
    );

    window.setInterval(function () {
      if (state.key) pollHealth();
    }, 30000);
    window.setInterval(function () {
      if (state.key && state.autoRefresh && (state.statuses.length === 0 || state.statuses.indexOf("pending") >= 0)) {
        loadSummary();
        reload();
      }
    }, 15000);

    // Resume an existing session when a key is already stored.
    /* Resume an existing session when a key is already stored. The gate is
       visible until this succeeds, so any failure here simply leaves the user
       looking at the sign-in form rather than at nothing. */
    var existing = storedKey();
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
