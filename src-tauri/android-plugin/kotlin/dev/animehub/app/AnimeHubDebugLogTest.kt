// SPDX-License-Identifier: MIT
//
// Behavioural regression tests for the bounded, redacted Kotlin event ring.

package dev.animehub.app

import org.junit.Assert
import org.junit.Before
import org.junit.Test

class AnimeHubDebugLogTest {
  @Before
  fun resetLog() {
    AnimeHubDebugLog.resetForTests()
  }

  @Test
  fun `collection is opt-in and disabling clears the in-memory ring`() {
    AnimeHubDebugLog.record("open", "start", "started", "k-1")
    Assert.assertTrue(AnimeHubDebugLog.snapshot().events.isEmpty())

    AnimeHubDebugLog.setEnabled(true)
    AnimeHubDebugLog.record("open", "profile_check", "present", "k-1")
    Assert.assertTrue(AnimeHubDebugLog.snapshot().events.isNotEmpty())

    AnimeHubDebugLog.setEnabled(false)
    val disabled = AnimeHubDebugLog.snapshot()
    Assert.assertTrue(disabled.events.isEmpty())
    Assert.assertEquals(0L, disabled.droppedEvents)
    Assert.assertEquals(AnimeHubDebugLog.MAX_EVENTS, disabled.capacity)
  }

  @Test
  fun `watchdog and retirement outcomes survive the closed result allowlist`() {
    AnimeHubDebugLog.setEnabled(true)
    AnimeHubDebugLog.record("clear", "watchdog_arm", "armed", "k-12")
    AnimeHubDebugLog.record(
      "delete", "quarantine_retire", "refused", "k-12", errorClass = "active_site",
    )

    val events = AnimeHubDebugLog.snapshot().events.drop(1)
    Assert.assertEquals(listOf("armed", "refused"), events.map { it.result })
  }

  @Test
  fun `events use monotone sequence and timestamp and preserve operation ids`() {
    AnimeHubDebugLog.setEnabled(true)
    val operationId = AnimeHubDebugLog.newOperationId()
    AnimeHubDebugLog.record("open", "profile_check", "started", operationId)
    AnimeHubDebugLog.record(
      "open",
      "profile_bind",
      "ok",
      operationId,
      durationMs = 12,
      stepId = 4,
    )
    val events = AnimeHubDebugLog.snapshot().events

    Assert.assertTrue(events.size >= 3) // includes the gate-enabled marker
    Assert.assertTrue(events.zipWithNext().all { (a, b) -> a.sequence < b.sequence })
    Assert.assertTrue(events.zipWithNext().all { (a, b) -> a.timestampMs <= b.timestampMs })
    Assert.assertTrue(events.drop(1).all { it.operationId == operationId })
    Assert.assertEquals(12L, events.last().durationMs)
    Assert.assertEquals(4L, events.last().stepId)
  }

  @Test
  fun `overflow is bounded and counted while reset leaves a safe marker`() {
    AnimeHubDebugLog.setEnabled(true)
    repeat(AnimeHubDebugLog.MAX_EVENTS + 9) { index ->
      AnimeHubDebugLog.record("profile", "profile_check", "present", "k-1", count = index.toLong())
    }
    val full = AnimeHubDebugLog.snapshot()
    Assert.assertEquals(AnimeHubDebugLog.MAX_EVENTS, full.events.size)
    Assert.assertEquals(10L, full.droppedEvents) // gate marker plus 2,009 records
    Assert.assertEquals(AnimeHubDebugLog.MAX_EVENTS, full.capacity)
    Assert.assertTrue(full.events.first().sequence > 1)

    AnimeHubDebugLog.reset()
    val reset = AnimeHubDebugLog.snapshot()
    Assert.assertEquals(1, reset.events.size)
    Assert.assertEquals("devlog", reset.events.single().area)
    Assert.assertEquals("reset", reset.events.single().phase)
    Assert.assertEquals("reset", reset.events.single().result)
    Assert.assertEquals(0L, reset.droppedEvents)
  }

  @Test
  fun `unsafe text and ids are replaced by closed codes before they enter memory`() {
    AnimeHubDebugLog.setEnabled(true)
    val privateText = "https://private.example/path?token=TOP-SECRET; Cookie: secret"
    AnimeHubDebugLog.record(
      area = privateText,
      phase = privateText,
      result = privateText,
      operationId = "site-secret-profile-name",
      watchdog = privateText,
      errorClass = privateText,
      durationMs = -1,
      count = -1,
      stepId = -1,
    )
    val json = AnimeHubDebugLog.snapshot().toString()

    Assert.assertFalse(json.contains("private.example"))
    Assert.assertFalse(json.contains("TOP-SECRET"))
    Assert.assertFalse(json.contains("Cookie"))
    Assert.assertFalse(json.contains("site-secret"))
    val event = AnimeHubDebugLog.snapshot().events.last()
    Assert.assertEquals("site", event.area)
    Assert.assertEquals("complete", event.phase)
    Assert.assertEquals("unknown", event.result)
    Assert.assertNull(event.operationId)
    Assert.assertEquals("unknown", event.watchdog)
    Assert.assertEquals("unknown", event.errorClass)
    Assert.assertNull(event.durationMs)
    Assert.assertNull(event.count)
    Assert.assertNull(event.stepId)
  }
}
