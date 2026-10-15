// SPDX-License-Identifier: MIT
//
// Behaviour tests for the per-site profile flows (SiteProfileOps).
//
// These do NOT scan source text: they drive the real sequencing logic with
// fake ports, covering the failure paths that used to be swallowed —
// close/flush durability, clear verification (including the awaited
// geolocation step and the final flush), the strict one-time cookie
// migration criterion, active-slot rollback after a failed open, and the
// launcher back-close status decisions.
//
// CI compiles this with the generated Gradle project (real JUnit4) and runs
// it in the "Run Kotlin policy unit test" step; `dev.animehub.app.*Test`
// covers it alongside SiteNavigationPolicyTest.

package dev.animehub.app

import org.junit.Assert
import org.junit.Test

class SiteProfileOpsTest {

  // ---------------------------------------------------------------- close

  private class FakeClosePort : SiteClosePort {
    var active = true
    var resolveError: Throwable? = null
    var flushError: Throwable? = null
    var destroyed = 0
    var resolveCalls = 0
    var flushCalls = 0
    val log = mutableListOf<String>()

    override fun hasActiveView(): Boolean = active

    override fun resolveProfileName(): Result<String> {
      resolveCalls++
      log += "resolve"
      val error = resolveError
      return if (error != null) Result.failure(error) else Result.success("site-x-g0")
    }

    override fun flushCookies(profileName: String, onDone: (Throwable?) -> Unit) {
      flushCalls++
      log += "flush:$profileName"
      onDone(flushError)
    }

    override fun destroyActiveView() {
      destroyed++
      active = false
      log += "destroy"
    }
  }

  @Test
  fun `close without an active view succeeds idempotently without touching the profile`() {
    val port = FakeClosePort().also { it.active = false }
    var result = "unset"
    SiteCloseFlow.run(port, flush = true) { result = it ?: "ok" }
    Assert.assertEquals("ok", result)
    Assert.assertEquals(0, port.resolveCalls)
    Assert.assertEquals(0, port.flushCalls)
    Assert.assertEquals(0, port.destroyed)
  }

  @Test
  fun `wipe close flush=false destroys without any flush`() {
    val port = FakeClosePort()
    var result = "unset"
    SiteCloseFlow.run(port, flush = false) { result = it ?: "ok" }
    Assert.assertEquals("ok", result)
    Assert.assertEquals(0, port.flushCalls)
    Assert.assertEquals(1, port.destroyed)
  }

  @Test
  fun `normal close destroys only after a durable flush`() {
    val port = FakeClosePort()
    var result = "unset"
    SiteCloseFlow.run(port, flush = true) { result = it ?: "ok" }
    Assert.assertEquals("ok", result)
    Assert.assertEquals(listOf("resolve", "flush:site-x-g0", "destroy"), port.log)
    Assert.assertEquals(1, port.destroyed)
  }

  @Test
  fun `flush failure is an error and keeps the view for a retry`() {
    val port = FakeClosePort().also { it.flushError = RuntimeException("disk full") }
    var result = "unset"
    SiteCloseFlow.run(port, flush = true) { result = it ?: "ok" }
    Assert.assertNotEquals("ok", result)
    Assert.assertTrue("error should mention durability: $result", result.contains("kalıcılığı"))
    Assert.assertEquals("view must stay open", 0, port.destroyed)
    Assert.assertTrue("slot must stay active", port.active)
  }

  @Test
  fun `profile resolution failure is an error, never a silent success`() {
    val port = FakeClosePort().also { it.resolveError = RuntimeException("no profile") }
    var result = "unset"
    SiteCloseFlow.run(port, flush = true) { result = it ?: "ok" }
    Assert.assertNotEquals("ok", result)
    Assert.assertEquals(0, port.flushCalls)
    Assert.assertEquals(0, port.destroyed)
    Assert.assertTrue(port.active)
  }

  @Test
  fun `close reports exactly once even if callbacks misbehave`() {
    val port =
      object : SiteClosePort {
        var results = 0
        override fun hasActiveView(): Boolean = true
        override fun resolveProfileName(): Result<String> = Result.success("site-x-g0")
        override fun flushCookies(profileName: String, onDone: (Throwable?) -> Unit) {
          onDone(null)
          onDone(RuntimeException("late duplicate"))
        }

        override fun destroyActiveView() {}
      }
    var count = 0
    SiteCloseFlow.run(port, flush = true) { count++ }
    Assert.assertEquals(1, count)
  }

  // ---------------------------------------------------------------- clear

  private class FakeClearPort(
    var exists: Boolean = true,
    var cookiesOk: Boolean = true,
    var geoOk: Boolean = true,
    var storageThrows: Boolean = false,
    var flushError: Throwable? = null,
  ) : SiteClearPort {
    val log = mutableListOf<String>()
    var timeouts = mutableListOf<() -> Unit>()

    override fun profileExists(): Boolean {
      log += "exists"
      return exists
    }

    override fun removeAllCookies(onDone: (Boolean) -> Unit) {
      log += "cookies"
      onDone(cookiesOk)
    }

    override fun deleteAllStorage() {
      log += "storage"
      if (storageThrows) throw IllegalStateException("storage boom")
    }

    override fun clearGeolocationPermissions(onDone: (Boolean) -> Unit) {
      log += "geo"
      onDone(geoOk)
    }

    override fun flushCookies(onDone: (Throwable?) -> Unit) {
      log += "flush"
      onDone(flushError)
    }

    override fun scheduleTimeout(millis: Long, onTimeout: () -> Unit) {
      timeouts += onTimeout
    }
  }

  @Test
  fun `clear on an unsupported provider refuses instead of faking success`() {
    val port = FakeClearPort()
    var result = "unset"
    SiteClearFlow.run(supported = false, port = port) { result = it ?: "ok" }
    Assert.assertEquals(SiteProfileErrors.UNSUPPORTED, result)
    Assert.assertTrue("no port operation may run", port.log.isEmpty())
  }

  @Test
  fun `clear of an absent profile is already-clean success`() {
    val port = FakeClearPort(exists = false)
    var result = "unset"
    SiteClearFlow.run(supported = true, port = port) { result = it ?: "ok" }
    Assert.assertEquals("ok", result)
    Assert.assertEquals(listOf("exists"), port.log)
  }

  @Test
  fun `clear success runs every step in order`() {
    val port = FakeClearPort()
    var result = "unset"
    SiteClearFlow.run(supported = true, port = port) { result = it ?: "ok" }
    Assert.assertEquals("ok", result)
    Assert.assertEquals(listOf("exists", "cookies", "geo", "storage", "flush"), port.log)
  }

  @Test
  fun `cookie callback failure fails the clear but still attempts the rest`() {
    val port = FakeClearPort(cookiesOk = false)
    var result = "unset"
    SiteClearFlow.run(supported = true, port = port) { result = it ?: "ok" }
    Assert.assertNotEquals("ok", result)
    Assert.assertTrue("storage must still run: ${port.log}", port.log.contains("storage"))
    Assert.assertTrue("flush must still run: ${port.log}", port.log.contains("flush"))
  }

  @Test
  fun `geolocation callback failure fails the clear`() {
    val port = FakeClearPort(geoOk = false)
    var result = "unset"
    SiteClearFlow.run(supported = true, port = port) { result = it ?: "ok" }
    Assert.assertNotEquals("ok", result)
    Assert.assertTrue(result.contains("geolocation"))
  }

  @Test
  fun `geolocation step settles via the watchdog when the callback never fires`() {
    val port =
      object : SiteClearPort {
        val log = mutableListOf<String>()
        var scheduled: (() -> Unit)? = null
        var results = 0
        override fun profileExists(): Boolean = true
        override fun removeAllCookies(onDone: (Boolean) -> Unit) {
          onDone(true)
        }

        override fun deleteAllStorage() {
          log += "storage"
        }

        override fun clearGeolocationPermissions(onDone: (Boolean) -> Unit) {
          log += "geo-armed"
          // Callback never fires — only the watchdog can settle the step.
        }

        override fun flushCookies(onDone: (Throwable?) -> Unit) {
          log += "flush"
          onDone(null)
        }

        override fun scheduleTimeout(millis: Long, onTimeout: () -> Unit) {
          scheduled = onTimeout
        }
      }
    var last: String? = null
    SiteClearFlow.run(supported = true, port = port) { last = it }
    Assert.assertNull("still pending before the watchdog", last)
    port.scheduled!!.invoke()
    Assert.assertTrue("timeout must fail the clear", (last ?: "ok") != "ok")
    Assert.assertEquals(1, port.log.count { it == "storage" })
    // A late real callback after the watchdog must not double-report.
    var extra = 0
    SiteCloseFlow.run(flush = true, port = object : SiteClosePort {
      override fun hasActiveView(): Boolean = false
      override fun resolveProfileName(): Result<String> = Result.success("x")
      override fun flushCookies(profileName: String, onDone: (Throwable?) -> Unit) {}
      override fun destroyActiveView() {}
    }) { extra++ }
    Assert.assertEquals(1, extra)
  }

  @Test
  fun `storage failure fails the clear but the flush still runs`() {
    val port = FakeClearPort(storageThrows = true)
    var result = "unset"
    SiteClearFlow.run(supported = true, port = port) { result = it ?: "ok" }
    Assert.assertNotEquals("ok", result)
    Assert.assertTrue(result.contains("deposu"))
    Assert.assertTrue(port.log.contains("flush"))
  }

  @Test
  fun `final flush failure is never reported as success`() {
    val port = FakeClearPort(flushError = RuntimeException("io error"))
    var result = "unset"
    SiteClearFlow.run(supported = true, port = port) { result = it ?: "ok" }
    Assert.assertNotEquals("ok", result)
    Assert.assertTrue(result.contains("kalıcılığı"))
  }

  @Test
  fun `clear reports exactly once even with a late duplicate callback`() {
    var count = 0
    var last: String? = null
    val port =
      object : SiteClearPort {
        override fun profileExists(): Boolean = true
        override fun removeAllCookies(onDone: (Boolean) -> Unit) {
          onDone(true)
          onDone(false)
        }

        override fun deleteAllStorage() {}

        override fun clearGeolocationPermissions(onDone: (Boolean) -> Unit) {
          onDone(true)
          onDone(false)
        }

        override fun flushCookies(onDone: (Throwable?) -> Unit) {
          onDone(null)
          onDone(RuntimeException("late"))
        }

        override fun scheduleTimeout(millis: Long, onTimeout: () -> Unit) {}
      }
    SiteClearFlow.run(supported = true, port = port) {
      count++
      last = it
    }
    Assert.assertEquals(1, count)
    Assert.assertNull("duplicate failure must not overwrite the first success", last)
  }

  // ------------------------------------------------------------ migration

  private class FakeMigrationPort(
    var exists: Boolean = false,
    var createThrows: Boolean = false,
    var acceptAll: Boolean = true,
    var setCookieThrowsAt: Int = -1,
    var flushError: Throwable? = null,
    cookieCount: Int = 0,
  ) : SiteMigrationPort {
    val log = mutableListOf<String>()
    var scheduled = mutableListOf<() -> Unit>()
    var cookieIndex = 0
    val totalCookies = cookieCount

    override fun profileExists(): Boolean {
      log += "exists"
      return exists
    }

    override fun createProfile() {
      log += "create"
      if (createThrows) throw IllegalStateException("cannot create")
    }

    override fun setCookie(cookie: String, onAccepted: (Boolean) -> Unit) {
      val i = cookieIndex++
      if (i == setCookieThrowsAt) throw IllegalStateException("set cookie failed")
      log += "cookie$i"
      onAccepted(acceptAll)
    }

    override fun flushCookies(onDone: (Throwable?) -> Unit) {
      log += "flush"
      onDone(flushError)
    }

    override fun scheduleTimeout(millis: Long, onTimeout: () -> Unit) {
      scheduled += onTimeout
    }
  }

  @Test
  fun `migration requires a virgin profile and never touches an existing one`() {
    val port = FakeMigrationPort(exists = true)
    var outcome: MigrationOutcome? = null
    SiteMigrationFlow.run(listOf("a=1", "b=2"), port) { outcome = it }
    Assert.assertTrue(outcome is MigrationOutcome.ProfileAlreadyExists)
    Assert.assertEquals(listOf("exists"), port.log)
  }

  @Test
  fun `migration completes only when every cookie was accepted and flushed`() {
    val port = FakeMigrationPort(cookieCount = 3)
    var outcome: MigrationOutcome? = null
    SiteMigrationFlow.run(listOf("a=1", "b=2", "c=3"), port) { outcome = it }
    val complete = outcome as? MigrationOutcome.Complete
    Assert.assertNotNull("expected Complete, got $outcome", complete)
    Assert.assertEquals(3, complete!!.accepted)
    Assert.assertEquals(listOf("exists", "create", "cookie0", "cookie1", "cookie2", "flush"), port.log)
  }

  @Test
  fun `a rejected cookie fails the migration and stops the chain`() {
    val port = FakeMigrationPort(acceptAll = false)
    var outcome: MigrationOutcome? = null
    SiteMigrationFlow.run(listOf("a=1", "b=2"), port) { outcome = it }
    val failed = outcome as? MigrationOutcome.Failed
    Assert.assertNotNull("rejection must not be a success, got $outcome", failed)
    Assert.assertTrue(failed!!.reason.contains("reddedildi"))
    Assert.assertEquals("chain must stop after the first rejection", 1, port.log.count { it.startsWith("cookie") })
    Assert.assertFalse("doomed profile must not be flushed", port.log.contains("flush"))
  }

  @Test
  fun `createProfile failure fails the migration`() {
    val port = FakeMigrationPort(createThrows = true)
    var outcome: MigrationOutcome? = null
    SiteMigrationFlow.run(listOf("a=1"), port) { outcome = it }
    Assert.assertTrue(outcome is MigrationOutcome.Failed)
  }

  @Test
  fun `setCookie exception fails the migration`() {
    val port = FakeMigrationPort(setCookieThrowsAt = 1)
    var outcome: MigrationOutcome? = null
    SiteMigrationFlow.run(listOf("a=1", "b=2", "c=3"), port) { outcome = it }
    val failed = outcome as? MigrationOutcome.Failed
    Assert.assertNotNull(failed)
    Assert.assertFalse(port.log.contains("flush"))
  }

  @Test
  fun `flush failure fails the migration even with all cookies accepted`() {
    val port = FakeMigrationPort(flushError = RuntimeException("io"))
    var outcome: MigrationOutcome? = null
    SiteMigrationFlow.run(listOf("a=1"), port) { outcome = it }
    val failed = outcome as? MigrationOutcome.Failed
    Assert.assertNotNull("flush failure must not be success, got $outcome", failed)
    Assert.assertTrue(failed!!.reason.contains("kalıcı"))
  }

  @Test
  fun `migration watchdog turns a lost cookie callback into a retryable failure`() {
    val port =
      object : SiteMigrationPort {
        var scheduled: (() -> Unit)? = null
        var outcome: MigrationOutcome? = null
        override fun profileExists(): Boolean = false
        override fun createProfile() {}
        override fun setCookie(cookie: String, onAccepted: (Boolean) -> Unit) {
          // Never answers.
        }

        override fun flushCookies(onDone: (Throwable?) -> Unit) {
          onDone(null)
        }

        override fun scheduleTimeout(millis: Long, onTimeout: () -> Unit) {
          scheduled = onTimeout
        }
      }
    SiteMigrationFlow.run(listOf("a=1"), port) { port.outcome = it }
    Assert.assertNull(port.outcome)
    port.scheduled!!.invoke()
    Assert.assertTrue("timeout must fail the migration", port.outcome is MigrationOutcome.Failed)
  }

  @Test
  fun `migration completes exactly once despite a duplicate cookie callback`() {
    val port =
      object : SiteMigrationPort {
        var count = 0
        override fun profileExists(): Boolean = false
        override fun createProfile() {}
        override fun setCookie(cookie: String, onAccepted: (Boolean) -> Unit) {
          onAccepted(true)
          onAccepted(true) // duplicate
        }

        override fun flushCookies(onDone: (Throwable?) -> Unit) {
          onDone(null)
        }

        override fun scheduleTimeout(millis: Long, onTimeout: () -> Unit) {}
      }
    var results = 0
    SiteMigrationFlow.run(listOf("only"), port) { results++ ; port.count = results }
    Assert.assertEquals(1, results)
  }

  // ---------------------------------------------------------- session slot

  @Test
  fun `failed open releases its own view and a new open can proceed`() {
    val state = SiteSessionState<String>()
    Assert.assertTrue(state.acquire("view-A", "site-h-g0"))
    // Failure path releases exactly A:
    Assert.assertTrue(state.releaseIf("view-A"))
    Assert.assertFalse(state.hasActive())
    // Next open works:
    Assert.assertTrue(state.acquire("view-B", "site-h-g1"))
    Assert.assertEquals("view-B", state.activeToken)
    Assert.assertEquals("site-h-g1", state.activeProfile)
  }

  @Test
  fun `a stale failure cannot clear a newer session`() {
    val state = SiteSessionState<String>()
    Assert.assertTrue(state.acquire("view-A", "site-h-g0"))
    Assert.assertFalse("second acquire must not steal the slot", state.acquire("view-B", "site-h-g1"))
    // Simulate: A released elsewhere, B takes over, THEN A's stale rollback:
    Assert.assertTrue(state.releaseIf("view-A"))
    Assert.assertTrue(state.acquire("view-B", "site-h-g1"))
    Assert.assertFalse("stale token must not release B", state.releaseIf("view-A"))
    Assert.assertEquals("view-B", state.activeToken)
    Assert.assertEquals("site-h-g1", state.activeProfile)
  }

  @Test
  fun `clear() drops the pair together and reports the profile`() {
    val state = SiteSessionState<String>()
    state.acquire("view-A", "site-h-g0")
    Assert.assertEquals("site-h-g0", state.clear())
    Assert.assertNull(state.activeToken)
    Assert.assertNull(state.activeProfile)
    Assert.assertFalse(state.hasActive())
  }

  // ------------------------------------------------------- back handshake

  @Test
  fun `back status pending polls until the budget then gives up silently`() {
    Assert.assertEquals(BackCloseDecision.POLL, SiteBackStatus.decide("pending", 0, 3))
    Assert.assertEquals(BackCloseDecision.POLL, SiteBackStatus.decide("pending", 2, 3))
    Assert.assertEquals(BackCloseDecision.GIVE_UP, SiteBackStatus.decide("pending", 3, 3))
  }

  @Test
  fun `back status closed and failed map to their outcomes`() {
    Assert.assertEquals(BackCloseDecision.CLOSED, SiteBackStatus.decide("closed", 0, 3))
    Assert.assertEquals(BackCloseDecision.FAILED, SiteBackStatus.decide("failed:boom", 0, 3))
    Assert.assertEquals("boom", SiteBackStatus.failureMessage("failed:boom"))
  }

  @Test
  fun `back status with no launcher answer falls back to a native close`() {
    Assert.assertEquals(BackCloseDecision.FALLBACK, SiteBackStatus.decide(null, 0, 3))
    Assert.assertEquals(BackCloseDecision.FALLBACK, SiteBackStatus.decide("idle", 0, 3))
    Assert.assertEquals(BackCloseDecision.FALLBACK, SiteBackStatus.decide("wat", 0, 3))
  }

  // ---------------------------------------------------------- sentinels

  @Test
  fun `sentinels keep their shared literals`() {
    Assert.assertEquals("unsupported-multi-profile", SiteProfileErrors.UNSUPPORTED)
    Assert.assertEquals("migration-partial", SiteProfileErrors.MIGRATION_RETRY)
    Assert.assertEquals(SiteProfileErrors.UNSUPPORTED, AnimeHubSiteController.ERR_UNSUPPORTED_MULTI_PROFILE)
    Assert.assertEquals(SiteProfileErrors.MIGRATION_RETRY, AnimeHubSiteController.ERR_MIGRATION_RETRY)
  }
}
