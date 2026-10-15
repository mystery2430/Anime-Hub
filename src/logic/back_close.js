// SPDX-License-Identifier: MIT
/**
 * Launcher-side close session for the Android site-back handshake.
 *
 * WHY THIS EXISTS
 * ---------------
 * When the site WebView's history is exhausted, `AnimeHubSiteController`
 * (Kotlin) evaluates `window.__animehubBackToLauncher()` inside the launcher
 * page and then polls `window.__animehubBackStatus()`. The controller never
 * trusts a bare "it ran" for success: it needs the *real* outcome of the
 * normal `close_site` flow — pending / closed / failed:<reason> — so a failed
 * close is surfaced (a Toast above the site view) and a slow flush is simply
 * waited for instead of double-closed. The launcher must therefore keep its
 * `openSiteLabel` bookkeeping honest across that handshake:
 *
 *  * the label is NOT cleared in a `finally` after issuing `close_site` —
 *    the close may still be pending or may have failed (that was defect
 *    A.5: success claimed while Rust/JS disagreed);
 *  * `close_site` failing marks the session `failed:<msg>` instead of
 *    silently going idle, so the next back press retries cleanly;
 *  * the `animehub://site-closed` event clears the session only when the
 *    payload matches the site this session owns — a stale event from an
 *    earlier close can never orphan an active one.
 *
 * Pure logic: no DOM, no Tauri import — `main.js` injects the `call`
 * bridge, the notice callback and the error formatter, and the tests drive
 * every transition with fakes.
 */

/**
 * Create a back/close session state machine.
 *
 * @param {object} deps
 * @param {(cmd: string, args?: object) => Promise<any>} deps.call
 *   Tauri invoke bridge (`close_site` etc.).
 * @param {(message: string) => void} [deps.notify]
 *   Called with the human-readable reason when a close fails (the site view
 *   is still up, so the launcher shows whatever it can; Kotlin shows a Toast).
 * @param {(e: unknown) => string} [deps.errorText]
 *   Formats a rejected invoke into a readable message.
 */
export function createBackSession({ call, notify, errorText }) {
  /** Active site window label; null when nothing is open (or fully closed). */
  let label = null;
  /** Site id that owns this session (guard for stale close events). */
  let siteId = null;
  /** idle | pending | closed | failed:<message> */
  let state = "idle";
  /** Reason of the last failure (kept for diagnostics/retry messages). */
  let lastError = "";

  function settle(next, message) {
    state = message ? `${next}:${message}` : next;
  }

  async function runClose(target) {
    settle("pending");
    try {
      await call("close_site", { label: target });
      // The session may already have been re-opened for another site while
      // this call was in flight — never clobber the newer owner.
      if (label === target) settle("closed");
    } catch (e) {
      const message = errorText ? errorText(e) : String(e);
      lastError = message;
      if (label === target) {
        settle("failed", message);
        if (typeof notify === "function") notify(message);
      }
    }
  }

  return {
    /**
     * Take ownership after `open_site` succeeded. Resets any leftover
     * failure state: the new site starts clean.
     */
    opened(newLabel, newSiteId) {
      label = newLabel || null;
      siteId = newSiteId ?? null;
      state = "idle";
      lastError = "";
    },

    /** Whether a close can be started right now (the Kotlin hook's value). */
    hasSession() {
      return Boolean(label);
    },

    /**
     * Request the normal close flow. Returns `{accepted}`:
     *  * `false` — no session (Kotlin falls back to a native close);
     *  * `true`  — a close is started, or is already pending (never two
     *              concurrent `close_site` calls), or a previous failure is
     *              being retried.
     */
    requestClose() {
      if (!label) return { accepted: false };
      if (state === "pending") return { accepted: true };
      if (state === "closed") {
        // Already done (event may be on its way) — report accepted so the
        // poller observes `closed` instead of falling back.
        return { accepted: true };
      }
      void runClose(label);
      return { accepted: true };
    },

    /** The exact string Kotlin's `SiteBackStatus.decide` consumes. */
    statusString() {
      return state;
    },

    /** Last failure reason ("" when none). */
    lastError() {
      return lastError;
    },

    /**
     * `animehub://site-closed` bookkeeping. Clears the session only when the
     * event belongs to it: a payload matching the owned site (or an
     * unverifiable payload with no owned id) clears; anything else is stale
     * and ignored.
     */
    closedByEvent(payloadSiteId) {
      if (siteId && payloadSiteId && payloadSiteId !== siteId) return false;
      label = null;
      siteId = null;
      state = "idle";
      lastError = "";
      return true;
    },

    /**
     * Clear regardless of ownership — used where the caller has already
     * awaited the close outcome itself (never as a `finally` shortcut).
     */
    reset() {
      label = null;
      siteId = null;
      state = "idle";
      lastError = "";
    },

    /** The label the session owns (null when idle) — for main.js sync. */
    currentLabel() {
      return label;
    },
  };
}
