// SPDX-License-Identifier: MIT
//
// Bounded, in-memory structured diagnostics shared by the Android plugin,
// site-profile flows and PiP controller. No file, network or page payloads.

package dev.animehub.app

import java.util.concurrent.atomic.AtomicLong

/** One structured event. The Rust bridge merges both source rings by time. */
data class DebugEvent(
  val sequence: Long,
  val timestampMs: Long,
  val source: String,
  val area: String,
  val phase: String,
  val result: String,
  val operationId: String? = null,
  val durationMs: Long? = null,
  val watchdog: String? = null,
  val errorClass: String? = null,
  val count: Long? = null,
  val stepId: Long? = null,
)

data class DebugLogSnapshot(
  val events: List<DebugEvent>,
  val droppedEvents: Long,
  val capacity: Int,
)

/**
 * Process-local diagnostics for the developer panel. Every recorded text
 * field is a code from a closed allowlist; call sites cannot accidentally
 * pass an exception message, URL, origin, profile name, cookie or page text.
 */
object AnimeHubDebugLog {
  const val MAX_EVENTS: Int = 2_000

  private val events = ArrayDeque<DebugEvent>()
  private val operationCounter = AtomicLong(0)
  private var nextSequence = 1L
  private var lastTimestampMs = 0L
  private var droppedEvents = 0L
  private var enabled = false

  private val operationPattern = Regex("^[rk]-[1-9][0-9]{0,19}$")
  private val areas = setOf(
    "app", "back", "bridge", "clear", "close", "delete", "devlog", "flush", "ipc",
    "migration", "open", "pip", "profile", "queue", "shutdown", "site", "startup", "webview",
  )
  private val phases = setOf(
    "back_pressed", "back_step", "capability", "clear", "close", "complete", "cookie_verify",
    "cookie_wipe", "cookie_write", "destroy", "destroy_flush", "drain", "enumerate",
    "flag_write", "flush", "fullscreen", "gate", "generation", "geolocation",
    "geolocation_origins", "geolocation_wipe", "ipc_dispatch", "ipc_setup", "late_callback",
    "launcher_close", "migration", "native_close", "native_fallback", "navigation_start",
    "open", "orphan_sweep", "pip_enter", "pip_playback", "pip_prepare", "pip_restore",
    "pip_transfer", "poll", "profile_bind", "profile_check", "profile_create", "profile_delete",
    "profile_flush", "profile_list", "provider_delete", "quarantine", "quarantine_retire",
    "queue_remove", "reset", "resolve", "retire", "script", "slot_claim", "start", "storage",
    "sync", "tombstone", "transfer_to_launcher", "view_attach", "view_create", "web_history",
    "verified", "watchdog", "watchdog_arm", "worker_start", "worker_wait",
  )
  private val results = setOf(
    "accepted", "absent", "armed", "attached", "available", "blocked", "closed", "copied", "deferred",
    "deleted", "disabled", "duplicate", "empty", "enabled", "entered", "error", "failed",
    "failed_closed", "fallback", "flow_failed", "handled", "intercepted", "late", "lifted",
    "not_entered", "not_needed", "not_restored", "not_empty", "ok", "owed", "paused", "pending",
    "playing", "present", "passed", "quarantined", "queued", "rejected", "refused", "reset", "restored",
    "retired", "settled", "skipped", "started", "stuck", "timeout", "unknown", "unsupported",
    "unsupported_provider", "unavailable", "verified", "watchdog_refused", "supported", "invalid",
  )
  private val watchdogStates = setOf("armed", "cancelled", "fired", "late", "refused", "unknown")
  private val errorClasses = setOf(
    "active_site", "activity_gone", "cookie_unverified", "in_use", "ipc", "invalid_profile",
    "migration_partial", "permission", "provider", "quarantined", "storage", "timeout",
    "unknown", "unavailable", "unsupported", "watchdog_refused",
  )

  /** Enable/disable collection. Disabling also discards the in-memory ring. */
  @Synchronized
  fun setEnabled(value: Boolean) {
    if (enabled == value) return
    enabled = value
    if (!value) {
      events.clear()
      droppedEvents = 0
      return
    }
    append(
      area = "devlog",
      phase = "gate",
      result = "enabled",
      operationId = null,
    )
  }

  /** A safe local id used for events that begin on the Kotlin lifecycle side. */
  fun newOperationId(): String = "k-${operationCounter.incrementAndGet()}"

  /** Reject any non-generated value before it can enter a diagnostic event. */
  fun safeOperationId(value: String?): String? =
    value?.takeIf { operationPattern.matches(it) }

  /** Record one event; only closed-vocabulary values reach the buffer. */
  @Synchronized
  fun record(
    area: String,
    phase: String,
    result: String,
    operationId: String? = null,
    durationMs: Long? = null,
    watchdog: String? = null,
    errorClass: String? = null,
    count: Long? = null,
    stepId: Long? = null,
  ) {
    if (!enabled) return
    append(
      area = area,
      phase = phase,
      result = result,
      operationId = safeOperationId(operationId),
      durationMs = durationMs?.coerceAtLeast(0),
      watchdog = watchdog?.let { if (it in watchdogStates) it else "unknown" },
      errorClass = errorClass?.let { if (it in errorClasses) it else "unknown" },
      count = count?.coerceAtLeast(0),
      stepId = stepId?.takeIf { it >= 0 },
    )
  }

  /** Explicit user-triggered reset. The reset itself remains visible. */
  @Synchronized
  fun reset() {
    if (!enabled) return
    events.clear()
    droppedEvents = 0
    append("devlog", "reset", "reset", null)
  }

  @Synchronized
  fun snapshot(): DebugLogSnapshot =
    DebugLogSnapshot(events.toList(), droppedEvents, MAX_EVENTS)

  @Synchronized
  internal fun resetForTests() {
    events.clear()
    droppedEvents = 0
    nextSequence = 1L
    lastTimestampMs = 0L
    enabled = false
    operationCounter.set(0L)
  }

  private fun append(
    area: String,
    phase: String,
    result: String,
    operationId: String?,
    durationMs: Long? = null,
    watchdog: String? = null,
    errorClass: String? = null,
    count: Long? = null,
    stepId: Long? = null,
  ) {
    val timestamp = maxOf(System.currentTimeMillis(), lastTimestampMs)
    lastTimestampMs = timestamp
    if (events.size == MAX_EVENTS) {
      events.removeFirst()
      droppedEvents++
    }
    events.addLast(
      DebugEvent(
        sequence = nextSequence++,
        timestampMs = timestamp,
        source = "kotlin",
        area = if (area in areas) area else "site",
        phase = if (phase in phases) phase else "complete",
        result = if (result in results) result else "unknown",
        operationId = safeOperationId(operationId),
        durationMs = durationMs,
        watchdog = watchdog,
        errorClass = errorClass,
        count = count,
        stepId = stepId,
      ),
    )
  }
}
