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
 *  * the `animehub://site-closed` event clears the session only when its
 *    payload carries the exact opaque `sessionId` returned by THIS
 *    `open_site`. A site id is not an opening identity: closing and reopening
 *    the same site produces two sessions with the same site id, so a delayed
 *    event from the first opening must not clear the second.
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
  /** Site id that owns this session (diagnostics / extra stale-event guard). */
  let siteId = null;
  /**
   * Opaque identity of the exact opening this session owns. Returned by the
   * native `open_site` command and echoed by `animehub://site-closed`.
   */
  let sessionId = null;
  /** idle | pending | closed | failed:<message> */
  let state = "idle";
  /** Reason of the last failure (kept for diagnostics/retry messages). */
  let lastError = "";

  /**
   * Validate the native close payload.
   *
   * The event is a serialized `SiteClosedEvent`: both fields must be
   * non-empty strings. Anything else is malformed and proves nothing about
   * ownership. Returning `null` here makes malformed data unable to clear a
   * live session, which is the fail-closed direction.
   */
  function closeEventIdentity(payload) {
    if (!payload || typeof payload !== "object" || Array.isArray(payload)) {
      return null;
    }
    const { siteId: payloadSiteId, sessionId: payloadSessionId } = payload;
    if (typeof payloadSiteId !== "string" || payloadSiteId.length === 0) {
      return null;
    }
    if (typeof payloadSessionId !== "string" || payloadSessionId.length === 0) {
      return null;
    }
    return { siteId: payloadSiteId, sessionId: payloadSessionId };
  }

  function settle(next, message) {
    state = message ? `${next}:${message}` : next;
  }

  async function runClose(target, operationId, targetSessionId) {
    settle("pending");
    try {
      const args = { label: target };
      // Required by the native command: labels are reused, session IDs aren't.
      if (typeof targetSessionId === "string" && targetSessionId) args.sessionId = targetSessionId;
      if (typeof operationId === "string") args.operationId = operationId;
      await call("close_site", args);
      // The session may already have been re-opened while this call was in
      // flight — even the SAME site, which reuses the label. Only settle the
      // outcome when this is still the exact opening that requested it.
      if (label === target && sessionId === targetSessionId) settle("closed");
    } catch (e) {
      const message = errorText ? errorText(e) : String(e);
      lastError = message;
      if (label === target && sessionId === targetSessionId) {
        settle("failed", message);
        if (typeof notify === "function") notify(message);
      }
    }
  }

  return {
    /**
     * Take ownership after `open_site` succeeded. Resets any leftover
     * failure state: the new opening starts clean.
     *
     * `newSessionId` is the native opening identity. Without it, no close
     * event can prove ownership, so such a session can only be cleared by the
     * explicit `reset()` path.
     */
    opened(newLabel, newSiteId, newSessionId) {
      label = newLabel || null;
      siteId = newSiteId ?? null;
      sessionId = typeof newSessionId === "string" && newSessionId ? newSessionId : null;
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
    requestClose(operationId) {
      if (!label) return { accepted: false };
      if (state === "pending") return { accepted: true };
      if (state === "closed") {
        // Already done (event may be on its way) — report accepted so the
        // poller observes `closed` instead of falling back.
        return { accepted: true };
      }
      void runClose(label, operationId, sessionId);
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
     * event provably belongs to this exact opening:
     *  * the session must have an owned opening identity;
     *  * the payload must be a well-formed native close event with non-empty
     *    `siteId` and `sessionId` strings;
     *  * `sessionId` must match EXACTLY, and the payload's site id must match
     *    when this session has one.
     *
     * A missing/unknown identity, malformed payload, or an event from a
     * previous opening of the same site is ignored. It never clears the
     * active session, including when no identity is owned (fail closed).
     */
    closedByEvent(payload) {
      if (!sessionId) return false;
      const event = closeEventIdentity(payload);
      if (!event) return false;
      if (event.sessionId !== sessionId) return false;
      if (siteId && event.siteId !== siteId) return false;
      label = null;
      siteId = null;
      sessionId = null;
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
      sessionId = null;
      state = "idle";
      lastError = "";
    },

    /** The label the session owns (null when idle) — for main.js sync. */
    currentLabel() {
      return label;
    },

    /** The opening identity this session owns (null when idle). */
    currentSessionId() {
      return sessionId;
    },
  };
}
