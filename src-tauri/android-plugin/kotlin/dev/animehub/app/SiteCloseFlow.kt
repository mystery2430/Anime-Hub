// SPDX-License-Identifier: MIT
//
// Close flow: flush-then-destroy for a normal close, keep-everything on failure.
//
// Pure decision/sequencing code: this file must NOT import `android.*` or any
// Tauri class, so the CI unit-test source set compiles and runs it against the
// generated Android project (see SiteProfileOps.kt for the full rationale and
// the one-thread-per-flow rule). Split out of SiteProfileOps.kt so each
// responsibility — and what may change it — is visible at a glance.

package dev.animehub.app

import java.util.concurrent.atomic.AtomicBoolean

// ---------------------------------------------------------------------------
// Close flow: normal close flushes first and keeps the view on failure
// ---------------------------------------------------------------------------

/**
 * The Android operations [SiteCloseFlow] needs. The controller implements
 * these over the active WebView/Profile; tests implement them over fakes.
 */
interface SiteClosePort {
  /** Whether a site view is currently active (idempotent-close check). */
  fun hasActiveView(): Boolean

  /**
   * Resolve the profile backing the active view via
   * `WebViewCompat.getProfile(view).name`. Errors MUST surface: a close that
   * cannot resolve its profile cannot claim cookie durability.
   */
  fun resolveProfileName(): Result<String>

  /** Blocking `Profile.getCookieManager().flush()`, off the caller thread. */
  fun flushCookies(profileName: String, onDone: (Throwable?) -> Unit)

  /**
   * One-shot delayed callback used as the watchdog for the flush await —
   * the same bound [SiteClearFlow] and [SiteMigrationFlow] put on their
   * awaited steps. A flush that never calls back must not hang the close.
   *
   * Returns [Watchdog.Armed] with the canceller (idempotent, callable from any
   * thread): the flow calls it the moment the step settles, so a step that
   * answered normally does not leave a delayed runnable queued — holding this
   * port, and through it the WebView and the Activity — for the rest of the
   * window.
   *
   * An implementation that CANNOT arm the watchdog must return
   * [Watchdog.Refused] and must NOT invoke [onTimeout] itself: the flow fails
   * the step and skips the provider work, because a call nobody can time out
   * would leave the caller — and the bridge call behind it — waiting forever.
   * `Handler.postDelayed` reports that case as a `false` return value, not as an
   * exception; [WatchdogArming] is the pure decision both forms go through.
   */
  fun scheduleTimeout(millis: Long, onTimeout: () -> Unit): Watchdog

  /** Remove + destroy the active view and re-bind PiP/focus to the launcher. */
  fun destroyActiveView()
}

/**
 * Normal close (`flush=true`): resolve profile → flush → only then destroy
 * and report success. **A flush or resolve failure keeps the view and the
 * session ownership** so the Rust `current_site` state and the Kotlin slot
 * stay identical, and the user can retry (or the back ladder falls back).
 *
 * The flush await is bounded by [FLUSH_TIMEOUT_MS]: the callback and the
 * watchdog race, exactly one of them wins, and a timeout answers the caller
 * with a failure while leaving everything untouched — no destroy, no
 * session release. A flush callback arriving AFTER the timeout is ignored:
 * it can neither flip the delivered failure back to success nor destroy the
 * view underneath the still-claimed session (the view may by then belong to
 * a retried close or to a newer open).
 *
 * Wipe close (`flush=false`, clear/delete paths): destroy without flushing —
 * deliberately a different branch, never a subset of the durable path.
 *
 * No active view → success (idempotent; a stale Rust session stays harmless).
 */
object SiteCloseFlow {
  /**
   * Bound for the flush await. Same magnitude as the migration flow's flush
   * watchdog: `CookieManager.flush()` is the one blocking provider call
   * behind a single-threaded executor, so every flow that waits on it gets
   * the same ceiling.
   */
  const val FLUSH_TIMEOUT_MS: Long = 15_000

  fun run(
    port: SiteClosePort,
    flush: Boolean,
    operationId: String = AnimeHubDebugLog.newOperationId(),
    onDone: (String?) -> Unit,
  ) {
    val safeOperationId = AnimeHubDebugLog.safeOperationId(operationId) ?: AnimeHubDebugLog.newOperationId()
    val flowStartedAt = System.nanoTime()
    val once = AtomicBoolean(false)
    fun done(error: String?, errorClass: String? = null) {
      if (once.compareAndSet(false, true)) {
        AnimeHubDebugLog.record(
          "close", "complete", if (error == null) "ok" else "failed", safeOperationId,
          durationMs = ((System.nanoTime() - flowStartedAt) / 1_000_000).coerceAtLeast(0),
          errorClass = if (error == null) null else errorClass ?: "provider",
        )
        onDone(error)
      }
    }
    AnimeHubDebugLog.record("close", "start", "started", safeOperationId)
    if (!port.hasActiveView()) {
      AnimeHubDebugLog.record("close", "profile_check", "absent", safeOperationId)
      AnimeHubDebugLog.record("close", "destroy", "not_needed", safeOperationId)
      done(null)
      return
    }
    AnimeHubDebugLog.record("close", "profile_check", "present", safeOperationId)
    if (!flush) {
      AnimeHubDebugLog.record("close", "destroy", "started", safeOperationId)
      port.destroyActiveView()
      AnimeHubDebugLog.record("close", "destroy", "closed", safeOperationId)
      done(null)
      return
    }
    AnimeHubDebugLog.record("close", "resolve", "started", safeOperationId)
    val profileName =
      port.resolveProfileName().getOrElse {
        // Keep the view: durability is unverified, so the session stays claimed.
        AnimeHubDebugLog.record("close", "resolve", "failed", safeOperationId, errorClass = "provider")
        done("profil çözümlenemedi: provider", "provider")
        return
      }
    AnimeHubDebugLog.record("close", "resolve", "ok", safeOperationId)

    // Watchdog + callback race, settled exactly once. Timeout = failure with
    // the view AND the session ownership untouched (retryable); a late flush
    // callback cannot destroy a view whose ownership was never released.
    val settled = AtomicBoolean(false)
    val stepStartedAt = System.nanoTime()
    val stepId = StepGuards.create().operationId
    AnimeHubDebugLog.record("close", "flush", "started", safeOperationId, stepId = stepId)
    val watchdog = port.scheduleTimeout(FLUSH_TIMEOUT_MS) {
      if (settled.compareAndSet(false, true)) {
        AnimeHubDebugLog.record(
          "close", "flush", "timeout", safeOperationId,
          durationMs = ((System.nanoTime() - stepStartedAt) / 1_000_000).coerceAtLeast(0),
          watchdog = "fired", errorClass = "timeout", stepId = stepId,
        )
        done("çerez kalıcılığı zaman aşımına uğradı: görünüm açık kaldı", "timeout")
      }
    }
    if (watchdog is Watchdog.Refused) {
      // No watchdog ⇒ no bound on the blocking flush, so never start it.
      if (settled.compareAndSet(false, true)) {
        AnimeHubDebugLog.record(
          "close", "flush", "failed", safeOperationId,
          durationMs = ((System.nanoTime() - stepStartedAt) / 1_000_000).coerceAtLeast(0),
          watchdog = "refused", errorClass = "watchdog_refused", stepId = stepId,
        )
        done("çerez kalıcılığı ${SiteProfileErrors.WATCHDOG_REFUSED}: görünüm açık kaldı", "watchdog_refused")
      }
      return
    }
    AnimeHubDebugLog.record("close", "watchdog_arm", "armed", safeOperationId, stepId = stepId)
    val cancelWatchdog = (watchdog as Watchdog.Armed).cancel
    port.flushCookies(profileName) { error ->
      if (!settled.compareAndSet(false, true)) {
        AnimeHubDebugLog.record(
          "close", "late_callback", "late", safeOperationId,
          watchdog = "late", errorClass = "timeout", stepId = stepId,
        )
        return@flushCookies
      }
      // The flush answered: drop the queued watchdog so it cannot keep this
      // port (and the view/Activity behind it) reachable for another 15 s.
      cancelWatchdog()
      if (error != null) {
        AnimeHubDebugLog.record(
          "close", "flush", "failed", safeOperationId,
          durationMs = ((System.nanoTime() - stepStartedAt) / 1_000_000).coerceAtLeast(0),
          errorClass = "provider", stepId = stepId,
        )
        done("çerez kalıcılığı başarısız: provider", "provider")
        return@flushCookies
      }
      AnimeHubDebugLog.record(
        "close", "flush", "ok", safeOperationId,
        durationMs = ((System.nanoTime() - stepStartedAt) / 1_000_000).coerceAtLeast(0),
        stepId = stepId,
      )
      AnimeHubDebugLog.record("close", "destroy", "started", safeOperationId)
      port.destroyActiveView()
      AnimeHubDebugLog.record("close", "destroy", "closed", safeOperationId)
      done(null)
    }
  }
}
