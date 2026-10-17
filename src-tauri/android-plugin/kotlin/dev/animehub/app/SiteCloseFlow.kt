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

  fun run(port: SiteClosePort, flush: Boolean, onDone: (String?) -> Unit) {
    val once = AtomicBoolean(false)
    fun done(error: String?) {
      if (once.compareAndSet(false, true)) onDone(error)
    }
    if (!port.hasActiveView()) {
      done(null)
      return
    }
    if (!flush) {
      port.destroyActiveView()
      done(null)
      return
    }
    val profileName =
      port.resolveProfileName().getOrElse { e ->
        // Keep the view: durability is unverified, so the session stays claimed.
        done("profil çözümlenemedi: ${e.message}")
        return
      }
    // Watchdog + callback race, settled exactly once. Timeout = failure with
    // the view AND the session ownership untouched (retryable); a late flush
    // callback loses the race and is ignored — it must not destroy a view
    // whose ownership was never released.
    val settled = AtomicBoolean(false)
    val watchdog = port.scheduleTimeout(FLUSH_TIMEOUT_MS) {
      if (settled.compareAndSet(false, true)) {
        done("çerez kalıcılığı zaman aşımına uğradı: görünüm açık kaldı")
      }
    }
    if (watchdog is Watchdog.Refused) {
      // No watchdog ⇒ no bound on the blocking flush, so the flush is never
      // started. Reported like a timeout (failure, retryable) and with the same
      // conservative state: the view AND the session ownership stay, because
      // durability was never verified.
      if (settled.compareAndSet(false, true)) {
        done("çerez kalıcılığı ${SiteProfileErrors.WATCHDOG_REFUSED}: görünüm açık kaldı")
      }
      return
    }
    val cancelWatchdog = (watchdog as Watchdog.Armed).cancel
    port.flushCookies(profileName) { error ->
      if (!settled.compareAndSet(false, true)) return@flushCookies
      // The flush answered: drop the queued watchdog so it cannot keep this
      // port (and the view/Activity behind it) reachable for another 15 s.
      cancelWatchdog()
      if (error != null) {
        done("çerez kalıcılığı başarısız: ${error.message}")
        return@flushCookies
      }
      port.destroyActiveView()
      done(null)
    }
  }
}
