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
 * close-session bookkeeping honest across that handshake:
 *
 *  * the label is NOT cleared in a `finally` after issuing `close_site` —
 *    the close may still be pending or may have failed (that was defect
 *    A.5: success claimed while Rust/JS disagreed);
 *  * `close_site` failing marks the session `failed:<msg>` instead of
 *    silently going idle, so the next back press retries cleanly;
 *  * the `animehub://site-closed` event clears the session only when the
 *    payload provably belongs to it: the legacy typed event's `sessionId`
 *    must match exactly, and the current `{ siteId, label }` payload (or the
 *    legacy bare-string payload) must name the tracked label or site id. A
 *    site id alone is not an opening identity: closing and reopening the
 *    same site produces two sessions with the same site id, so a delayed
 *    event from the first opening must not clear the second (the typed
 *    `sessionId` contract guards that; a mid-rollout binary may still see a
 *    repeated label clear, which the user can retry).
 *  * an `open_site` result is committed only while it is still the newest
 *    attempt, so two rapid tile clicks cannot leave the label on the losing
 *    site when their invokes resolve out of order.
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
  /**
   * Monotonic attempt counter. An `open_site` result may arrive after a newer
   * attempt has started; only the newest attempt may replace the label.
   */
  let openGeneration = 0;
  /** idle | pending | closed | failed:<message> */
  let state = "idle";

  /**
   * Validate the native close payload and extract the identity it carries.
   *
   * Accepted shapes, newest first:
   *  * `{ siteId, label }` — the contract the Rust side emits now;
   *  * a bare site-id string — the pre-v0.4 producer (one-line shim, so a
   *    mid-rollout binary cannot brick the back path);
   *  * `{ siteId, sessionId }` — the legacy typed `SiteClosedEvent`, verified
   *    exactly against the opening identity (kept for a mixed rollout).
   * Anything else is malformed and proves nothing about ownership. Returning
   * `null` here makes malformed data unable to clear a live session, which is
   * the fail-closed direction.
   */
  function closeEventIdentity(payload) {
    if (typeof payload === "string") {
      // Legacy string payload: compared against both tracked identities.
      return payload ? { label: payload, siteId: payload } : null;
    }
    if (!payload || typeof payload !== "object" || Array.isArray(payload)) {
      return null;
    }
    const { siteId, label, sessionId } = payload;
    if (typeof sessionId === "string" && sessionId) {
      if (typeof siteId !== "string" || !siteId) return null;
      return { siteId, sessionId };
    }
    const id = typeof siteId === "string" && siteId ? siteId : null;
    if (typeof label === "string" && label) return { label, siteId: id };
    if (id) return { siteId: id };
    return null;
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
      if (label === target && sessionId === targetSessionId) {
        settle("failed", message);
        if (typeof notify === "function") notify(message);
      }
    }
  }

  return {
    /**
     * Start an opening attempt and return its generation token. Pass the token
     * to [opened] so a slow result cannot overwrite a newer attempt.
     */
    beginOpen() {
      openGeneration += 1;
      return openGeneration;
    },

    /**
     * Take ownership after `open_site` succeeded. Resets any leftover
     * failure state: the new opening starts clean.
     *
     * `newSessionId` is the native opening identity. Without it, no close
     * event can prove ownership, so such a session can only be cleared by the
     * explicit `reset()` path.
     *
     * `generation` is the token from [beginOpen]. When supplied, the commit is
     * refused if a newer attempt has started; returns whether it committed.
     */
    opened(newLabel, newSiteId, newSessionId, generation) {
      if (typeof generation === "number" && generation !== openGeneration) return false;
      label = newLabel || null;
      siteId = newSiteId ?? null;
      sessionId = typeof newSessionId === "string" && newSessionId ? newSessionId : null;
      state = "idle";
      return true;
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

    /**
     * `animehub://site-closed` bookkeeping. Clears the session only when the
     * event provably belongs to this session:
     *  * a legacy typed event must match the owned `sessionId` exactly (and
     *    the site id, when the session has one);
     *  * the new `{ siteId, label }` contract (or the legacy string payload)
     *    must name the tracked label or the tracked site id.
     *
     * A missing/unknown/malformed identity or an event from another session
     * is ignored. It never clears the active session by accident (fail
     * closed).
     */
    closedByEvent(payload) {
      if (!label) return false;
      const event = closeEventIdentity(payload);
      if (!event) return false;
      if (event.sessionId) {
        if (!sessionId || event.sessionId !== sessionId) return false;
        if (siteId && event.siteId !== siteId) return false;
      } else {
        const byLabel = event.label === label;
        const bySiteId =
          event.siteId != null &&
          (event.siteId === siteId || event.siteId === label);
        if (!byLabel && !bySiteId) return false;
      }
      label = null;
      siteId = null;
      sessionId = null;
      state = "idle";
      return true;
    },
  };
}
