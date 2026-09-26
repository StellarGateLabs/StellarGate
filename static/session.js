/* API-key session storage for the dashboard.
 *
 * Isolating the storage rules here (issue #678) keeps them out of the DOM
 * layer and, more importantly, makes them testable: the store takes its
 * storage areas and its clock as arguments, so `node --test` can drive it with
 * plain objects (issue #723) and a browser can pass the real `sessionStorage` /
 * `localStorage`.
 *
 * Security note — the reason this is worth getting right: the key is a bearer
 * credential. It is written to Web Storage, which is readable by any script on
 * this origin, so the rules that keep it out of *the rest* of the browser
 * (URLs, history, referrers, logs) are the ones that matter. Nothing in this
 * module ever puts the key into a URL, and `describe()` below is careful to
 * report only the prefix, never the secret.
 */

export const KEY_NAME = "stellargate.apiKey";
export const KEY_SAVED_AT = "stellargate.apiKeySavedAt";

/**
 * How long a remembered key is treated as live before the dashboard stops
 * showing a session-expiry hint. This mirrors the server's key lifetime guidance
 * and is only ever a *display* concern — the server is the authority on whether
 * a key still works, and a rejected key signs the operator out immediately.
 */
export const SESSION_TTL_MS = 30 * 24 * 60 * 60 * 1000;

/**
 * An in-memory Storage-shaped object.
 *
 * Used as the default so importing this module in a bare Node process (or any
 * context where Web Storage is unavailable or throws) still yields a working
 * store rather than a crash. The dashboard's sign-in gate is the safety net if
 * the key cannot persist: the operator re-enters it on reload.
 */
export function memoryStorage() {
  var map = Object.create(null);
  return {
    getItem: function (k) {
      return Object.prototype.hasOwnProperty.call(map, k) ? map[k] : null;
    },
    setItem: function (k, v) {
      map[k] = String(v);
    },
    removeItem: function (k) {
      delete map[k];
    },
  };
}

/** Read a value from a Storage-shaped object, tolerating one that throws. */
function safeGet(area, name) {
  try {
    return area ? area.getItem(name) : null;
  } catch (e) {
    return null;
  }
}

function safeSet(area, name, value) {
  try {
    area.setItem(name, value);
    return true;
  } catch (e) {
    return false;
  }
}

function safeRemove(area, name) {
  try {
    area.removeItem(name);
  } catch (e) {
    /* nothing to do */
  }
}

/**
 * Build a session store.
 *
 * @param {object}  [opts]
 * @param {object}  [opts.session]  Storage area for "this tab only" keys.
 * @param {object}  [opts.local]    Storage area for "remember me" keys.
 * @param {function} [opts.now]     Clock returning epoch milliseconds.
 */
export function createSessionStore(opts) {
  var o = opts || {};
  var session = o.session || memoryStorage();
  var local = o.local || memoryStorage();
  var now =
    o.now ||
    function () {
      return Date.now();
    };

  /**
   * The stored key, or null.
   *
   * Session storage is checked first so a key entered for this tab wins over a
   * remembered one — signing in without "remember me" must not leave the
   * remembered key in charge of the next request.
   */
  function read() {
    return safeGet(session, KEY_NAME) || safeGet(local, KEY_NAME) || null;
  }

  /**
   * Persist a key, plus the timestamp the session-expiry hint is derived from.
   * `persist` selects local (remembered) or session (this tab) storage.
   * Returns whether the key was actually stored; a `false` is non-fatal, the key
   * simply does not survive a reload.
   */
  function write(key, persist) {
    if (!key) return false;
    var area = persist ? local : session;
    var stored = safeSet(area, KEY_NAME, key);
    safeSet(area, KEY_SAVED_AT, String(now()));
    return stored;
  }

  /** Drop the key from both areas. Used on sign-out and on a 401. */
  function clear() {
    safeRemove(session, KEY_NAME);
    safeRemove(session, KEY_SAVED_AT);
    safeRemove(local, KEY_NAME);
    safeRemove(local, KEY_SAVED_AT);
  }

  /** Epoch ms the stored key was saved at, or null when nothing is stored. */
  function savedAt() {
    var raw = safeGet(session, KEY_SAVED_AT) || safeGet(local, KEY_SAVED_AT);
    if (!raw) return null;
    var n = Number(raw);
    return isFinite(n) ? n : null;
  }

  /** Epoch ms the stored key's display window elapses, or null. */
  function expiresAt() {
    var at = savedAt();
    return at === null ? null : at + SESSION_TTL_MS;
  }

  /** True when a key is stored at all, regardless of whether it is still valid. */
  function has() {
    return read() !== null;
  }

  /**
   * A non-reversible description of the stored key, safe to show or log.
   *
   * Reports the length and the first few characters only. Nothing in the
   * dashboard should ever need the secret itself for a display purpose, and
   * having one function that is explicitly safe to render removes the
   * temptation to reach for `read()` when building UI text.
   */
  function describe() {
    var key = read();
    if (!key) return null;
    return { length: key.length, prefix: key.slice(0, 8) };
  }

  return {
    read: read,
    write: write,
    clear: clear,
    savedAt: savedAt,
    expiresAt: expiresAt,
    has: has,
    describe: describe,
  };
}
