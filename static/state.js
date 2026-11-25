/* Single source of truth for the dashboard's view state (issue #679).
 *
 * Before this existed, filters, the pagination cursor and the loaded rows lived
 * in ad-hoc closure variables that the DOM re-read on every render, so two parts
 * of the UI could disagree about what was being shown. Here they live in one
 * object with an explicit get/set/subscribe API: a mutation goes through
 * `update()`, subscribers are told what changed, and rendering is a pure
 * function of state rather than of whatever the DOM happens to contain.
 *
 * The module is DOM-free so it can be unit tested with `node --test`
 * (issue #723). It never reads `window.location` either: the hash is parsed and
 * serialised here, but the caller applies the resulting string to the address
 * bar, which keeps this testable and keeps the "write to history" side effect
 * in one obvious place in the controller.
 */
// @ts-check

/* Sibling modules are imported with relative specifiers, not `/dashboard/...`.
 * That is deliberate: an absolute path resolves in the browser but not under
 * `node --test`, which is what makes these modules unit-testable (issue #723).
 * The file names match the routes they are served on, so `./format.js` is
 * `/dashboard/format.js` once the page loads. */
import { filterByAmount, filterPayments } from "./format.js";

/**
 * A payment as returned by `GET /payments` (see `openapi.yaml`).
 *
 * @typedef {object} Payment
 * @property {string} id
 * @property {number} amount
 * @property {string} currency
 * @property {string} status
 * @property {string} created_at
 * @property {string} [description]
 * @property {string} [customer_email]
 */

/**
 * A webhook delivery as returned by `GET /webhooks/deliveries`
 * (see `openapi.yaml`).
 *
 * @typedef {object} WebhookDelivery
 * @property {string} id
 * @property {string} event
 * @property {string} status
 * @property {number} attempts
 * @property {string} created_at
 * @property {string} [response_status]
 */

/**
 * The dashboard's view state.
 *
 * @typedef {object} DashboardState
 * @property {string|null} key
 * @property {string} status
 * @property {string} search
 * @property {number} pageSize
 * @property {string} createdAfter
 * @property {string} createdBefore
 * @property {boolean} autoRefresh
 * @property {string|null} cursor
 * @property {boolean} loading
 * @property {Payment[]} loadedPayments
 * @property {string|null} selectedPaymentId
 * @property {number} activeRow
 * @property {boolean} helpOpen
 * @property {string} merchantId
 * @property {MerchantKey[]} merchantKeys
 * @property {boolean} keysLoading
 * @property {string} keysError
 * @property {string|null} revealedKey
 * @property {string|null} revealedKeyPrefix
 * @property {string|null} sortKey
 * @property {"asc"|"desc"} sortDir
 */

/**
 * An API key as returned by `GET /v1/merchants/{id}/keys` (issue #767).
 *
 * Only the non-secret fields are modelled: the API never returns full key
 * material for an existing key, and the dashboard must never hold it either.
 *
 * @typedef {object} MerchantKey
 * @property {string} id
 * @property {string} prefix
 * @property {string} created_at
 * @property {string|null} [revoked_at]
 */

/** State a freshly loaded page starts from. */
export function initialState() {
  return {
    key: null,
    status: "",
    search: "",
    /** Asset code filter ("" = all assets). */
    asset: "",
    /** Asset codes seen on loaded payments, for the asset dropdown. */
    knownAssets: [],
    pageSize: 25,
    createdAfter: "",
    createdBefore: "",
    /** Client-side amount range filter, decimal strings (#782). */
    minAmount: "",
    maxAmount: "",
    autoRefresh: false,
    cursor: null,
    loading: false,
    loadedPayments: [],
    /** Id of the payment whose detail drawer is open, or null. */
    selectedPaymentId: null,
    /** Index of the keyboard-highlighted row, or -1 for none. */
    activeRow: -1,
    /** Whether the `?` shortcut overlay is open. */
    helpOpen: false,
    /** Merchant whose API keys the admin panel is showing (issue #767). */
    merchantId: "",
    /** Keys loaded for `merchantId`; never contains full key material. */
    merchantKeys: [],
    /** Whether the key list is being fetched. */
    keysLoading: false,
    /** Human-readable error from the last key request, or "". */
    keysError: "",
    /**
     * The one-time key material from a just-issued key (issue #768), or null.
     * Transient: never serialised to the hash, and cleared as soon as the
     * reveal panel closes.
     */
    revealedKey: null,
    /** Prefix of the revealed key, shown beside it; cleared with it. */
    revealedKeyPrefix: null,
    /** Column the payments table is sorted by (issue #770), or null. */
    sortKey: null,
    /** "asc" or "desc"; only meaningful when `sortKey` is set. */
    sortDir: "asc",
  };
}

/**
 * Build a store over `initialState()`.
 *
 * `subscribe` is called after every successful mutation with the full next
 * state and the set of keys that changed, so a subscriber can skip work it does
 * not care about without diffing the object itself.
 *
 * @param {Partial<DashboardState>} [seed]
 */
export function createStore(seed) {
  var state = Object.assign(initialState(), seed || {});
  /** @type {Array<(state: DashboardState, changed: string[]) => void>} */
  var listeners = [];

  /** The current state. Treat as read-only; mutate through update(). */
  function get() {
    return state;
  }

  /**
   * Merge a patch into the state and notify subscribers.
   *
   * Only the keys actually present in the patch are reported as changed, so a
   * subscriber can distinguish "status changed" from "state was replaced". Keys
   * set to `undefined` are dropped rather than written, which keeps a partially
   * built patch from blanking out a field.
   *
   * @param {Partial<DashboardState>} patch
   */
  function update(patch) {
    var changed = [];
    Object.keys(patch || {}).forEach(function (key) {
      if (patch[key] === undefined) return;
      if (!Object.is(state[key], patch[key])) changed.push(key);
      state[key] = patch[key];
    });
    if (changed.length > 0) emit(changed);
    return state;
  }

  /**
   * Replace the whole state (used when the hash is re-read on navigation).
   *
   * @param {Partial<DashboardState>} next
   */
  function replace(next) {
    var changed = [];
    Object.keys(next || {}).forEach(function (key) {
      if (!Object.is(state[key], next[key])) changed.push(key);
    });
    state = Object.assign(initialState(), next);
    if (changed.length > 0) emit(changed);
    return state;
  }

  /**
   * Register a subscriber. Returns an unsubscribe function so a component can
   * be torn down without leaking a listener — the dashboard's single-page nature
   * means nothing is ever torn down today, but a store that cannot be
   * unsubscribed is impossible to reuse.
   *
   * @param {(state: DashboardState, changed: string[]) => void} fn
   */
  function subscribe(fn) {
    listeners.push(fn);
    return function () {
      var i = listeners.indexOf(fn);
      if (i >= 0) listeners.splice(i, 1);
    };
  }

  /** @param {string[]} changed */
  function emit(changed) {
    // Iterate a copy: a subscriber that unsubscribes during notification must
    // not shift the list under the loop.
    listeners.slice().forEach(function (fn) {
      fn(state, changed);
    });
  }

  /* ── Derived reads ──────────────────────────────────────────────────────
   * Kept here rather than in the controller so "which rows should be on
   * screen" has exactly one answer, shared by the renderer and the `j`/`k`
   * navigation. */

  /** The rows currently on screen: loaded rows narrowed by the search box. */
  function visiblePayments() {
    var rows = filterByAmount(
      filterPayments(state.loadedPayments, state.search),
      state.minAmount,
      state.maxAmount
    );
    if (!state.asset) return rows;
    return rows.filter(function (p) { return (p.asset || "XLM") === state.asset; });
  }

  /**
   * The rows currently on screen, ordered by the active sort column
   * (issue #770).
   *
   * Sorting is client-side over the rows already loaded, so it is only a
   * complete ordering once every page has been fetched; the renderer labels it
   * as such while `cursor` is non-null. Ties fall back to the original load
   * order so the sort is stable across re-renders.
   */
  function sortedPayments() {
    var rows = visiblePayments();
    var key = state.sortKey;
    if (!key) return rows;
    var dir = state.sortDir === "desc" ? -1 : 1;
    // Decorate with the original index so equal keys keep their load order.
    return rows
      .map(function (row, index) {
        return { row: row, index: index };
      })
      .sort(function (a, b) {
        var cmp = comparePayments(a.row, b.row, key);
        if (cmp === 0) return a.index - b.index;
        return cmp * dir;
      })
      .map(function (entry) {
        return entry.row;
      });
  }

  /**
   * Compare two payments on `key`. Returns 0 for equal or missing values so
   * rows without the field keep their relative order rather than jumping.
   *
   * @param {Payment} a
   * @param {Payment} b
   * @param {string} key
   */
  function comparePayments(a, b, key) {
    var av = a ? a[key] : undefined;
    var bv = b ? b[key] : undefined;
    if (av === undefined || av === null) return 0;
    if (bv === undefined || bv === null) return 0;
    if (key === "amount") {
      var an = Number(av);
      var bn = Number(bv);
      if (isNaN(an) || isNaN(bn)) return 0;
      return an === bn ? 0 : an < bn ? -1 : 1;
    }
    // created_at is an ISO-8601 string, so lexical order is chronological.
    var as = String(av);
    var bs = String(bv);
    return as === bs ? 0 : as < bs ? -1 : 1;
  }

  /**
   * Toggle the sort on `key`: a new column starts ascending, and the active
   * column goes ascending, then descending, then back to the load order.
   *
   * @param {string} key
   */
  function toggleSort(key) {
    if (state.sortKey !== key) return update({ sortKey: key, sortDir: "asc" });
    if (state.sortDir === "asc") return update({ sortDir: "desc" });
    return update({ sortKey: null, sortDir: "asc" });
  }

  /**
   * Whether the sort covers only part of the result set: more pages remain,
   * so the renderer must label the order as client-side (issue #770).
   */
  function sortIsPartial() {
    return state.cursor !== null && state.cursor !== undefined;
  }

  /**
   * Store the one-time key material from a just-issued key (issue #768).
   * Callers must clear it via `clearRevealedKey()` once the panel closes.
   *
   * @param {string|null} material
   * @param {string|null} [prefix]
   */
  function revealKey(material, prefix) {
    return update({
      revealedKey: material === undefined ? null : material,
      revealedKeyPrefix: prefix === undefined ? null : prefix,
    });
  }

  /** Drop the revealed key from state so it is not held in memory. */
  function clearRevealedKey() {
    return update({ revealedKey: null, revealedKeyPrefix: null });
  }

  /**
   * Replace the admin key list (issue #767). Only the non-secret fields are
   * kept, so a response that carried key material cannot leave it in state.
   *
   * @param {MerchantKey[]} keys
   */
  function setMerchantKeys(keys) {
    return update({
      merchantKeys: (keys || []).map(function (k) {
        return { id: k.id, prefix: k.prefix, created_at: k.created_at, revoked_at: k.revoked_at || null };
      }),
    });
  }

  /**
   * Whether `typed` confirms revoking key `id` (issue #769): the operator must
   * retype the prefix exactly, and a revoked key cannot be revoked again.
   *
   * @param {string} id
   * @param {string} typed
   */
  function canRevokeKey(id, typed) {
    var key = (state.merchantKeys || []).filter(function (k) { return k.id === id; })[0];
    return !!key && !key.revoked_at && typed === key.prefix;
  }

  /**
   * Clamp the highlighted row to the current result set.
   *
   * A filter change can leave the cursor past the end of the list — selecting
   * row 40 and then typing in the search box must not leave `j` doing nothing.
   */
  function clampActiveRow() {
    var count = visiblePayments().length;
    if (count === 0) return update({ activeRow: -1 });
    if (state.activeRow >= count) return update({ activeRow: count - 1 });
    if (state.activeRow < -1) return update({ activeRow: -1 });
    return state;
  }

  /** Reset per-page state. Called whenever a filter changes. */
  function resetPaging() {
    return update({ cursor: null, activeRow: -1 });
  }

  /**
   * The payment whose detail drawer is open, or null.
   *
   * Selection is stored as an id rather than as a row object so that a refresh
   * which replaces `loadedPayments` does not leave the drawer pointing at a
   * stale copy of the row. Resolving the id here keeps that rule in one place.
   */
  function selectedPayment() {
    if (state.selectedPaymentId === null) return null;
    var rows = state.loadedPayments || [];
    for (var i = 0; i < rows.length; i++) {
      if (rows[i] && rows[i].id === state.selectedPaymentId) return rows[i];
    }
    return null;
  }

  /** Open the drawer for a payment id, or close it when passed null. */
  function selectPayment(id) {
    return update({ selectedPaymentId: id === undefined ? null : id });
  }

  /**
   * Append a page of rows and advance the cursor.
   *
   * `nextCursor` is whatever the API returned; a null cursor means the last
   * page has been loaded, which the renderer uses to hide "load more".
   */
  function appendPage(rows, nextCursor) {
    return update({
      loadedPayments: (state.loadedPayments || []).concat(rows || []),
      cursor: nextCursor === undefined ? null : nextCursor,
      loading: false,
    });
  }

  /** Replace the loaded rows, e.g. after a filter change or a manual refresh. */
  function setPayments(rows) {
    return update({
      loadedPayments: rows || [],
      cursor: null,
      activeRow: -1,
      loading: false,
    });
  }

  return {
    get: get,
    update: update,
    replace: replace,
    subscribe: subscribe,
    visiblePayments: visiblePayments,
    sortedPayments: sortedPayments,
    toggleSort: toggleSort,
    sortIsPartial: sortIsPartial,
    revealKey: revealKey,
    clearRevealedKey: clearRevealedKey,
    setMerchantKeys: setMerchantKeys,
    canRevokeKey: canRevokeKey,
    clampActiveRow: clampActiveRow,
    resetPaging: resetPaging,
    selectedPayment: selectedPayment,
    selectPayment: selectPayment,
    appendPage: appendPage,
    setPayments: setPayments,
  };
}

/* ── Request cancellation (#681) ──────────────────────────────────────────
 * Changing a filter quickly can let an older response overwrite newer results,
 * and closing the detail drawer can leave its fetch running. Both are fixed by
 * aborting the in-flight request when the thing that started it is superseded.
 *
 * `AbortController` is used directly: it is available in current Chrome,
 * Firefox and Safari (and their mobile builds), and the controller is created
 * per request so a stale abort can never cancel a newer one. The API key is
 * passed as a header by the caller, never appended to the URL, so nothing here
 * can leak it into a URL, a log line or the console. */

/**
 * Track one in-flight request per key and abort the previous one when a new
 * request for the same key starts.
 *
 * `key` is "list" for the payments list and "detail" for the drawer, so a
 * filter change aborts only the list request and closing the drawer aborts only
 * the detail request. Returns a handle whose `signal` is passed to `fetch` and
 * whose `done()` releases the slot once the response has been consumed.
 */
export function createRequestTracker() {
  var inflight = {};

  /**
   * Start a request under `key`, aborting any request already running for it.
   * The returned signal is aborted if a later call reuses the same key.
   */
  function begin(key) {
    abort(key);
    var controller = new AbortController();
    inflight[key] = controller;
    return {
      signal: controller.signal,
      done: function () {
        // Only clear the slot if this request is still the current one; a
        // newer request may already have replaced it.
        if (inflight[key] === controller) delete inflight[key];
      },
    };
  }

  /** Abort the in-flight request for `key`, if any. Safe to call repeatedly. */
  function abort(key) {
    var controller = inflight[key];
    if (!controller) return;
    delete inflight[key];
    controller.abort();
  }

  /** Abort every in-flight request (used when the page is torn down). */
  function abortAll() {
    Object.keys(inflight).forEach(abort);
  }

  return { begin: begin, abort: abort, abortAll: abortAll };
}

/* ── Display preferences (#771, #772) ─────────────────────────────────────
 * Page size, auto-refresh, theme, timezone and column visibility are display
 * preferences, not secrets, so they are persisted in `localStorage` under a
 * dedicated `stellargate.prefs.` namespace. That namespace is deliberately
 * disjoint from the API-key keys owned by `session.js`: nothing here reads or
 * writes the key, and nothing here logs a value, so a preference can never
 * carry the key into storage, a URL or the console.
 *
 * Every read is defensive. `localStorage` can throw (Safari private mode, a
 * disabled cookie policy) and a stored value can be hand-edited, so a bad or
 * missing entry falls back to the default rather than breaking the page. */

/** Prefix for every preference key. Never used for the API key. */
export const PREFS_NAMESPACE = "stellargate.prefs.";

/** The themes the stylesheet understands. */
export const THEMES = ["system", "light", "dark"];

/** Page sizes offered in the settings dialog. */
export const PAGE_SIZES = [10, 25, 50, 100];

/** Auto-refresh intervals (seconds) offered in the settings dialog (#813). */
export const REFRESH_INTERVALS = [10, 30, 60, 300];

/**
 * A table column.
 *
 * `alwaysVisible` columns (Status and Payment ID) have no checkbox in the
 * column menu and cannot be hidden: the status is the point of the table and
 * the id is what a merchant quotes to support.
 *
 * @typedef {object} Column
 * @property {string} id
 * @property {string} label
 * @property {boolean} alwaysVisible
 */

/** Every table column, in display order. The single source of truth. */
export const COLUMNS = [
  { id: "id", label: "Payment ID", alwaysVisible: true },
  { id: "created_at", label: "Created", alwaysVisible: false },
  { id: "amount", label: "Amount", alwaysVisible: false },
  { id: "currency", label: "Currency", alwaysVisible: false },
  { id: "status", label: "Status", alwaysVisible: true },
  { id: "description", label: "Memo", alwaysVisible: false },
  { id: "customer_email", label: "Customer", alwaysVisible: false },
];

/** The columns that can be toggled in the column menu. */
export function toggleableColumns() {
  return COLUMNS.filter(function (column) {
    return !column.alwaysVisible;
  });
}

/** The default display preferences. */
export function defaultPrefs() {
  var columns = {};
  COLUMNS.forEach(function (column) {
    columns[column.id] = true;
  });
  return {
    pageSize: 25,
    autoRefresh: false,
    refreshIntervalSec: 30,
    theme: "system",
    timezone: "local",
    columns: columns,
  };
}

/** Coerce a stored page size to one of PAGE_SIZES, else the default. */
function coercePageSize(value) {
  var n = Number(value);
  return PAGE_SIZES.indexOf(n) >= 0 ? n : defaultPrefs().pageSize;
}

/** Coerce a stored refresh interval to one of REFRESH_INTERVALS, else the default. */
function coerceRefreshInterval(value) {
  var n = Number(value);
  return REFRESH_INTERVALS.indexOf(n) >= 0 ? n : defaultPrefs().refreshIntervalSec;
}

/** Coerce a stored theme to one of THEMES, else the default. */
function coerceTheme(value) {
  return THEMES.indexOf(value) >= 0 ? value : defaultPrefs().theme;
}

/**
 * Coerce a stored timezone. "local" and "utc" are the two the dashboard
 * renders; anything else falls back to "local".
 */
function coerceTimezone(value) {
  return value === "utc" ? "utc" : "local";
}

/**
 * Coerce a stored column map. Always-visible columns are forced on, unknown
 * ids are dropped, and a missing id takes its default, so a stale entry from an
 * older build cannot hide a column that no longer has a checkbox.
 */
function coerceColumns(value) {
  var out = {};
  var stored = value && typeof value === "object" ? value : {};
  COLUMNS.forEach(function (column) {
    if (column.alwaysVisible) {
      out[column.id] = true;
    } else {
      out[column.id] = stored[column.id] === undefined ? true : !!stored[column.id];
    }
  });
  return out;
}

/**
 * Normalise an arbitrary object into a complete, valid preference set.
 *
 * @param {Partial<ReturnType<typeof defaultPrefs>>} [raw]
 */
export function normalizePrefs(raw) {
  var defaults = defaultPrefs();
  var s = raw && typeof raw === "object" ? raw : {};
  return {
    pageSize: coercePageSize(s.pageSize),
    autoRefresh: !!s.autoRefresh,
    refreshIntervalSec: coerceRefreshInterval(s.refreshIntervalSec),
    theme: coerceTheme(s.theme),
    timezone: coerceTimezone(s.timezone),
    columns: coerceColumns(s.columns),
  };
}

/**
 * The storage backing the preferences. Defaults to `window.localStorage` when
 * present; tests pass a stub. Returns null when no storage is available.
 *
 * @param {Storage} [storage]
 */
function resolveStorage(storage) {
  if (storage) return storage;
  try {
    if (typeof localStorage !== "undefined" && localStorage) return localStorage;
  } catch (e) {
    // Accessing localStorage can throw when storage is disabled.
  }
  return null;
}

/** Read one namespaced key, returning null on any failure. */
function readKey(storage, name) {
  try {
    return storage.getItem(PREFS_NAMESPACE + name);
  } catch (e) {
    return null;
  }
}

/** Write one namespaced key, ignoring quota or disabled-storage failures. */
function writeKey(storage, name, value) {
  try {
    storage.setItem(PREFS_NAMESPACE + name, value);
  } catch (e) {
    // A full or disabled store must not break the page.
  }
}

/**
 * Load the persisted preferences, falling back to defaults for anything
 * missing or invalid. Never throws.
 *
 * @param {Storage} [storage]
 */
export function loadPrefs(storage) {
  var store = resolveStorage(storage);
  if (!store) return defaultPrefs();
  var raw = {
    pageSize: readKey(store, "pageSize"),
    autoRefresh: readKey(store, "autoRefresh") === "1",
    refreshIntervalSec: readKey(store, "refreshIntervalSec"),
    theme: readKey(store, "theme"),
    timezone: readKey(store, "timezone"),
    columns: null,
  };
  var columnsRaw = readKey(store, "columns");
  if (columnsRaw) {
    try {
      raw.columns = JSON.parse(columnsRaw);
    } catch (e) {
      raw.columns = null;
    }
  }
  return normalizePrefs(raw);
}

/**
 * Persist a preference set. Values are normalised first so an invalid value is
 * never written. Never throws.
 *
 * @param {Partial<ReturnType<typeof defaultPrefs>>} prefs
 * @param {Storage} [storage]
 */
export function savePrefs(prefs, storage) {
  var store = resolveStorage(storage);
  var normalized = normalizePrefs(prefs);
  if (!store) return normalized;
  writeKey(store, "pageSize", String(normalized.pageSize));
  writeKey(store, "autoRefresh", normalized.autoRefresh ? "1" : "0");
  writeKey(store, "refreshIntervalSec", String(normalized.refreshIntervalSec));
  writeKey(store, "theme", normalized.theme);
  writeKey(store, "timezone", normalized.timezone);
  writeKey(store, "columns", JSON.stringify(normalized.columns));
  return normalized;
}

/**
 * Remove every namespaced preference key and return the defaults.
 *
 * Only keys under PREFS_NAMESPACE are removed, so the API-key keys owned by
 * `session.js` are untouched. Never throws.
 *
 * @param {Storage} [storage]
 */
export function resetToDefaults(storage) {
  var store = resolveStorage(storage);
  if (store) {
    var doomed = [];
    try {
      for (var i = 0; i < store.length; i++) {
        var name = store.key(i);
        if (name && name.indexOf(PREFS_NAMESPACE) === 0) doomed.push(name);
      }
    } catch (e) {
      // Fall through to the known keys below.
    }
    // Remove the known keys even if enumeration failed.
    ["pageSize", "autoRefresh", "refreshIntervalSec", "theme", "timezone", "columns"].forEach(function (name) {
      doomed.push(PREFS_NAMESPACE + name);
    });
    doomed.forEach(function (name) {
      try {
        store.removeItem(name);
      } catch (e) {
        // Ignore a disabled store.
      }
    });
  }
  return defaultPrefs();
}

/* ── URL-hash serialisation (#696) ────────────────────────────────────────
 * Filters live in the hash, never the query string. That is not a style
 * preference: anything in `location.search` is sent to the server, logged by
 * proxies, and pasted into bug reports. The API key is never in either. */

const HASH_KEYS = ["status", "search", "auto_refresh"];

/**
 * Read filter state out of a location hash such as
 * "#status=pending&auto_refresh=1".
 *
 * Only the keys in HASH_KEYS are honoured. An unrecognised key is ignored
 * rather than copied into state, so a hand-edited URL cannot set an arbitrary
 * field. `merchantId`, `revealedKey` and any key material are deliberately not
 * in HASH_KEYS: they must never appear in a URL (issues #767–#769).
 *
 * @param {string} hash
 */
export function parseHash(hash) {
  var out = { status: "", search: "", autoRefresh: false };
  var raw = String(hash || "").replace(/^#/, "");
  if (!raw) return out;
  var params;
  try {
    params = new URLSearchParams(raw);
  } catch (e) {
    return out;
  }
  HASH_KEYS.forEach(function (key) {
    if (!params.has(key)) return;
    var value = params.get(key);
    if (key === "auto_refresh") {
      out.autoRefresh = value === "1" || value === "true";
    } else {
      out[key] = value || "";
    }
  });
  return out;
}

/**
 * Serialise the filter fields of a state object to a hash string, without the
 * leading "#". Returns "" when no filter is set, which the controller turns into
 * a plain `location.pathname` so the URL does not collect a bare "#".
 *
 * Only HASH_KEYS are written, so the merchant id, the revealed key and the
 * loaded key list can never reach the address bar even if they are present in
 * the state object passed in.
 *
 * @param {Partial<DashboardState>} state
 */
export function serializeHash(state) {
  var s = state || {};
  var params = new URLSearchParams();
  HASH_KEYS.forEach(function (key) {
    var value = key === "auto_refresh" ? (s.autoRefresh ? "1" : "") : s[key];
    if (value) params.set(key, value);
  });
  return params.toString();
}
