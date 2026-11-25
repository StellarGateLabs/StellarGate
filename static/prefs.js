/* Preferences store for the dashboard (#771 / #772).
 *
 * `app.js` imports `createPrefsStore`, `COLUMNS` and `ALWAYS_VISIBLE` from
 * here. The storage rules themselves (the "stellargate.prefs." namespace,
 * validation of every stored value, reset that never touches the API key) live
 * in `state.js`; this module only wraps them in the small get/update/reset
 * store the page uses, so there is one implementation of those rules.
 *
 * DOM-free so it runs under `node --test` (issue #723).
 */

import {
  COLUMNS as COLUMN_DEFS,
  loadPrefs,
  resetToDefaults,
  savePrefs,
} from "./state.js";

/** Column ids in display order. */
export const COLUMNS = COLUMN_DEFS.map(function (column) {
  return column.id;
});

/** Column ids that are always shown and cannot be hidden. */
export const ALWAYS_VISIBLE = COLUMN_DEFS.filter(function (column) {
  return column.alwaysVisible;
}).map(function (column) {
  return column.id;
});

/**
 * A store over the persisted preferences.
 *
 * `get()` returns the current, always-valid preference set. `update(partial)`
 * merges, normalises and persists it. `reset()` clears every namespaced key
 * and returns the defaults. None of them throw when storage is unavailable:
 * the preferences then live for the page only.
 *
 * @param {{ local?: Storage | null }} [options]
 */
export function createPrefsStore(options) {
  var storage = (options && options.local) || null;
  var memory = null;
  if (!storage) {
    var data = Object.create(null);
    memory = {
      get length() {
        return Object.keys(data).length;
      },
      key: function (i) {
        return Object.keys(data)[i] || null;
      },
      getItem: function (k) {
        return k in data ? data[k] : null;
      },
      setItem: function (k, v) {
        data[k] = String(v);
      },
      removeItem: function (k) {
        delete data[k];
      },
    };
  }
  var backing = storage || memory;
  var current = loadPrefs(backing);

  return {
    get: function () {
      return current;
    },
    update: function (partial) {
      current = savePrefs(Object.assign({}, current, partial || {}), backing);
      return current;
    },
    reset: function () {
      current = resetToDefaults(backing);
      return current;
    },
  };
}
