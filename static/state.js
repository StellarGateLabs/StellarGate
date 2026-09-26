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

/* Sibling modules are imported with relative specifiers, not `/dashboard/...`.
 * That is deliberate: an absolute path resolves in the browser but not under
 * `node --test`, which is what makes these modules unit-testable (issue #723).
 * The file names match the routes they are served on, so `./format.js` is
 * `/dashboard/format.js` once the page loads. */
import { filterPayments } from "./format.js";
/** State a freshly loaded page starts from. */
export function initialState() {
  return {
    key: null,
    status: "",
    search: "",
    pageSize: 25,
    createdAfter: "",
    createdBefore: "",
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
  };
}

/**
 * Build a store over `initialState()`.
 *
 * `subscribe` is called after every successful mutation with the full next
 * state and the set of keys that changed, so a subscriber can skip work it does
 * not care about without diffing the object itself.
 */
export function createStore(seed) {
  var state = Object.assign(initialState(), seed || {});
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

  /** Replace the whole state (used when the hash is re-read on navigation). */
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
   */
  function subscribe(fn) {
    listeners.push(fn);
    return function () {
      var i = listeners.indexOf(fn);
      if (i >= 0) listeners.splice(i, 1);
    };
  }

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
    return filterPayments(state.loadedPayments, state.search);
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

  return {
    get: get,
    update: update,
    replace: replace,
    subscribe: subscribe,
    visiblePayments: visiblePayments,
    clampActiveRow: clampActiveRow,
    resetPaging: resetPaging,
  };
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
 * field.
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
