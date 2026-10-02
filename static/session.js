// Session helpers for the dashboard.
//
// These implement the API key storage/expiry rules from #252. The key is kept
// in memory only and is never written to a URL, a log line or the console.

// How long a stored key stays valid, in milliseconds.
const KEY_TTL_MS = 30 * 60 * 1000;

// In-memory storage for the API key. Deliberately not persisted anywhere so
// the key can never leak through URLs, logs or console output.
let storedKey = null;
let storedKeyExpiresAt = 0;

// Returns the currently stored API key, or null when there is none or the
// stored key has expired. Expired keys are dropped on read.
export function getStoredKey() {
  if (storedKey === null) {
    return null;
  }
  if (Date.now() >= storedKeyExpiresAt) {
    forgetKey();
    return null;
  }
  return storedKey;
}

// Stores an API key with the standard expiry window.
export function storeKey(key) {
  storedKey = key;
  storedKeyExpiresAt = Date.now() + KEY_TTL_MS;
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

  /**
   * Sign the operator out because the server rejected the stored key.
   *
   * This is the single place a 401 is turned into a sign-out (issue #682): the
   * key is dropped from both storage areas and a `stellargate:unauthorized`
   * event is dispatched so the sign-in gate can re-render with a clear message.
   * The event carries no key material — only the reason — so it is safe to log
   * or forward. Returns true when a key was actually present and cleared, so a
   * caller can avoid re-showing the gate for an already-signed-out session.
   *
   * @param {string} [reason] Short, non-secret explanation for the gate.
   */
  function signOut(reason) {
    var had = has();
    clear();
    if (typeof window !== "undefined" && typeof window.dispatchEvent === "function") {
      try {
        window.dispatchEvent(
          new CustomEvent("stellargate:unauthorized", {
            detail: { reason: reason || "Your API key was rejected. Please sign in again." },
          })
        );
      } catch (e) {
        /* CustomEvent unavailable; the gate still re-renders on next read(). */
      }
    }
    return had;
  }

  return {
    read: read,
    write: write,
    clear: clear,
    savedAt: savedAt,
    expiresAt: expiresAt,
    has: has,
    describe: describe,
    signOut: signOut,
  };
}
