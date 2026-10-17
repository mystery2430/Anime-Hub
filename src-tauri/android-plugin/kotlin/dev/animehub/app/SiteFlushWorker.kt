// SPDX-License-Identifier: MIT
//
// The single blocking-flush worker, and the UI-thread watchdog arming every
// site flow shares.
//
// Split out of AnimeHubSiteController.kt because both are INVARIANTS, not
// details of one call site:
//   * every blocking `CookieManager.flush()` runs on ONE serialised worker, and
//     a stuck worker is made observable instead of being papered over with more
//     threads;
//   * a watchdog is armed once, cancelled the moment its step settles, and — if
//     it cannot be armed at all — fails the step immediately rather than leaving
//     the caller (and the Tauri bridge call behind it) waiting forever.
//
// This file is the platform boundary: it touches `android.os.Handler`/`Looper`
// and `Activity`, which is exactly what the pure flow files must not do.

package dev.animehub.app

import android.app.Activity
import android.os.Handler
import android.os.Looper
import android.util.Log
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicLong

/**
 * One serialised worker for every blocking flush, plus UI-thread watchdog
 * arming. All of `site_close`, `site_clear_data`, `site_migrate`, the periodic
 * durability sync and the destroy-time teardown flush go through here.
 */
object SiteFlushWorker {
  /** Same tag as the controller, so one logcat filter shows the whole site path. */
  private const val LOG_TAG = "AnimeHubSite"

  private val FLUSH_EXECUTOR = Executors.newSingleThreadExecutor { runnable ->
    Thread(runnable, "AnimeHubSiteFlush").apply { isDaemon = true }
  }

  /**
   * When the flush task currently running on [FLUSH_EXECUTOR] started, or 0
   * while the worker is idle. Single-threaded on purpose: `CookieManager
   * .flush()` is the blocking provider call, and serialising it keeps the
   * profile cookie stores from being written concurrently.
   */
  private val flushBusySince = AtomicLong(0L)

  /**
   * How long a running flush may take before a newly queued one is logged as
   * evidence of a stuck worker.
   *
   * A flow watchdog answers the CALLER — it never cancels the blocking
   * `flush()` underneath, because the provider call offers no cancellation.
   * So a genuinely stuck flush leaves its task occupying the worker; later
   * flushes queue behind it and every later await is answered by its own
   * watchdog. This counter makes that pileup observable in the in-app log
   * instead of invisible (no unbounded thread spawning to paper over it).
   */
  private const val FLUSH_STUCK_WARN_MS = 10_000L

  /** UI-thread scheduler for the watchdogs (the same looper the controller uses). */
  private val mainHandler = Handler(Looper.getMainLooper())

  /**
   * Arm one watchdog on the UI thread and return its canceller.
   *
   * The canceller is the point: a step that answered normally used to leave its
   * delayed runnable queued for the whole timeout window (10 s per step, 15 s
   * for a flush), and that runnable holds the port — and through it the site
   * WebView and the Activity — reachable until it fires. Every close, clear and
   * migration step paid that. `removeCallbacks` is idempotent and thread-safe,
   * so the flows cancel from whichever thread settles the step.
   *
   * If the watchdog cannot be armed — `postDelayed` returning `false` (its
   * documented, NON-throwing refusal) or an exception — this returns
   * [Watchdog.Refused] and does not invoke [onTimeout]: the flow fails the step
   * and skips the provider work, because a call nobody can time out would leave
   * the caller, and the Tauri bridge call behind it, waiting forever. Silently
   * ignoring the refusal (as this used to) traded a hang for an unbounded wait,
   * and answering the timeout here would have claimed a provider operation that
   * was never issued.
   */
  fun scheduleWatchdog(millis: Long, onTimeout: () -> Unit): Watchdog {
    val task = Runnable { onTimeout() }
    var queued = false
    var thrown: Throwable? = null
    try {
      // The API reports a refusal by RETURN VALUE: `false` means the runnable
      // could NOT be queued (typically a looper that is exiting) and nothing
      // throws — which is why the old try/catch alone never saw this case.
      queued = mainHandler.postDelayed(task, millis)
    } catch (e: Exception) {
      thrown = e
    }
    val refusal = WatchdogArming.refusal(queued, thrown)
    if (refusal != null) {
      // Honest, measurable, and NOT our call to settle: the flow fails the step
      // and skips the provider work (see [Watchdog.Refused]). Invoking
      // [onTimeout] here instead would report a timeout for a provider call
      // that was never issued — and, for the cookie step, quarantine an
      // operation that does not exist.
      Log.w(LOG_TAG, "$refusal; the step fails without starting its provider call")
      AnimeHubDebugLog.add("site", "watchdog refused - step failed without provider work")
      return Watchdog.Refused
    }
    return Watchdog.Armed({ mainHandler.removeCallbacks(task) })
  }

  /**
   * Queue [work] on the single flush worker, with the stuck-worker evidence
   * attached. No result delivery: the caller decides what completion means
   * (the destroy-time teardown counts its own latch, [submit] answers a flow).
   */
  fun execute(tag: String, work: () -> Unit) {
    warnIfWorkerStuck(tag)
    FLUSH_EXECUTOR.execute {
      flushBusySince.compareAndSet(0L, System.currentTimeMillis())
      try {
        work()
      } finally {
        flushBusySince.set(0L)
      }
    }
  }

  /**
   * Run one blocking `flush()` on the worker and deliver its result on the UI
   * thread. Warns (logcat + in-app log) when a previous flush has still been
   * running for [FLUSH_STUCK_WARN_MS] or more — the caller has its watchdog;
   * this is the evidence that the worker itself is stuck.
   */
  fun submit(
    activity: Activity,
    tag: String,
    work: () -> Unit,
    onDone: (Throwable?) -> Unit,
  ) {
    try {
      execute(tag) {
        val error =
          try {
            work()
            null
          } catch (e: Exception) {
            e
          }
        activity.runOnUiThread { onDone(error) }
      }
    } catch (e: Exception) {
      activity.runOnUiThread { onDone(e) }
    }
  }

  private fun warnIfWorkerStuck(tag: String) {
    val since = flushBusySince.get()
    if (since > 0L) {
      val waited = System.currentTimeMillis() - since
      if (waited >= FLUSH_STUCK_WARN_MS) {
        Log.w(LOG_TAG, "flush worker stuck ${waited}ms while queueing '$tag'")
        AnimeHubDebugLog.add("site", "flush worker stuck ${waited}ms ($tag)")
      }
    }
  }
}
