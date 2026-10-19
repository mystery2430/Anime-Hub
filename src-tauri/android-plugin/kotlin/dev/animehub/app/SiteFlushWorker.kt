// SPDX-License-Identifier: MIT
//
// The single blocking-flush worker, and the UI-thread watchdog arming every
// site flow shares. It is deliberately process-local and never writes logs.

package dev.animehub.app

import android.app.Activity
import android.os.Handler
import android.os.Looper
import android.util.Log
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicLong

/**
 * One serialised worker for every blocking flush, plus UI-thread watchdog
 * arming. All site close/clear/migration/sync and destroy-time flushes use it.
 */
object SiteFlushWorker {
  private const val LOG_TAG = "AnimeHubSite"
  private val FLUSH_EXECUTOR = Executors.newSingleThreadExecutor { runnable ->
    Thread(runnable, "AnimeHubSiteFlush").apply { isDaemon = true }
  }
  private val flushBusySince = AtomicLong(0L)
  private const val FLUSH_STUCK_WARN_MS = 10_000L
  private val mainHandler = Handler(Looper.getMainLooper())

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
    warnIfWorkerStuck(tag, operationId)
    FLUSH_EXECUTOR.execute {
      val startedAt = System.nanoTime()
      flushBusySince.compareAndSet(0L, System.currentTimeMillis())
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
        flushBusySince.set(0L)
      }
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

  private fun warnIfWorkerStuck(tag: String, operationId: String) {
    val since = flushBusySince.get()
    if (since > 0L) {
      val waited = System.currentTimeMillis() - since
      if (waited >= FLUSH_STUCK_WARN_MS) {
        Log.w(LOG_TAG, "flush worker remains busy")
        AnimeHubDebugLog.record(
          "flush", "worker_wait", "stuck", operationId,
          durationMs = waited, watchdog = "fired", errorClass = "timeout",
        )
      }
    }
  }
}
