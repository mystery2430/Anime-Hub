// SPDX-License-Identifier: MIT
//
// Shutdown flow: the destroy-time flush of a site profile, awaited within an
// explicit budget before the generated Activity may kill the process.
//
// Pure decision/sequencing code: this file must NOT import `android.*` or any
// Tauri class, so the CI unit-test source set compiles and runs it against the
// generated Android project (see SiteProfileOps.kt for the full rationale and
// the one-thread-per-flow rule). Split out of SiteProfileOps.kt so each
// responsibility — and what may change it — is visible at a glance.

package dev.animehub.app

// ---------------------------------------------------------------------------
// Shutdown: the destroy-time flush, awaited within an explicit budget
// ---------------------------------------------------------------------------

/** What the destroy-time flush of a site profile actually achieved. */
sealed interface ShutdownFlush {
  /** No site profile was active: there was nothing to persist. */
  object NotNeeded : ShutdownFlush

  /** The background worker finished the flush inside the budget. */
  object Completed : ShutdownFlush

  /** The worker failed; [reason] is a safe class code, never exception text. */
  data class Failed(val reason: String) : ShutdownFlush

  /**
   * The budget ran out with the flush still in flight. Cookie durability is
   * NOT guaranteed — the process can be killed before the worker lands.
   */
  object TimedOut : ShutdownFlush
}

/**
 * What the destroy path contributes to [ShutdownFlow]. Split out so the
 * sequencing is executable in CI unit tests rather than only readable on a
 * device: the real port needs an Activity, a live WebView and the provider's
 * cookie manager.
 */
interface ShutdownPort {
  /**
   * Hand the ACTIVE site profile's cookie flush to the background worker and
   * report whether there was one to hand over (`false` = no site profile).
   * MUST NOT run the flush on the calling thread: `flush()` is disk I/O and
   * this is the UI thread.
   */
  fun submitFlush(): Boolean

  /** Detach and destroy the site WebView. Runs even when nothing was submitted. */
  fun teardownView()

  /** Wait at most [budgetMillis] for the submitted flush; `true` if it finished. */
  fun awaitFlush(budgetMillis: Long): Boolean

  /** Safe failure code (`provider`), or `null`; provider text is never returned. */
  fun flushFailure(): String?
}

/**
 * Destroy-time shutdown protocol for the site WebView.
 *
 * WHY IT EXISTS: the generated `MainActivity.onDestroy` ends the process when
 * the Activity is finishing, and the site profile's `flush()` used to be
 * *scheduled* on the background worker and never awaited — so the kill could
 * land while the write was still queued, losing that site's cookies. The
 * default-profile `CookieManager.getInstance().flush()` the generated Activity
 * performs afterwards proves nothing about the SITE profile's store, so it
 * cannot stand in for this either.
 *
 * THE ORDER, and why it is not simply "block until done":
 *  1. submit the flush to the worker — the disk I/O never runs on the UI
 *     thread;
 *  2. tear the view down — quick, and it overlaps the flush instead of
 *     serialising behind it;
 *  3. await the flush for a bounded budget ([DESTROY_FLUSH_BUDGET_MS]).
 * A bounded wait is the honest compromise: after `onDestroy` the process may die
 * at any moment, so waiting forever would trade a cookie loss for an ANR, and
 * not waiting at all loses the cookies. Whatever the outcome it is REPORTED —
 * [ShutdownFlush.TimedOut] and [ShutdownFlush.Failed] are never presented as
 * durability.
 *
 * OWNERSHIP: each call supplies its own completion state, and the session slot
 * is cleared before the protocol runs, so a second (double) teardown submits
 * nothing and reports [ShutdownFlush.NotNeeded]; a flush belonging to an older
 * session cannot be counted for a newer one.
 */
object ShutdownFlow {
  /**
   * Teardown budget for the destroy-time flush. Deliberately far below
   * [SiteCloseFlow.FLUSH_TIMEOUT_MS] (15 s): this wait happens on the UI thread
   * while the Activity is being destroyed, so it must stay well inside ANR
   * territory while still giving a real flush time to land.
   */
  const val DESTROY_FLUSH_BUDGET_MS: Long = 1_000

  private fun safeFailureClass(value: String?): String? =
    when (value) {
      null -> null
      "provider" -> "provider"
      else -> "unknown"
    }

  fun run(
    port: ShutdownPort,
    budgetMillis: Long = DESTROY_FLUSH_BUDGET_MS,
    operationId: String = AnimeHubDebugLog.newOperationId(),
  ): ShutdownFlush {
    require(budgetMillis > 0) { "the destroy-time flush budget must be positive" }
    val safeOperationId = AnimeHubDebugLog.safeOperationId(operationId) ?: AnimeHubDebugLog.newOperationId()
    val startedAt = System.nanoTime()
    AnimeHubDebugLog.record("shutdown", "destroy", "started", safeOperationId)
    AnimeHubDebugLog.record("shutdown", "worker_start", "started", safeOperationId)
    val submitted = port.submitFlush()
    AnimeHubDebugLog.record(
      "shutdown", "worker_start", if (submitted) "accepted" else "skipped", safeOperationId,
    )
    // Teardown runs whatever happened: a site view must never survive destroy
    // because a flush was skipped, slow or failed.
    AnimeHubDebugLog.record("shutdown", "destroy", "started", safeOperationId)
    port.teardownView()
    AnimeHubDebugLog.record("shutdown", "destroy", "closed", safeOperationId)
    if (!submitted) {
      val failure = safeFailureClass(port.flushFailure())
      val outcome = failure?.let { ShutdownFlush.Failed(it) } ?: ShutdownFlush.NotNeeded
      val result = if (outcome == ShutdownFlush.NotNeeded) "not_needed" else "failed"
      AnimeHubDebugLog.record(
        "shutdown", "destroy_flush", result, safeOperationId,
        durationMs = ((System.nanoTime() - startedAt) / 1_000_000).coerceAtLeast(0),
        errorClass = failure,
      )
      AnimeHubDebugLog.record(
        "shutdown", "complete", if (failure == null) "ok" else "failed", safeOperationId,
        durationMs = ((System.nanoTime() - startedAt) / 1_000_000).coerceAtLeast(0),
        errorClass = failure,
      )
      return outcome
    }
    AnimeHubDebugLog.record("shutdown", "destroy_flush", "started", safeOperationId)
    val outcome =
      if (!port.awaitFlush(budgetMillis)) {
        ShutdownFlush.TimedOut
      } else {
        safeFailureClass(port.flushFailure())?.let { ShutdownFlush.Failed(it) }
          ?: ShutdownFlush.Completed
      }
    val duration = ((System.nanoTime() - startedAt) / 1_000_000).coerceAtLeast(0)
    when (outcome) {
      ShutdownFlush.Completed ->
        AnimeHubDebugLog.record(
          "shutdown", "destroy_flush", "ok", safeOperationId, durationMs = duration,
        )
      ShutdownFlush.NotNeeded ->
        AnimeHubDebugLog.record("shutdown", "destroy_flush", "not_needed", safeOperationId, durationMs = duration)
      ShutdownFlush.TimedOut ->
        AnimeHubDebugLog.record(
          "shutdown", "destroy_flush", "timeout", safeOperationId,
          durationMs = duration, watchdog = "fired", errorClass = "timeout",
        )
      is ShutdownFlush.Failed ->
        AnimeHubDebugLog.record(
          "shutdown", "destroy_flush", "failed", safeOperationId,
          durationMs = duration,
          errorClass = if (outcome.reason == "provider") "provider" else "unknown",
        )
    }
    AnimeHubDebugLog.record(
      "shutdown", "complete", if (outcome is ShutdownFlush.Completed) "ok" else "failed",
      safeOperationId, durationMs = duration,
      errorClass = when (outcome) {
        ShutdownFlush.TimedOut -> "timeout"
        is ShutdownFlush.Failed -> if (outcome.reason == "provider") "provider" else "unknown"
        else -> null
      },
    )
    return outcome
  }
}
