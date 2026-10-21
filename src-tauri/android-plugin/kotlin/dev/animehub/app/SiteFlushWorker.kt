// SPDX-License-Identifier: MIT
//
// The single blocking-flush worker, and the UI-thread watchdog arming every
// site flow shares. It is deliberately process-local and never writes logs.
//
// RECOVERY: one unresponsive provider `flush()` used to wedge the worker
// forever; every later close/clear/migration queued behind it and only a log
// line said so. The worker keeps a generation and, after a measured stall,
// the next submission swaps in a fresh single-thread executor. The stuck
// daemon thread is abandoned and its queue discarded; callers all have a
// watchdog or a bounded await, so a discarded task settles as a failure.

package dev.animehub.app

import android.app.Activity
import android.os.Handler
import android.os.Looper
import android.util.Log
import java.util.concurrent.ExecutorService
import java.util.concurrent.Executors

/**
 * One serialised worker for every blocking flush, plus UI-thread watchdog
 * arming. All site close/clear/migration/sync and destroy-time flushes use it.
 */
object SiteFlushWorker {
  private const val LOG_TAG = "AnimeHubSite"
  // Literals pinned by a source test; FlushRecovery owns the decision.
  private const val FLUSH_STUCK_WARN_MS = 10_000L
  private const val FLUSH_STUCK_REPLACE_MS = 30_000L
  private val mainHandler = Handler(Looper.getMainLooper())

  /** Guards [flushExecutor], its generation and the busy bookkeeping below. */
  private val stateLock = Any()
  private var flushExecutor: ExecutorService = newFlushExecutor()
  private var executorGeneration = 0L
  private var busySince = 0L
  private var busyGeneration = -1L

  /** The ONLY place a flush thread is created (startup or stall recovery). */
  private fun newFlushExecutor(): ExecutorService =
    Executors.newSingleThreadExecutor { runnable ->
      Thread(runnable, "AnimeHubSiteFlush").apply { isDaemon = true }
    }

  /**
   * A refusal never starts the provider operation: the owning flow records the
   * correlated refusal and fails its step rather than hanging without a timer.
   */
  fun scheduleWatchdog(millis: Long, onTimeout: () -> Unit): Watchdog {
    val task = Runnable { onTimeout() }
    var queued = false
    var thrown: Throwable? = null
    try {
      queued = mainHandler.postDelayed(task, millis)
    } catch (e: Exception) {
      thrown = e
    }
    val refusal = WatchdogArming.refusal(queued, thrown)
    if (refusal != null) {
      Log.w(LOG_TAG, "watchdog could not be armed; provider work will not start")
      return Watchdog.Refused
    }
    return Watchdog.Armed({ mainHandler.removeCallbacks(task) })
  }

  /** Queue work on the single blocking worker; no provider text is logged. */
  fun execute(
    tag: String,
    operationId: String,
    recordWorkerStart: Boolean = true,
    recordSuccess: Boolean = true,
    work: () -> Unit,
  ) {
    warnOrRecover(operationId)
    val target: ExecutorService
    val generation: Long
    synchronized(stateLock) {
      target = flushExecutor
      generation = executorGeneration
    }
    try {
      target.execute {
        val startedAt = System.nanoTime()
        synchronized(stateLock) {
          if (executorGeneration == generation) {
            busySince = System.currentTimeMillis()
            busyGeneration = generation
          }
        }
        if (recordWorkerStart) {
          AnimeHubDebugLog.record("flush", "worker_wait", "started", operationId)
        }
        try {
          work()
          if (recordSuccess) {
            AnimeHubDebugLog.record(
              "flush", tag, "ok", operationId,
              durationMs = (System.nanoTime() - startedAt) / 1_000_000,
            )
          }
        } catch (_: Exception) {
          AnimeHubDebugLog.record(
            "flush", tag, "failed", operationId,
            durationMs = (System.nanoTime() - startedAt) / 1_000_000,
            errorClass = "provider",
          )
        } finally {
          synchronized(stateLock) {
            if (busyGeneration == generation) {
              busySince = 0L
              busyGeneration = -1L
            }
          }
        }
      }
    } catch (_: Exception) {
      // Rejected after a concurrent replacement. The caller still owns a
      // watchdog/bounded await, so report a failure instead of hanging.
      AnimeHubDebugLog.record("flush", tag, "failed", operationId, errorClass = "provider")
    }
  }

  /** Run a blocking flush and deliver its result on the UI thread. */
  fun submit(
    activity: Activity,
    tag: String,
    work: () -> Unit,
    operationId: String,
    onDone: (Throwable?) -> Unit,
  ) {
    val startedAt = System.nanoTime()
    val routineSync = tag == "sync"
    try {
      execute(
        tag,
        operationId,
        recordWorkerStart = !routineSync,
        recordSuccess = false,
      ) {
        val error =
          try {
            work()
            null
          } catch (e: Exception) {
            e
          }
        if (error != null || !routineSync) {
          AnimeHubDebugLog.record(
            "flush", "flush", if (error == null) "ok" else "failed", operationId,
            durationMs = (System.nanoTime() - startedAt) / 1_000_000,
            errorClass = if (error == null) null else "provider",
          )
        }
        activity.runOnUiThread { onDone(error) }
      }
    } catch (_: Exception) {
      AnimeHubDebugLog.record("flush", "flush", "failed", operationId, errorClass = "provider")
      activity.runOnUiThread { onDone(IllegalStateException("flush worker unavailable")) }
    }
  }

  /**
   * Warn on the first measured stall and replace the worker after the hard
   * threshold. Replacement is lazy (next submission), so an idle app never
   * pays for a thread it does not need.
   */
  private fun warnOrRecover(operationId: String) {
    var replacement: ExecutorService? = null
    var stuckFor = 0L
    synchronized(stateLock) {
      val now = System.currentTimeMillis()
      val since = busySince
      stuckFor = FlushRecovery.stuckFor(since, now)
      if (FlushRecovery.shouldReplace(since, now)) {
        replacement = flushExecutor
        executorGeneration += 1
        flushExecutor = newFlushExecutor()
        busySince = 0L
        busyGeneration = -1L
      }
    }
    val old = replacement
    if (old != null) {
      old.shutdownNow()
      Log.w(LOG_TAG, "flush worker replaced after a stuck provider call")
      AnimeHubDebugLog.record(
        "flush", "worker_wait", "recovered", operationId,
        durationMs = stuckFor, watchdog = "fired", errorClass = "timeout",
      )
      return
    }
    if (stuckFor >= FLUSH_STUCK_WARN_MS) {
      Log.w(LOG_TAG, "flush worker remains busy")
      AnimeHubDebugLog.record(
        "flush", "worker_wait", "stuck", operationId,
        durationMs = stuckFor, watchdog = "fired", errorClass = "timeout",
      )
    }
  }
}
