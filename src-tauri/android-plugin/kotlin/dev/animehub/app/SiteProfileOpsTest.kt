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
import org.junit.Before
import org.junit.Test

class SiteProfileOpsTest {

  /**
   * A name shaped exactly like Rust's `android_profile_name` output, so the
   * quarantine registry is exercised with a realistic key.
   */
  private val profile = "site-00000000000000000000000000000000-g7"

  /** [ProfileQuarantine] is process-global: every test starts from empty. */
  @Before
  fun resetQuarantine() {
    ProfileQuarantine.reset()
    AnimeHubDebugLog.resetForTests()
  }

  // ---------------------------------------------------------------- close

  private class FakeClosePort : SiteClosePort {
    var active = true
    var resolveError: Throwable? = null
    var flushError: Throwable? = null

    /** When true the flush callback never fires — only the watchdog settles. */
    var flushNeverAnswers = false

    /** The captured flush callback, so a test can deliver it LATE. */
    var pendingFlush: ((Throwable?) -> Unit)? = null
    var destroyed = 0
    var resolveCalls = 0
    var flushCalls = 0
    val timeouts = mutableListOf<() -> Unit>()

    /** How many armed watchdogs the flow cancelled after settling. */
    var cancelled = 0
    /** Arming counter, so a test can refuse one specific watchdog. */
    private var armings = 0

    /** How many armings the scheduler refused. */
    var refusals = 0

    /** Refuse every arming (`true`) or only the one with this index. */
    var armFails = false
    var refuseArmingAt: Int = -1
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
      if (flushNeverAnswers) {
        pendingFlush = onDone
        return
      }
      onDone(flushError)
    }

    override fun scheduleTimeout(millis: Long, onTimeout: () -> Unit): Watchdog {
      val index = armings++
      if (armFails || index == refuseArmingAt) {
        // The scheduler refused: nothing was queued, so nothing is recorded as
        // an armed watchdog and no timeout may ever be fired by a test.
        refusals++
        return Watchdog.Refused
      }
      timeouts += onTimeout
      return Watchdog.Armed({ cancelled++ })
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

        override fun scheduleTimeout(millis: Long, onTimeout: () -> Unit): Watchdog =
          Watchdog.Armed({})
        override fun destroyActiveView() {}
      }
    var count = 0
    SiteCloseFlow.run(port, flush = true) { count++ }
    Assert.assertEquals(1, count)
  }

  @Test
  fun `close flush watchdog answers when the callback never arrives`() {
    // The flush is a blocking provider call on a single-threaded executor:
    // without a bound, a lost callback hangs the close — and with it the
    // Rust session lock — forever.
    val port = FakeClosePort().also { it.flushNeverAnswers = true }
    var result: String? = null
    SiteCloseFlow.run(port, flush = true) { result = it }
    Assert.assertNull("still pending before the watchdog", result)
    Assert.assertEquals("the flush await must be armed", 1, port.timeouts.size)
    port.timeouts[0].invoke()
    Assert.assertTrue("timeout must fail the close: $result", (result ?: "ok") != "ok")
    Assert.assertTrue("timeout must name durability: $result", result!!.contains("kalıcılığı"))
    Assert.assertEquals("view must stay open for a retry", 0, port.destroyed)
    Assert.assertTrue("session ownership must stay claimed", port.active)
  }

  @Test
  fun `a late successful flush cannot rescue a timed-out close or destroy the view`() {
    val port = FakeClosePort().also { it.flushNeverAnswers = true }
    var results = 0
    var last: String? = null
    SiteCloseFlow.run(port, flush = true) { results++; last = it }
    port.timeouts[0].invoke()
    Assert.assertEquals(1, results)
    // The blocked flush finally lands AFTER the watchdog answered. It must
    // neither flip the delivered failure back to success nor destroy a view
    // whose ownership was never released (the slot may already belong to a
    // retried close or a newer open).
    port.pendingFlush!!.invoke(null)
    Assert.assertEquals("exactly one result", 1, results)
    Assert.assertTrue("the timeout failure stands: $last", (last ?: "ok") != "ok")
    Assert.assertEquals(0, port.destroyed)
    Assert.assertTrue(port.active)
    // A late FAILURE callback is equally powerless.
    port.pendingFlush!!.invoke(RuntimeException("too late"))
    Assert.assertEquals(1, results)
    Assert.assertEquals(0, port.destroyed)
  }

  @Test
  fun `close flush callback wins the race when it beats the watchdog`() {
    val port = FakeClosePort()
    var results = 0
    var last: String? = "unset"
    SiteCloseFlow.run(port, flush = true) { results++; last = it }
    Assert.assertEquals(1, results)
    Assert.assertNull("callback success stands", last)
    Assert.assertEquals(1, port.destroyed)
    // The stale watchdog firing afterwards changes nothing.
    port.timeouts[0].invoke()
    Assert.assertEquals(1, results)
    Assert.assertNull(last)
    Assert.assertEquals("the view is destroyed exactly once", 1, port.destroyed)
  }

  @Test
  fun `close can be retried after a timeout because ownership was kept`() {
    val port = FakeClosePort().also { it.flushNeverAnswers = true }
    var first: String? = null
    SiteCloseFlow.run(port, flush = true) { first = it }
    port.timeouts[0].invoke()
    Assert.assertTrue((first ?: "ok") != "ok")
    Assert.assertEquals(0, port.destroyed)
    // Retry with a provider that answers this time: the view and the slot
    // were never released, so the second close completes normally.
    port.flushNeverAnswers = false
    var second: String? = "unset"
    SiteCloseFlow.run(port, flush = true) { second = it }
    Assert.assertNull("retry must succeed: $second", second)
    Assert.assertEquals(1, port.destroyed)
    Assert.assertFalse(port.active)
  }

  // ---------------------------------------------------------------- clear

  /**
   * Recording stand-in for `profile.geolocationPermissions`: it counts the
   * `clear` calls the PRODUCTION [GeolocationWipe] triggers and keeps the
   * origins a session currently holds, so "the late callback changed the
   * data" is measurable instead of inferred from an `onDone` count.
   */
  private class FakeGeolocationApi(
    var origins: Set<String>? = setOf("https://a.example", "https://b.example"),
    var hold: Boolean = false,
    var throwOnGet: Boolean = false,
    var clearThrows: Boolean = false,
  ) : GeolocationApi {
    val cleared = mutableListOf<String>()
    var getOriginsCalls = 0

    /** Always captured, so a test can deliver the same callback again/late. */
    var pending: ((Set<String>?) -> Unit)? = null

    override fun getOrigins(onResult: (Set<String>?) -> Unit) {
      getOriginsCalls++
      if (throwOnGet) throw IllegalStateException("provider boom")
      pending = onResult
      if (!hold) onResult(origins)
    }

    override fun clear(origin: String) {
      cleared += origin
      if (clearThrows) throw IllegalStateException("clear boom")
      origins = (origins ?: emptySet()) - origin
    }

    /** Deliver the captured enumeration callback (again, or after a timeout). */
    fun deliver(with: Set<String>? = origins) {
      pending!!.invoke(with)
    }
  }

  private class FakeClearPort(
    var exists: Boolean = true,
    /** The provider's raw answer: were ANY cookies deleted. */
    var cookiesOk: Boolean = true,
    /**
     * The emptiness probe ([SiteClearPort.cookiesCleared]). Defaults to "the
     * jar still holds cookies", so an `Answered(false)` delivery stays a
     * failure unless a test pins the profile as genuinely empty.
     */
    var cookieJarEmpty: Boolean? = false,
    /** The call never reaches the store — a real error, not `Answered(false)`. */
    var cookiesFailWith: String? = null,
    var geoOk: Boolean = true,
    var storageThrows: Boolean = false,
    var flushError: Throwable? = null,
    /** The cookie wipe never answers: only its watchdog can settle the step. */
    var cookiesNeverAnswer: Boolean = false,
    /** The final flush never answers: only its watchdog can settle the wipe. */
    var flushNeverAnswers: Boolean = false,
    /**
     * When set, [cookiesCleared] is computed EXACTLY like the production
     * adapter computes it — [CookieWipes.emptinessOf] over a `hasCookies()`
     * probe — instead of returning [cookieJarEmpty] directly, so the wiring
     * (helper + flow), not only the helper, is under test. A probe that throws
     * models an unreadable store.
     */
    var hasCookiesProbe: (() -> Boolean)? = null,
    /**
     * The scheduler refuses EVERY arming — what `SiteFlushWorker` reports as
     * [Watchdog.Refused] when `Handler.postDelayed` returns `false` or throws.
     */
    var armFails: Boolean = false,
    /** Refuse only the arming with this index (0 = the cookie step's). */
    var refuseArmingAt: Int = -1,
    /** The geolocation enumeration is held so a test can deliver it LATE. */
    geoHoldsCallback: Boolean = false,
    val geo: FakeGeolocationApi = FakeGeolocationApi(hold = geoHoldsCallback),
  ) : SiteClearPort {
    /** How many armed watchdogs the flow cancelled after settling. */
    var cancelled = 0
    /** Arming counter, so a test can refuse one specific watchdog. */
    private var armings = 0

    /** How many armings the scheduler refused. */
    var refusals = 0

    val log = mutableListOf<String>()
    var timeouts = mutableListOf<() -> Unit>()

    /** The captured cookie callback, for late/duplicate delivery. */
    var pendingCookies: ((CookieWipe) -> Unit)? = null
    var pendingFlush: ((Throwable?) -> Unit)? = null

    /** How often the flow consulted the emptiness probe (it must not race). */
    var emptinessChecks = 0

    override fun profileExists(): Boolean {
      log += "exists"
      return exists
    }

    override fun removeAllCookies(onDone: (CookieWipe) -> Unit) {
      log += "cookies"
      if (cookiesNeverAnswer) {
        pendingCookies = onDone
        return
      }
      cookiesFailWith?.let {
        onDone(CookieWipe.Failed(it))
        return
      }
      onDone(CookieWipe.Answered(cookiesOk))
    }

    override fun cookiesCleared(): Boolean? {
      emptinessChecks++
      val probe = hasCookiesProbe
      return if (probe != null) CookieWipes.emptinessOf(probe) else cookieJarEmpty
    }

    override fun deleteAllStorage() {
      log += "storage"
      if (storageThrows) throw IllegalStateException("storage boom")
    }

    /**
     * The real adapter delegates to [GeolocationWipe] over the profile's
     * `GeolocationPermissions`; this fake does exactly the same over
     * [FakeGeolocationApi], so the ordering under test is production code.
     */
    override fun clearGeolocationPermissions(guard: StepGuard, onDone: (Boolean) -> Unit) {
      log += "geo"
      GeolocationWipe.start(guard, geo) { ok -> onDone(ok && geoOk) }
    }

    override fun flushCookies(onDone: (Throwable?) -> Unit) {
      log += "flush"
      if (flushNeverAnswers) {
        pendingFlush = onDone
        return
      }
      onDone(flushError)
    }

    override fun scheduleTimeout(millis: Long, onTimeout: () -> Unit): Watchdog {
      val index = armings++
      if (armFails || index == refuseArmingAt) {
        // The scheduler refused (postDelayed == false, or a throw): nothing was
        // queued, so this is NOT recorded as an armed watchdog and no test may
        // fire it. The flow must fail the step itself and skip the provider call.
        refusals++
        return Watchdog.Refused
      }
      timeouts += onTimeout
      return Watchdog.Armed({ cancelled++ })
    }
  }

  @Test
  fun `clear on an unsupported provider refuses instead of faking success`() {
    AnimeHubDebugLog.setEnabled(true)
    val port = FakeClearPort()
    var result = "unset"
    SiteClearFlow.run(supported = false, profileName = profile, port = port) { result = it ?: "ok" }
    Assert.assertEquals(SiteProfileErrors.UNSUPPORTED, result)
    Assert.assertTrue("no port operation may run", port.log.isEmpty())
    Assert.assertTrue(AnimeHubDebugLog.snapshot().events.any {
      it.area == "clear" && it.phase == "complete" &&
        it.result == "unsupported" && it.errorClass == "unsupported"
    })
  }

  @Test
  fun `clear of an absent profile is already-clean success`() {
    val port = FakeClearPort(exists = false)
    var result = "unset"
    SiteClearFlow.run(supported = true, profileName = profile, port = port) { result = it ?: "ok" }
    Assert.assertEquals("ok", result)
    Assert.assertEquals(listOf("exists"), port.log)
  }

  @Test
  fun `clear success runs every step in order`() {
    val port = FakeClearPort()
    var result = "unset"
    SiteClearFlow.run(supported = true, profileName = profile, port = port) { result = it ?: "ok" }
    Assert.assertEquals("ok", result)
    Assert.assertEquals(listOf("exists", "cookies", "geo", "storage", "flush"), port.log)
  }

  @Test
  fun `diagnostics are observational and redact a clear failure without changing its safety outcome`() {
    AnimeHubDebugLog.setEnabled(true)
    val privateError = "https://private.example/path?token=TOP-SECRET; Cookie: value"
    val port = FakeClearPort(cookiesFailWith = privateError)
    var results = 0
    var result: String? = null
    SiteClearFlow.run(
      supported = true,
      profileName = profile,
      port = port,
      operationId = "k-77",
    ) { error ->
      results++
      result = error
    }

    Assert.assertEquals(1, results)
    Assert.assertEquals(
      "${SiteClearFlow.COOKIE_ACCESS_FAILED_PREFIX}: ${SiteClearFlow.PROVIDER_FAILURE}",
      result,
    )
    Assert.assertEquals(
      "exists, wipe, geolocation, storage, then flush still run",
      listOf("exists", "cookies", "geo", "storage", "flush"),
      port.log,
    )
    Assert.assertFalse("a settled failure does not quarantine the profile", ProfileQuarantine.isBlocked(profile))
    Assert.assertNull("retry remains possible", SiteOpenGate.check(profile))

    val snapshot = AnimeHubDebugLog.snapshot()
    val flowEvents = snapshot.events.filter { it.operationId == "k-77" }
    Assert.assertTrue(flowEvents.any { it.phase == "cookie_wipe" && it.result == "failed" && it.errorClass == "provider" })
    Assert.assertTrue(flowEvents.any { it.phase == "complete" && it.result == "failed" })
    Assert.assertTrue(flowEvents.all { it.source == "kotlin" })
    val exported = snapshot.toString()
    Assert.assertFalse(exported.contains("private.example"))
    Assert.assertFalse(exported.contains("TOP-SECRET"))
    Assert.assertFalse(exported.contains("Cookie"))
  }

  @Test
  fun `diagnostics record a timed-out cookie wipe and late settlement without reopening the profile`() {
    AnimeHubDebugLog.setEnabled(true)
    val port = FakeClearPort(cookiesNeverAnswer = true)
    var results = 0
    var result: String? = null
    SiteClearFlow.run(
      supported = true,
      profileName = profile,
      port = port,
      operationId = "k-78",
    ) { error ->
      results++
      result = error
    }

    Assert.assertNull("the cookie call is still pending", result)
    Assert.assertTrue("no quarantine before timeout", ProfileQuarantine.pendingOperations(profile).isEmpty())
    port.timeouts[0].invoke()
    Assert.assertEquals(
      "the timeout owns one outstanding deletion",
      1,
      ProfileQuarantine.pendingOperations(profile).size,
    )
    Assert.assertEquals(1, results)
    Assert.assertNotNull("timeout is not reported as a successful clear", result)
    Assert.assertTrue("the profile remains blocked", ProfileQuarantine.isBlocked(profile))
    Assert.assertNotNull(SiteOpenGate.check(profile))

    port.pendingCookies!!.invoke(CookieWipe.Answered(true))
    Assert.assertEquals("late callback cannot re-report the operation", 1, results)
    Assert.assertTrue("settling is not verification", ProfileQuarantine.isBlocked(profile))
    val flowEvents = AnimeHubDebugLog.snapshot().events.filter { it.operationId == "k-78" }
    Assert.assertTrue(flowEvents.any {
      it.phase == "cookie_wipe" && it.result == "timeout" && it.watchdog == "fired"
    })
    Assert.assertTrue(flowEvents.any { it.phase == "quarantine" && it.result == "quarantined" })
    Assert.assertTrue(flowEvents.any {
      it.phase == "late_callback" && it.result == "late" && it.watchdog == "late"
    })
    val exported = AnimeHubDebugLog.snapshot().toString()
    Assert.assertFalse(exported.contains(profile))
    Assert.assertFalse(exported.contains("Cookie"))
  }

  @Test
  fun `cookie callback failure fails the clear but still attempts the rest`() {
    // The provider deleted NOTHING and the jar still holds cookies: a real
    // failure, reported with the precise reason, and every later step still runs.
    val port = FakeClearPort(cookiesOk = false, cookieJarEmpty = false)
    var result = "unset"
    SiteClearFlow.run(supported = true, profileName = profile, port = port) { result = it ?: "ok" }
    Assert.assertEquals(SiteClearFlow.COOKIE_STILL_PRESENT_REASON, result)
    Assert.assertTrue("storage must still run: ${port.log}", port.log.contains("storage"))
    Assert.assertTrue("flush must still run: ${port.log}", port.log.contains("flush"))
    Assert.assertEquals("the emptiness probe decides this step exactly once", 1, port.emptinessChecks)
  }

  @Test
  fun `geolocation callback failure fails the clear`() {
    val port = FakeClearPort(geoOk = false)
    var result = "unset"
    SiteClearFlow.run(supported = true, profileName = profile, port = port) { result = it ?: "ok" }
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
        override fun removeAllCookies(onDone: (CookieWipe) -> Unit) {
          onDone(CookieWipe.Answered(true))
        }

        override fun cookiesCleared(): Boolean? = false

        override fun deleteAllStorage() {
          log += "storage"
        }

        override fun clearGeolocationPermissions(guard: StepGuard, onDone: (Boolean) -> Unit) {
          log += "geo-armed"
          // Callback never fires — only the watchdog can settle the step.
        }

        override fun flushCookies(onDone: (Throwable?) -> Unit) {
          log += "flush"
          onDone(null)
        }

        override fun scheduleTimeout(millis: Long, onTimeout: () -> Unit): Watchdog {
          scheduled = onTimeout
          return Watchdog.Armed({})
          }
      }
    var last: String? = null
    SiteClearFlow.run(supported = true, profileName = profile, port = port) { last = it }
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
      override fun scheduleTimeout(millis: Long, onTimeout: () -> Unit): Watchdog =
          Watchdog.Armed({})
      override fun destroyActiveView() {}
    }) { extra++ }
    Assert.assertEquals(1, extra)
  }

  @Test
  fun `storage failure fails the clear but the flush still runs`() {
    val port = FakeClearPort(storageThrows = true)
    var result = "unset"
    SiteClearFlow.run(supported = true, profileName = profile, port = port) { result = it ?: "ok" }
    Assert.assertNotEquals("ok", result)
    Assert.assertTrue(result.contains("deposu"))
    Assert.assertTrue(port.log.contains("flush"))
  }

  @Test
  fun `final flush failure is never reported as success`() {
    val port = FakeClearPort(flushError = RuntimeException("io error"))
    var result = "unset"
    SiteClearFlow.run(supported = true, profileName = profile, port = port) { result = it ?: "ok" }
    Assert.assertNotEquals("ok", result)
    Assert.assertTrue(result.contains("kalıcılığı"))
  }

  @Test
  fun `clear final flush watchdog fails the wipe when the callback never arrives`() {
    val port =
      object : SiteClearPort {
        val timeouts = mutableListOf<() -> Unit>()
        var pendingFlush: ((Throwable?) -> Unit)? = null
        override fun profileExists(): Boolean = true
        override fun removeAllCookies(onDone: (CookieWipe) -> Unit) {
          onDone(CookieWipe.Answered(true))
        }

        override fun cookiesCleared(): Boolean? = false

        override fun deleteAllStorage() {}

        override fun clearGeolocationPermissions(guard: StepGuard, onDone: (Boolean) -> Unit) {
          onDone(true)
        }

        override fun flushCookies(onDone: (Throwable?) -> Unit) {
          // Never answers: only the watchdog can settle the final step.
          pendingFlush = onDone
        }

        override fun scheduleTimeout(millis: Long, onTimeout: () -> Unit): Watchdog {
          timeouts += onTimeout
          return Watchdog.Armed({})
          }
      }
    var results = 0
    var last: String? = null
    SiteClearFlow.run(supported = true, profileName = profile, port = port) { results++; last = it }
    Assert.assertNull("still pending before the flush watchdog", last)
    Assert.assertEquals(0, results)
    // Armed watchdogs: cookies step, geo step, final flush — fire the last.
    Assert.assertEquals(3, port.timeouts.size)
    port.timeouts[2].invoke()
    Assert.assertEquals(1, results)
    Assert.assertTrue("timeout must fail the clear: $last", (last ?: "ok") != "ok")
    Assert.assertTrue("timeout must name durability: $last", last!!.contains("kalıcılığı"))
    // Rust sees the failure, keeps the encrypted session blobs for a retry,
    // and never reports a half-finished wipe as success. A late flush
    // callback (even a successful one) cannot flip that.
    port.pendingFlush!!.invoke(null)
    Assert.assertEquals("exactly one result", 1, results)
    Assert.assertTrue("the timeout failure stands: $last", (last ?: "ok") != "ok")
  }

  @Test
  fun `a stale clear watchdog cannot change an already delivered result`() {
    val port = FakeClearPort()
    var results = 0
    var last: String? = "unset"
    SiteClearFlow.run(supported = true, profileName = profile, port = port) { results++; last = it }
    Assert.assertEquals(1, results)
    Assert.assertNull("every step answered: success", last)
    // Fire every armed watchdog after the fact (steps + final flush).
    port.timeouts.forEach { it.invoke() }
    Assert.assertEquals("no watchdog may double-report", 1, results)
    Assert.assertNull("success stands", last)
  }

  @Test
  fun `clear reports exactly once even with a late duplicate callback`() {
    var count = 0
    var last: String? = null
    val port =
      object : SiteClearPort {
        override fun profileExists(): Boolean = true
        override fun removeAllCookies(onDone: (CookieWipe) -> Unit) {
          onDone(CookieWipe.Answered(true))
          // A duplicate delivery with the OPPOSITE raw answer: the latch must
          // discard it, so neither the verdict nor the emptiness probe of this
          // second delivery may reach the reported result.
          onDone(CookieWipe.Answered(false))
        }

        override fun cookiesCleared(): Boolean? = false

        override fun deleteAllStorage() {}

        override fun clearGeolocationPermissions(guard: StepGuard, onDone: (Boolean) -> Unit) {
          onDone(true)
          onDone(false)
        }

        override fun flushCookies(onDone: (Throwable?) -> Unit) {
          onDone(null)
          onDone(RuntimeException("late"))
        }

        override fun scheduleTimeout(millis: Long, onTimeout: () -> Unit): Watchdog =
          Watchdog.Armed({})
      }
    SiteClearFlow.run(supported = true, profileName = profile, port = port) {
      count++
      last = it
    }
    Assert.assertEquals(1, count)
    Assert.assertNull("duplicate failure must not overwrite the first success", last)
  }

  // ------------------------------------------- timeout side-effect races
  //
  // A watchdog bounds the ANSWER a flow gives its caller. These tests are
  // about the DATA: a step that timed out must not let its late callback
  // mutate a profile that may already back a new session. They measure the
  // `geo.clear` calls the production GeolocationWipe triggers and the
  // quarantine the ungated `removeAllCookies` timeout must raise — never an
  // `onDone` count alone.

  @Test
  fun `geolocation permissions are cleared when the callback beats the watchdog`() {
    val port = FakeClearPort()
    var result = "unset"
    SiteClearFlow.run(supported = true, profileName = profile, port = port) { result = it ?: "ok" }
    Assert.assertEquals("ok", result)
    // The real side effect happened, once per stored origin, in the awaited step.
    Assert.assertEquals(
      listOf("https://a.example", "https://b.example"),
      port.geo.cleared,
    )
    Assert.assertEquals(1, port.geo.getOriginsCalls)
    // A verified wipe leaves the profile usable: no block, gate says open.
    Assert.assertFalse(ProfileQuarantine.isBlocked(profile))
    Assert.assertNull(SiteOpenGate.check(profile))
  }

  @Test
  fun `a geolocation callback that lost to the watchdog performs no clear at all`() {
    val port = FakeClearPort(geoHoldsCallback = true)
    var results = 0
    var last: String? = null
    SiteClearFlow.run(supported = true, profileName = profile, port = port) { results++; last = it }
    Assert.assertNull("pending until the geo watchdog fires", last)
    // Armed so far: cookies(0) answered, geolocation(1) is held. The final
    // flush watchdog(2) only exists once this step settles.
    Assert.assertEquals(2, port.timeouts.size)
    port.timeouts[1].invoke()
    Assert.assertEquals("the flush watchdog is armed after the geo step", 3, port.timeouts.size)
    Assert.assertEquals(1, results)
    Assert.assertTrue("timeout must fail the wipe: $last", (last ?: "ok") != "ok")
    Assert.assertTrue(last!!.contains("geolocation"))
    // NOW the provider answers. The mutation window is closed, so the wipe
    // that timed out must not touch the profile it gave up on.
    port.geo.deliver(setOf("https://a.example", "https://b.example"))
    Assert.assertEquals(
      "a timed-out step must trigger zero geo.clear calls",
      emptyList<String>(),
      port.geo.cleared,
    )
    Assert.assertEquals("the delivered failure stands", 1, results)
    Assert.assertTrue("a late callback cannot re-report", (last ?: "ok") != "ok")
    // Nothing is in flight (getOrigins is read-only, the clears were refused),
    // so this step quarantines nothing — the gate stays open by proof, not luck.
    Assert.assertFalse(ProfileQuarantine.isBlocked(profile))
    Assert.assertNull(SiteOpenGate.check(profile))
  }

  @Test
  fun `a stale geolocation callback cannot change a reopened session permissions`() {
    val port = FakeClearPort(geoHoldsCallback = true)
    var last: String? = null
    SiteClearFlow.run(supported = true, profileName = profile, port = port) { last = it }
    port.timeouts[1].invoke()
    Assert.assertTrue("the wipe failed: $last", (last ?: "ok") != "ok")
    // The user reopens the SAME profile name (allowed: the geo step is
    // provably side-effect-free) and the new session grants a permission.
    Assert.assertNull("reopen is allowed after a gated timeout", SiteOpenGate.check(profile))
    val granted = mutableSetOf("https://new-session.example")
    port.geo.origins = granted
    // The old callback finally lands, mid-new-session.
    port.geo.deliver(setOf("https://new-session.example"))
    Assert.assertEquals("the stale callback must clear nothing", emptyList<String>(), port.geo.cleared)
    Assert.assertEquals(
      "the new session keeps its permission",
      setOf("https://new-session.example"),
      granted,
    )
  }

  @Test
  fun `a duplicated geolocation callback repeats neither the clears nor the result`() {
    val port = FakeClearPort()
    var results = 0
    var last = "unset"
    SiteClearFlow.run(supported = true, profileName = profile, port = port) {
      results++
      last = it ?: "ok"
    }
    Assert.assertEquals("ok", last)
    Assert.assertEquals(2, port.geo.cleared.size)
    // Same enumeration callback delivered again (a provider double-fire).
    port.geo.deliver(setOf("https://a.example", "https://b.example"))
    port.geo.deliver(setOf("https://a.example"))
    Assert.assertEquals("no second wipe", 2, port.geo.cleared.size)
    Assert.assertEquals("exactly one result", 1, results)
    Assert.assertEquals("the first result stands", "ok", last)
  }

  @Test
  fun `a port that claimed the guard but reported late still loses to the watchdog`() {
    // The documented cross-thread interleaving: the port won the mutation
    // window and then stalled, so the watchdog settles the step first. The
    // result is reported exactly once and in the conservative direction
    // (failure → Rust keeps the blobs → idempotent retry), and the late
    // success cannot flip it.
    val port =
      object : SiteClearPort {
        val timeouts = mutableListOf<() -> Unit>()
        var report: ((Boolean) -> Unit)? = null

        override fun profileExists(): Boolean = true

        override fun removeAllCookies(onDone: (CookieWipe) -> Unit) {
          onDone(CookieWipe.Answered(true))
        }

        override fun cookiesCleared(): Boolean? = false

        override fun deleteAllStorage() {}

        override fun clearGeolocationPermissions(guard: StepGuard, onDone: (Boolean) -> Unit) {
          report = onDone
          guard.claim() // wins the mutation window, then never reports
        }

        override fun flushCookies(onDone: (Throwable?) -> Unit) {
          onDone(null)
        }

        override fun scheduleTimeout(millis: Long, onTimeout: () -> Unit): Watchdog {
          timeouts += onTimeout
          return Watchdog.Armed({})
          }
      }
    var results = 0
    var last: String? = null
    SiteClearFlow.run(supported = true, profileName = profile, port = port) { results++; last = it }
    Assert.assertNull("pending: the geo step never reported", last)
    port.timeouts[1].invoke()
    Assert.assertEquals(1, results)
    Assert.assertTrue("the watchdog settled it: $last", last!!.contains("geolocation"))
    port.report!!.invoke(true)
    Assert.assertEquals("exactly one result", 1, results)
    Assert.assertTrue("the late success cannot flip it: $last", (last ?: "ok") != "ok")
    Assert.assertFalse(
      "a gated step quarantines nothing",
      ProfileQuarantine.isBlocked(profile),
    )
  }

  @Test
  fun `an unverifiable cookie wipe blocks the profile and refuses a new session`() {
    val port = FakeClearPort(cookiesNeverAnswer = true)
    var results = 0
    var last: String? = null
    SiteClearFlow.run(supported = true, profileName = profile, port = port) { results++; last = it }
    Assert.assertNull("pending until the cookie watchdog fires", last)
    port.timeouts[0].invoke()
    Assert.assertEquals(1, results)
    Assert.assertTrue("timeout must fail the wipe: $last", (last ?: "ok") != "ok")
    Assert.assertTrue(last!!.contains("çerez"))
    // `removeAllCookies` cannot be cancelled and its completion is unverified:
    // it may still delete cookies AFTER this failure, so the profile must not
    // back a new session yet. The block is owned by THIS operation's id, not
    // by a name-wide flag another attempt could clear.
    Assert.assertTrue(ProfileQuarantine.isBlocked(profile))
    Assert.assertEquals(
      "exactly one outstanding operation",
      1,
      ProfileQuarantine.pendingOperations(profile).size,
    )
    val refusal = SiteOpenGate.check(profile)
    Assert.assertNotNull("the open must be refused", refusal)
    Assert.assertTrue(refusal!!.startsWith(SiteProfileErrors.QUARANTINED))
    Assert.assertTrue(refusal.contains(SiteClearFlow.COOKIE_UNVERIFIED_REASON))
    // Nothing here may assume the native call finished: it is still captured
    // and undelivered, which is the state every recovery test below starts from.
    Assert.assertNotNull("the provider operation is still outstanding", port.pendingCookies)
  }

  @Test
  fun `a verified retry cannot lift the block while an older wipe is still outstanding`() {
    // Clear A: the cookie wipe times out and its callback never arrives.
    val a = FakeClearPort(cookiesNeverAnswer = true)
    SiteClearFlow.run(supported = true, profileName = profile, port = a) {}
    a.timeouts[0].invoke()
    val outstanding = ProfileQuarantine.pendingOperations(profile)
    Assert.assertEquals(1, outstanding.size)

    // Clear B succeeds: every step answers, so B is fully verified. Under the
    // old single-record registry that success "released" the name while A's
    // native deletion was still in flight — and it then landed on the session
    // that reopened the profile. It must not.
    val b = FakeClearPort()
    var bResult = "unset"
    SiteClearFlow.run(supported = true, profileName = profile, port = b) { bResult = it ?: "ok" }
    Assert.assertEquals("B itself is a verified wipe", "ok", bResult)
    Assert.assertTrue(
      "B's success must not erase A's outstanding operation",
      ProfileQuarantine.isBlocked(profile),
    )
    Assert.assertEquals(
      "A's record is untouched",
      outstanding,
      ProfileQuarantine.pendingOperations(profile),
    )
    Assert.assertNotNull("the site must stay closed", SiteOpenGate.check(profile))
    Assert.assertNotNull("A's callback is STILL undelivered", a.pendingCookies)
  }

  @Test
  fun `a late answer closes only its own operation while another is outstanding`() {
    // A and B both time out: two independent destructive operations in flight.
    val a = FakeClearPort(cookiesNeverAnswer = true)
    SiteClearFlow.run(supported = true, profileName = profile, port = a) {}
    a.timeouts[0].invoke()
    val b = FakeClearPort(cookiesNeverAnswer = true)
    SiteClearFlow.run(supported = true, profileName = profile, port = b) {}
    b.timeouts[0].invoke()
    val both = ProfileQuarantine.pendingOperations(profile)
    Assert.assertEquals(2, both.size)

    // A's callback arrives while B is still outstanding.
    a.pendingCookies!!.invoke(CookieWipe.Answered(true))
    val left = ProfileQuarantine.pendingOperations(profile)
    Assert.assertEquals("A closed exactly one record", 1, left.size)
    Assert.assertEquals("and it was A's, not B's", both[1], left[0])
    Assert.assertTrue(
      "B is still in flight, so the profile stays blocked",
      ProfileQuarantine.isBlocked(profile),
    )
    Assert.assertNotNull(SiteOpenGate.check(profile))

    // Only B's own answer closes B; then the verification is still stale.
    b.pendingCookies!!.invoke(CookieWipe.Answered(true))
    Assert.assertTrue(ProfileQuarantine.pendingOperations(profile).isEmpty())
    Assert.assertTrue("nothing outstanding, but nothing verified since", ProfileQuarantine.isBlocked(profile))
    Assert.assertTrue(
      SiteOpenGate.check(profile)!!.contains(ProfileQuarantine.STALE_VERIFICATION_REASON),
    )
    // A wipe verified AFTER the last settle is what reopens the profile.
    val c = FakeClearPort()
    var cResult = "unset"
    SiteClearFlow.run(supported = true, profileName = profile, port = c) { cResult = it ?: "ok" }
    Assert.assertEquals("ok", cResult)
    Assert.assertFalse(ProfileQuarantine.isBlocked(profile))
    Assert.assertNull(SiteOpenGate.check(profile))
  }

  @Test
  fun `a late answer arriving after a verified clear does not declare the profile safe`() {
    // A times out; B is verified while A is outstanding; only THEN does A's
    // native deletion finally complete.
    val a = FakeClearPort(cookiesNeverAnswer = true)
    SiteClearFlow.run(supported = true, profileName = profile, port = a) {}
    a.timeouts[0].invoke()
    val b = FakeClearPort()
    SiteClearFlow.run(supported = true, profileName = profile, port = b) {}
    Assert.assertTrue(ProfileQuarantine.isBlocked(profile))

    a.pendingCookies!!.invoke(CookieWipe.Answered(true))
    // A's answer closes A's record, and that is all it may conclude: the last
    // verification (B) happened BEFORE the last destructive operation settled,
    // so the profile's state has not been verified since it became observable.
    Assert.assertTrue(ProfileQuarantine.pendingOperations(profile).isEmpty())
    Assert.assertTrue(
      "a late answer alone cannot make the reopen safe",
      ProfileQuarantine.isBlocked(profile),
    )
    Assert.assertTrue(
      SiteOpenGate.check(profile)!!.contains(ProfileQuarantine.STALE_VERIFICATION_REASON),
    )
    // Final state check: a wipe verified after the last settle reopens it.
    val c = FakeClearPort()
    var cResult = "unset"
    SiteClearFlow.run(supported = true, profileName = profile, port = c) { cResult = it ?: "ok" }
    Assert.assertEquals("ok", cResult)
    Assert.assertFalse(ProfileQuarantine.isBlocked(profile))
    Assert.assertNull("reliable end state: openable", SiteOpenGate.check(profile))
  }

  @Test
  fun `three outstanding operations lift the block only when all settled and re-verified`() {
    val ports =
      List(3) { FakeClearPort(cookiesNeverAnswer = true) }.also { list ->
        list.forEach { port ->
          SiteClearFlow.run(supported = true, profileName = profile, port = port) {}
          port.timeouts[0].invoke()
        }
      }
    Assert.assertEquals(3, ProfileQuarantine.pendingOperations(profile).size)
    Assert.assertTrue(ProfileQuarantine.isBlocked(profile))

    // Callbacks land out of order: C, then A, then B.
    ports[2].pendingCookies!!.invoke(CookieWipe.Answered(true))
    Assert.assertEquals(2, ProfileQuarantine.pendingOperations(profile).size)
    Assert.assertTrue(ProfileQuarantine.isBlocked(profile))
    ports[0].pendingCookies!!.invoke(CookieWipe.Answered(false))
    Assert.assertEquals(1, ProfileQuarantine.pendingOperations(profile).size)
    Assert.assertTrue(ProfileQuarantine.isBlocked(profile))

    // A fully verified wipe in the middle changes nothing: one op outstanding.
    val mid = FakeClearPort()
    SiteClearFlow.run(supported = true, profileName = profile, port = mid) {}
    Assert.assertTrue(
      "a verified wipe cannot erase an outstanding operation",
      ProfileQuarantine.isBlocked(profile),
    )

    ports[1].pendingCookies!!.invoke(CookieWipe.Answered(true))
    Assert.assertTrue(ProfileQuarantine.pendingOperations(profile).isEmpty())
    Assert.assertTrue(
      "the verification predates the last settle",
      ProfileQuarantine.isBlocked(profile),
    )
    val last = FakeClearPort()
    var result = "unset"
    SiteClearFlow.run(supported = true, profileName = profile, port = last) { result = it ?: "ok" }
    Assert.assertEquals("ok", result)
    Assert.assertFalse(ProfileQuarantine.isBlocked(profile))
    Assert.assertNull(SiteOpenGate.check(profile))
  }

  @Test
  fun `a duplicated cookie callback closes its record once and lifts nothing else`() {
    val a = FakeClearPort(cookiesNeverAnswer = true)
    var aResults = 0
    SiteClearFlow.run(supported = true, profileName = profile, port = a) { aResults++ }
    a.timeouts[0].invoke()
    val b = FakeClearPort(cookiesNeverAnswer = true)
    SiteClearFlow.run(supported = true, profileName = profile, port = b) {}
    b.timeouts[0].invoke()
    val both = ProfileQuarantine.pendingOperations(profile)
    Assert.assertEquals(2, both.size)

    a.pendingCookies!!.invoke(CookieWipe.Answered(true)) // A settles
    a.pendingCookies!!.invoke(CookieWipe.Answered(true)) // duplicate delivery of the SAME callback
    a.pendingCookies!!.invoke(CookieWipe.Answered(false))
    Assert.assertEquals("A reported exactly once", 1, aResults)
    Assert.assertEquals(
      "the duplicate closed nothing — B is still outstanding",
      listOf(both[1]),
      ProfileQuarantine.pendingOperations(profile),
    )
    Assert.assertTrue(ProfileQuarantine.isBlocked(profile))
    // And at the registry level a duplicate/unknown settle is refused outright.
    Assert.assertFalse(ProfileQuarantine.settle(profile, both[0]))
    Assert.assertFalse(ProfileQuarantine.settle(profile, 999_999L))
    Assert.assertEquals(listOf(both[1]), ProfileQuarantine.pendingOperations(profile))
  }

  // ------------------------------------------- watchdog arming + cancellation

  @Test
  fun `a settled clear cancels every watchdog it armed`() {
    val port = FakeClearPort()
    var last: String? = "unset"
    SiteClearFlow.run(supported = true, profileName = profile, port = port) { last = it ?: "ok" }
    Assert.assertEquals("ok", last)
    Assert.assertTrue("watchdogs were armed", port.timeouts.isNotEmpty())
    Assert.assertEquals(
      "a settled step must not leave its watchdog queued: the delayed runnable " +
        "keeps the port — and through it the WebView and the Activity — reachable " +
        "until it fires",
      port.timeouts.size,
      port.cancelled,
    )
  }

  @Test
  fun `the watchdog that fired is the only one a timed-out clear leaves queued`() {
    val port = FakeClearPort(cookiesNeverAnswer = true)
    var last: String? = "unset"
    SiteClearFlow.run(supported = true, profileName = profile, port = port) { last = it }
    Assert.assertEquals("suspended until its watchdog", "unset", last)
    Assert.assertEquals(0, port.cancelled)

    port.timeouts[0]()
    val delivered = last
    Assert.assertTrue("the timeout answered the caller", delivered!!.contains("zaman aşımı"))
    // The fired watchdog is not cancelled; every later step cancels its own.
    Assert.assertEquals(port.timeouts.size - 1, port.cancelled)

    // A late cookie answer neither changes the delivered result nor cancels more.
    val cancelledBefore = port.cancelled
    port.pendingCookies!!.invoke(CookieWipe.Answered(true))
    Assert.assertEquals(cancelledBefore, port.cancelled)
    Assert.assertEquals(delivered, last)
  }

  @Test
  fun `a refused watchdog fails the step and never starts the provider call`() {
    // `Handler.postDelayed` reports a refusal by returning false (no exception),
    // and SiteFlushWorker turns that into Watchdog.Refused. The flow must answer
    // its caller instead of waiting for a watchdog that will never fire — and it
    // must NOT issue the provider call nobody could bound.
    AnimeHubDebugLog.setEnabled(true)
    val port = FakeClearPort(armFails = true)
    var results = 0
    var last: String? = "unset"
    SiteClearFlow.run(
      supported = true,
      profileName = profile,
      port = port,
      operationId = "k-79",
    ) {
      results++
      last = it
    }
    Assert.assertEquals("the caller is answered exactly once", 1, results)
    Assert.assertTrue("refusal is reported as its own outcome: $last",
      last!!.contains(SiteProfileErrors.WATCHDOG_REFUSED))
    // No provider work was started: only the synchronous, unawaited storage
    // step ran. The cookie, geolocation and flush calls never happened.
    Assert.assertEquals(listOf("exists", "storage"), port.log)
    Assert.assertNull("no cookie callback was ever captured", port.pendingCookies)
    Assert.assertNull("no flush callback was ever captured", port.pendingFlush)
    Assert.assertEquals(0, port.geo.getOriginsCalls)
    Assert.assertTrue(port.refusals >= 1)
    Assert.assertTrue("nothing was armed, so nothing may be fireable", port.timeouts.isEmpty())
    // And because no destructive call was issued, nothing is outstanding: the
    // ledger tracks provider operations that may still land, not refusals.
    Assert.assertFalse(ProfileQuarantine.isBlocked(profile))
    Assert.assertNull(SiteOpenGate.check(profile))
    val flowEvents = AnimeHubDebugLog.snapshot().events.filter { it.operationId == "k-79" }
    Assert.assertTrue(flowEvents.any {
      it.phase == "cookie_wipe" && it.result == "failed" &&
        it.watchdog == "refused" && it.errorClass == "watchdog_refused"
    })
    Assert.assertFalse(AnimeHubDebugLog.snapshot().toString().contains(profile))
  }

  @Test
  fun `a refusal of only the final flush skips the flush and fails the wipe`() {
    // Arming index 2 is the final flush (0 = cookie step, 1 = geolocation).
    val port = FakeClearPort(refuseArmingAt = 2)
    var last: String? = "unset"
    SiteClearFlow.run(supported = true, profileName = profile, port = port) { last = it ?: "ok" }
    Assert.assertTrue("the earlier steps really ran: ${port.log}",
      port.log.containsAll(listOf("exists", "cookies", "geo", "storage")))
    Assert.assertFalse("the unbounded flush is never started", port.log.contains("flush"))
    Assert.assertNull(port.pendingFlush)
    Assert.assertEquals(1, port.refusals)
    Assert.assertEquals(2, port.cancelled) // the two armed watchdogs were cancelled
    Assert.assertEquals("çerez kalıcılığı: ${SiteProfileErrors.WATCHDOG_REFUSED}", last)
  }

  @Test
  fun `WatchdogArming treats a false postDelayed exactly like a throw`() {
    val cancel: () -> Unit = {}
    val armed = WatchdogArming.decide(queued = true, thrown = null, cancel = cancel)
    Assert.assertTrue("a queued runnable is armed", armed is Watchdog.Armed)
    Assert.assertNull(WatchdogArming.refusal(queued = true, thrown = null))

    // THE CONTRACT THIS ROUND FIXES: postDelayed returns false and throws
    // nothing, so a try/catch alone never sees it.
    val refusedByReturnValue = WatchdogArming.decide(queued = false, thrown = null, cancel = cancel)
    Assert.assertEquals(Watchdog.Refused, refusedByReturnValue)
    Assert.assertEquals(
      "watchdog_refused",
      WatchdogArming.refusal(queued = false, thrown = null),
    )

    val refusedByThrow = WatchdogArming.decide(queued = false, thrown = IllegalStateException("looper gone"), cancel = cancel)
    Assert.assertEquals(Watchdog.Refused, refusedByThrow)
    Assert.assertEquals(
      "watchdog_refused",
      WatchdogArming.refusal(queued = false, thrown = IllegalStateException("private exception text")),
    )
    // A throw wins even if something claims it queued: never hand out a canceller
    // for a runnable that is not in the queue.
    Assert.assertEquals(
      Watchdog.Refused,
      WatchdogArming.decide(queued = true, thrown = IllegalStateException("x"), cancel = cancel),
    )
  }

  @Test
  fun `a refused close watchdog keeps the view and never starts the flush`() {
    val port = FakeClosePort()
    port.armFails = true
    var results = 0
    var last: String? = null
    SiteCloseFlow.run(port, flush = true) { results++; last = it }
    Assert.assertEquals(1, results)
    Assert.assertTrue(last!!.contains(SiteProfileErrors.WATCHDOG_REFUSED))
    Assert.assertTrue("keeps the retryable wording", last!!.contains("görünüm açık kaldı"))
    Assert.assertEquals("the blocking flush is never started", 0, port.flushCalls)
    Assert.assertNull("no flush callback exists to arrive late", port.pendingFlush)
    Assert.assertEquals("the view survives: durability was never verified", 0, port.destroyed)
    Assert.assertTrue(port.timeouts.isEmpty())
  }

  @Test
  fun `a refused migration watchdog writes no cookie and claims no success`() {
    // Refuse the very first arming: the per-cookie watchdog.
    val cookiePort = FakeMigrationPort()
    cookiePort.armFails = true
    var outcome: MigrationOutcome? = null
    SiteMigrationFlow.run(listOf("a=1", "b=2"), cookiePort) { outcome = it }
    val failed = outcome as? MigrationOutcome.Failed
    Assert.assertNotNull("a refusal is a retryable failure, never Complete: $outcome", failed)
    Assert.assertTrue(failed!!.reason.contains(SiteProfileErrors.WATCHDOG_REFUSED))
    Assert.assertFalse("no cookie was written", cookiePort.log.any { it.startsWith("cookie") })
    Assert.assertFalse("and no flush was started", cookiePort.log.contains("flush"))

    // Refuse only the flush arming (index 1 with a single cookie): the cookie
    // was written, but an unbounded flush still cannot pass as durability.
    val flushPort = FakeMigrationPort(cookieCount = 1)
    flushPort.refuseArmingAt = 1
    var second: MigrationOutcome? = null
    SiteMigrationFlow.run(listOf("a=1"), flushPort) { second = it }
    val failedFlush = second as? MigrationOutcome.Failed
    Assert.assertNotNull("expected Failed, got $second", failedFlush)
    Assert.assertTrue(failedFlush!!.reason.contains("kalıcı hale getirilemedi"))
    Assert.assertTrue(failedFlush.reason.contains(SiteProfileErrors.WATCHDOG_REFUSED))
    Assert.assertTrue("the cookie step did run", flushPort.log.contains("cookie0"))
    Assert.assertFalse("the unbounded flush did not", flushPort.log.contains("flush"))
    Assert.assertEquals(1, flushPort.cancelled)
  }

  @Test
  fun `a settled migration cancels its per-cookie and flush watchdogs`() {
    val port = FakeMigrationPort(cookieCount = 2)
    var outcome: MigrationOutcome? = null
    SiteMigrationFlow.run(listOf("a=1", "b=2"), port) { outcome = it }
    Assert.assertNotNull("expected Complete, got $outcome", outcome as? MigrationOutcome.Complete)
    Assert.assertEquals("one watchdog per cookie plus the flush", 3, port.scheduled.size)
    Assert.assertEquals(port.scheduled.size, port.cancelled)
  }

  // ------------------------------------------------- shutdown (destroy) flow

  /**
   * Models the destroy path: [submitFlush] only HANDS the flush over (it never
   * runs inline — the real one is disk I/O on a worker), [teardownView] is the
   * view detach+destroy, and [awaitFlush] reports whether the worker landed
   * inside the budget.
   */
  private class FakeShutdownPort(
    /** How many calls to [submitFlush] report work to await. */
    var submissions: Int = 1,
    var completesInTime: Boolean = true,
    var failure: String? = null,
  ) : ShutdownPort {
    val log = mutableListOf<String>()
    var awaitedBudget: Long? = null
    var teardowns = 0

    override fun submitFlush(): Boolean {
      log += "submit"
      if (submissions <= 0) return false
      submissions -= 1
      return true
    }

    override fun teardownView() {
      teardowns++
      log += "teardown"
    }

    override fun awaitFlush(budgetMillis: Long): Boolean {
      awaitedBudget = budgetMillis
      log += "await"
      return completesInTime
    }

    override fun flushFailure(): String? {
      log += "failure?"
      return failure
    }
  }

  @Test
  fun `the destroy-time flush is submitted, torn down, then awaited inside a budget`() {
    val port = FakeShutdownPort()
    Assert.assertEquals(ShutdownFlush.Completed, ShutdownFlow.run(port, 250))
    // Order: the I/O is handed to the worker FIRST, the view is torn down while
    // it runs, and only then is the bounded wait performed.
    Assert.assertEquals(listOf("submit", "teardown", "await", "failure?"), port.log)
    Assert.assertEquals(250L, port.awaitedBudget)
    Assert.assertEquals(1, port.teardowns)
  }

  @Test
  fun `the default teardown budget is bounded and far below the in-app flush timeout`() {
    Assert.assertEquals(1_000L, ShutdownFlow.DESTROY_FLUSH_BUDGET_MS)
    Assert.assertTrue(
      "teardown must not wait like an in-app flush (ANR)",
      ShutdownFlow.DESTROY_FLUSH_BUDGET_MS < SiteCloseFlow.FLUSH_TIMEOUT_MS,
    )
    val port = FakeShutdownPort()
    ShutdownFlow.run(port)
    Assert.assertEquals(ShutdownFlow.DESTROY_FLUSH_BUDGET_MS, port.awaitedBudget)
  }

  @Test
  fun `a flush that outlives the budget is a timeout, never a durable shutdown`() {
    val port = FakeShutdownPort(completesInTime = false)
    Assert.assertEquals(ShutdownFlush.TimedOut, ShutdownFlow.run(port, 50))
    Assert.assertEquals("the view is torn down anyway", 1, port.teardowns)
    Assert.assertFalse(
      "a timeout must not be dressed up with a failure/success verdict",
      port.log.contains("failure?"),
    )
    Assert.assertNotEquals(ShutdownFlush.Completed, ShutdownFlow.run(port, 50))
  }

  @Test
  fun `a failed flush reports a safe class and never an exception detail`() {
    AnimeHubDebugLog.setEnabled(true)
    val privateError = "IOException: disk full at https://private.example/?token=TOP-SECRET"
    val port = FakeShutdownPort(failure = privateError)
    val outcome = ShutdownFlow.run(port, 50)
    Assert.assertTrue("a throw is a Failed, not Completed", outcome is ShutdownFlush.Failed)
    Assert.assertEquals("unknown", (outcome as ShutdownFlush.Failed).reason)
    Assert.assertEquals(1, port.teardowns)
    val exported = AnimeHubDebugLog.snapshot().toString()
    Assert.assertFalse(exported.contains("disk full"))
    Assert.assertFalse(exported.contains("private.example"))
    Assert.assertFalse(exported.contains("TOP-SECRET"))
    Assert.assertTrue(AnimeHubDebugLog.snapshot().events.any { it.errorClass == "unknown" })
  }

  @Test
  fun `no site profile means nothing to flush, but the view is still torn down`() {
    val port = FakeShutdownPort(submissions = 0)
    Assert.assertEquals(ShutdownFlush.NotNeeded, ShutdownFlow.run(port, 50))
    // The safe failure-class check distinguishes no profile from a failed profile lookup.
    Assert.assertEquals(listOf("submit", "teardown", "failure?"), port.log)
    Assert.assertNull("it never waits for work it never submitted", port.awaitedBudget)
  }

  @Test
  fun `a double teardown flushes nothing the second time and shares no state`() {
    val port = FakeShutdownPort(submissions = 1)
    Assert.assertEquals(ShutdownFlush.Completed, ShutdownFlow.run(port, 50))
    // Second destroy: the session slot was already cleared, so nothing is
    // submitted — but the view teardown still runs.
    Assert.assertEquals(ShutdownFlush.NotNeeded, ShutdownFlow.run(port, 50))
    Assert.assertEquals(2, port.teardowns)
    // Two independent calls keep independent verdicts.
    val slow = FakeShutdownPort(completesInTime = false)
    val fast = FakeShutdownPort()
    Assert.assertEquals(ShutdownFlush.TimedOut, ShutdownFlow.run(slow, 50))
    Assert.assertEquals(ShutdownFlush.Completed, ShutdownFlow.run(fast, 50))
  }

  @Test
  fun `a non-positive budget is refused instead of silently skipping the wait`() {
    for (budget in listOf(0L, -1L)) {
      val port = FakeShutdownPort()
      var thrown: Throwable? = null
      try {
        ShutdownFlow.run(port, budget)
      } catch (t: Throwable) {
        thrown = t
      }
      Assert.assertTrue("budget $budget must be refused", thrown is IllegalArgumentException)
      Assert.assertTrue("nothing may run on a refused budget", port.log.isEmpty())
    }
  }

  // --------------------------------------- profile retirement (delete protocol)

  @Test
  fun `retirement is refused for an invalid name or a profile a session is using`() {
    val name = "site-11111111111111111111111111111111-g3"
    Assert.assertNull("a derivable, inactive name may be retired", ProfileRetirement.refusal(name, null))
    Assert.assertNull(ProfileRetirement.refusal(name, "site-22222222222222222222222222222222-g3"))
    // Never address a name this app could not have derived — `Default` is the
    // launcher's own profile.
    for (bad in listOf("Default", "", "site-1111111111111111111111111111111g3", "site-ZZZZ", name + "x")) {
      Assert.assertEquals("refused: $bad", ProfileRetirement.INVALID_NAME, ProfileRetirement.refusal(bad, null))
    }
    // A profile a live session is using keeps its ledger until it is closed:
    // retiring it would unblock the very name that session is sitting on.
    Assert.assertEquals(ProfileRetirement.ACTIVE_PROFILE, ProfileRetirement.refusal(name, name))
  }

  @Test
  fun `the profile-name rule is shared, so validation and retirement cannot drift`() {
    Assert.assertTrue(SITE_PROFILE_NAME.matches("site-00000000000000000000000000000000-g0"))
    Assert.assertTrue(
      SITE_PROFILE_NAME.matches("site-abcdef0123456789abcdef0123456789-g4294967295")
    )
    for (bad in listOf(
      "site-ABCDEF0123456789ABCDEF0123456789-g0",
      "site-abcdef0123456789abcdef0123456789-g42949672951",
      "site-abcdef0123456789abcdef012345678-g0",
      "Default",
      "site-abcdef0123456789abcdef0123456789-g0 ",
    )) {
      Assert.assertFalse("must not match: $bad", SITE_PROFILE_NAME.matches(bad))
      Assert.assertEquals(ProfileRetirement.INVALID_NAME, ProfileRetirement.refusal(bad, null))
    }
  }

  @Test
  fun `a late callback on a retired name cannot touch the new generation`() {
    val old = "site-33333333333333333333333333333333-g0"
    val next = "site-33333333333333333333333333333333-g1"
    val operationId = 424242L
    // A `removeAllCookies` deletion on the OLD name whose watchdog fired: the
    // operation is still in flight, so the name is blocked.
    ProfileQuarantine.quarantine(old, operationId, SiteClearFlow.COOKIE_UNVERIFIED_REASON)
    Assert.assertTrue(ProfileQuarantine.isBlocked(old))
    Assert.assertEquals(listOf(operationId), ProfileQuarantine.pendingOperations(old))
    Assert.assertFalse(ProfileQuarantine.isBlocked(next))

    // Rust retires only after the generation bump is durable (round 9 order).
    Assert.assertEquals(1, ProfileQuarantine.retire(old))
    Assert.assertFalse(ProfileQuarantine.isBlocked(old))

    // The old deletion's late callback now settles a record that no longer
    // exists, and must neither create nor clear nor verify anything for the
    // NEW name — the generation that a fresh session will actually use.
    Assert.assertFalse("a retired operation is unknown", ProfileQuarantine.settle(old, operationId))
    ProfileQuarantine.verified(old)
    Assert.assertNull(ProfileRetirement.refusal(next, null))
    Assert.assertFalse("the new generation starts unblocked", ProfileQuarantine.isBlocked(next))
    Assert.assertEquals(emptyList<Long>(), ProfileQuarantine.pendingOperations(next))
    Assert.assertNull(SiteOpenGate.check(next))
    // Retirement is idempotent: a repeated bridge call drops nothing.
    Assert.assertEquals(0, ProfileQuarantine.retire(old))
  }

  // ------------------------------------------- adapter mapping (CookieWipes)

  @Test
  fun `the raw provider answer maps null-safely and never claims an unconfirmed deletion`() {
    Assert.assertEquals(CookieWipe.Answered(true), CookieWipes.answered(true))
    Assert.assertEquals(CookieWipe.Answered(false), CookieWipes.answered(false))
    Assert.assertEquals(
      "a null delivery is NOT a confirmed deletion",
      CookieWipe.Answered(false),
      CookieWipes.answered(null),
    )
    // So null goes down the indeterminate branch and is decided by the probe,
    // never read as "the provider deleted cookies".
    Assert.assertEquals(
      CookieStep(false, SiteClearFlow.COOKIE_STATE_UNKNOWN_REASON),
      SiteClearFlow.interpretCookieWipe(CookieWipes.answered(null)) { null },
    )
    Assert.assertEquals(
      CookieStep(true, null),
      SiteClearFlow.interpretCookieWipe(CookieWipes.answered(null)) { true },
    )
  }

  @Test
  fun `a thrown provider call and a vanished profile stay typed failures`() {
    val thrown = CookieWipes.failed(IllegalStateException("store gone"))
    Assert.assertTrue("a throw is a Failed, never an Answered", thrown is CookieWipe.Failed)
    Assert.assertEquals("provider", (thrown as CookieWipe.Failed).reason)
    // Neither the exception type nor its text enters the safe reason code.
    val messageless = CookieWipes.failed(IllegalStateException("private detail"))
    Assert.assertTrue(messageless is CookieWipe.Failed)
    Assert.assertEquals("provider", (messageless as CookieWipe.Failed).reason)

    val noProfile = CookieWipes.noProfile()
    Assert.assertTrue(noProfile is CookieWipe.Failed)
    Assert.assertEquals(CookieWipes.NO_PROFILE_REASON, (noProfile as CookieWipe.Failed).reason)

    // Neither may be readable as "the jar was empty", even when the probe would
    // have said so: the failure TYPE decides before any probe is consulted.
    for (wipe in listOf(thrown, messageless, noProfile)) {
      val step = SiteClearFlow.interpretCookieWipe(wipe) { true }
      Assert.assertFalse("must not become a success: $wipe", step.ok)
      Assert.assertTrue(step.error!!.startsWith(SiteClearFlow.COOKIE_ACCESS_FAILED_PREFIX))
    }
  }

  @Test
  fun `the emptiness probe inverts hasCookies and stays indeterminate on a throw`() {
    // `hasCookies()` asks the OPPOSITE question the flow asks, so the negation
    // is the single most dangerous line in this path: inverted twice it would
    // turn "cookies remain" into "empty → success".
    Assert.assertEquals(
      "hasCookies()=false means the jar IS empty",
      java.lang.Boolean.TRUE,
      CookieWipes.emptinessOf { false },
    )
    Assert.assertEquals(
      "hasCookies()=true means cookies remain",
      java.lang.Boolean.FALSE,
      CookieWipes.emptinessOf { true },
    )
    Assert.assertNull(
      "a throwing probe is indeterminate, never empty",
      CookieWipes.emptinessOf { throw IllegalStateException("probe boom") },
    )
  }

  @Test
  fun `the probe verdicts map onto exactly one flow decision each`() {
    fun verdict(hasCookies: () -> Boolean): CookieStep =
      SiteClearFlow.interpretCookieWipe(CookieWipes.answered(false)) {
        CookieWipes.emptinessOf(hasCookies)
      }

    Assert.assertEquals(CookieStep(true, null), verdict { false })
    Assert.assertEquals(
      CookieStep(false, SiteClearFlow.COOKIE_STILL_PRESENT_REASON),
      verdict { true },
    )
    Assert.assertEquals(
      CookieStep(false, SiteClearFlow.COOKIE_STATE_UNKNOWN_REASON),
      verdict { throw IllegalStateException("probe boom") },
    )
  }

  @Test
  fun `the adapter probe wiring decides a real clear end to end`() {
    // Same three provider store states, driven through the REAL flow with the
    // fake answering the probe exactly like the production adapter does.
    val empty = FakeClearPort(cookiesOk = false, hasCookiesProbe = { false })
    var first = "unset"
    SiteClearFlow.run(supported = true, profileName = profile, port = empty) { first = it ?: "ok" }
    Assert.assertEquals("an empty jar is a verified success", "ok", first)
    Assert.assertEquals(1, empty.emptinessChecks)
    Assert.assertNull(SiteOpenGate.check(profile))

    val full = FakeClearPort(cookiesOk = false, hasCookiesProbe = { true })
    var second = "unset"
    SiteClearFlow.run(supported = true, profileName = profile, port = full) { second = it ?: "ok" }
    Assert.assertEquals(SiteClearFlow.COOKIE_STILL_PRESENT_REASON, second)
    Assert.assertFalse("an answered wipe blocks nothing", ProfileQuarantine.isBlocked(profile))

    val broken =
      FakeClearPort(cookiesOk = false, hasCookiesProbe = { throw IllegalStateException("boom") })
    var third = "unset"
    SiteClearFlow.run(supported = true, profileName = profile, port = broken) { third = it ?: "ok" }
    Assert.assertEquals(SiteClearFlow.COOKIE_STATE_UNKNOWN_REASON, third)
    Assert.assertFalse(ProfileQuarantine.isBlocked(profile))
    Assert.assertNull("all three stay retryable", SiteOpenGate.check(profile))
  }

  @Test
  fun `a FALSE callback arriving after its timeout closes only its own record`() {
    // Round 6 ownership, round 7 answer shape: the provider finally reports
    // "no cookies were deleted" for an operation whose watchdog already fired.
    val a = FakeClearPort(cookiesNeverAnswer = true, cookieJarEmpty = false)
    var results = 0
    var last: String? = null
    SiteClearFlow.run(supported = true, profileName = profile, port = a) { results++; last = it }
    a.timeouts[0].invoke()
    Assert.assertEquals(1, results)
    Assert.assertTrue(last!!.contains("zaman aşımı"))
    val delivered = last!!
    val outstanding = ProfileQuarantine.pendingOperations(profile)
    Assert.assertEquals(1, outstanding.size)

    a.pendingCookies!!.invoke(CookieWipe.Answered(false))
    // Its OWN record is closed and nothing else happens: no second report, and
    // the late verdict (a non-empty jar) cannot overwrite the delivered timeout
    // failure either.
    Assert.assertEquals("no second result", 1, results)
    Assert.assertEquals("the timeout failure stands unchanged", delivered, last)
    Assert.assertTrue(
      "the late answer did not re-report its own verdict",
      !delivered.contains(SiteClearFlow.COOKIE_STILL_PRESENT_REASON),
    )
    Assert.assertTrue(ProfileQuarantine.pendingOperations(profile).isEmpty())
    Assert.assertTrue(
      "closing the last record still leaves the verification stale",
      ProfileQuarantine.isBlocked(profile),
    )
    Assert.assertTrue(
      SiteOpenGate.check(profile)!!.contains(ProfileQuarantine.STALE_VERIFICATION_REASON),
    )
    // A duplicate delivery of the same callback closes nothing a second time.
    a.pendingCookies!!.invoke(CookieWipe.Answered(false))
    Assert.assertEquals(1, results)
    Assert.assertTrue(ProfileQuarantine.isBlocked(profile))
    Assert.assertEquals(
      "still nobody else's record to close",
      emptyList<Long>(),
      ProfileQuarantine.pendingOperations(profile),
    )
  }

  @Test
  fun `an empty-jar success cannot lift an older outstanding deletion`() {
    // A times out; B then succeeds *because its jar was already empty* — the
    // round-7 success path. That success must not open the profile while A's
    // native deletion is still in flight.
    val a = FakeClearPort(cookiesNeverAnswer = true)
    SiteClearFlow.run(supported = true, profileName = profile, port = a) {}
    a.timeouts[0].invoke()
    val outstanding = ProfileQuarantine.pendingOperations(profile)
    Assert.assertEquals(1, outstanding.size)

    val b = FakeClearPort(cookiesOk = false, cookieJarEmpty = true)
    var bResult = "unset"
    SiteClearFlow.run(supported = true, profileName = profile, port = b) { bResult = it ?: "ok" }
    Assert.assertEquals("B is a verified success over an empty jar", "ok", bResult)
    Assert.assertTrue(
      "B's success must not erase A's outstanding operation",
      ProfileQuarantine.isBlocked(profile),
    )
    Assert.assertEquals(outstanding, ProfileQuarantine.pendingOperations(profile))
    Assert.assertNotNull("the site stays closed", SiteOpenGate.check(profile))

    // A's provider call turns out to have failed outright: the operation is no
    // longer in flight, so its record closes — but the verification (B) still
    // predates that settle.
    a.pendingCookies!!.invoke(CookieWipe.Failed("IllegalStateException: store gone"))
    Assert.assertTrue(ProfileQuarantine.pendingOperations(profile).isEmpty())
    Assert.assertTrue(ProfileQuarantine.isBlocked(profile))
    Assert.assertTrue(
      SiteOpenGate.check(profile)!!.contains(ProfileQuarantine.STALE_VERIFICATION_REASON),
    )
    // One more verified wipe after the last settle is what reopens the site.
    val c = FakeClearPort(cookiesOk = false, cookieJarEmpty = true)
    var cResult = "unset"
    SiteClearFlow.run(supported = true, profileName = profile, port = c) { cResult = it ?: "ok" }
    Assert.assertEquals("ok", cResult)
    Assert.assertFalse(ProfileQuarantine.isBlocked(profile))
    Assert.assertNull(SiteOpenGate.check(profile))
  }

  @Test
  fun `a retired profile name never blocks its new generation`() {
    // A times out on the current generation.
    val a = FakeClearPort(cookiesNeverAnswer = true)
    SiteClearFlow.run(supported = true, profileName = profile, port = a) {}
    a.timeouts[0].invoke()
    Assert.assertTrue(ProfileQuarantine.isBlocked(profile))
    val nextGeneration = "site-00000000000000000000000000000000-g8"
    Assert.assertNull(
      "the bumped generation is a different name and was never blocked",
      SiteOpenGate.check(nextGeneration),
    )

    // The provider accepts the deletion (or reports the profile absent): the
    // controller retires the name, Rust bumps the generation and never hands
    // the old name out again, so its outstanding operation cannot reach a
    // session — dropping the ledger is the defined behaviour.
    Assert.assertEquals(1, ProfileQuarantine.retire(profile))
    Assert.assertFalse(ProfileQuarantine.isBlocked(profile))
    Assert.assertEquals("retiring twice drops nothing", 0, ProfileQuarantine.retire(profile))

    // A's callback landing after retirement cannot touch the new generation.
    a.pendingCookies!!.invoke(CookieWipe.Answered(true))
    Assert.assertNull(SiteOpenGate.check(nextGeneration))
    Assert.assertFalse(ProfileQuarantine.isBlocked(nextGeneration))
    Assert.assertTrue(ProfileQuarantine.pendingOperations(nextGeneration).isEmpty())
    // The new generation opens and clears normally.
    val fresh = FakeClearPort()
    var result = "unset"
    SiteClearFlow.run(supported = true, profileName = nextGeneration, port = fresh) {
      result = it ?: "ok"
    }
    Assert.assertEquals("ok", result)
    Assert.assertNull(SiteOpenGate.check(nextGeneration))
  }

  @Test
  fun `the cookie callback answering after its timeout closes its record without re-reporting`() {
    val port = FakeClearPort(cookiesNeverAnswer = true)
    var results = 0
    var last: String? = null
    SiteClearFlow.run(supported = true, profileName = profile, port = port) { results++; last = it }
    port.timeouts[0].invoke()
    Assert.assertEquals(1, results)
    Assert.assertTrue(ProfileQuarantine.isBlocked(profile))
    // The provider finally answers, so this operation is no longer in flight —
    // but that alone does not certify the profile's end state: the last
    // verification predates the settle, so the block stays until a wipe is
    // verified after it. The delivered failure stands either way (Rust keeps
    // the encrypted blobs for the retry).
    port.pendingCookies!!.invoke(CookieWipe.Answered(true))
    Assert.assertTrue(
      "nothing outstanding any more",
      ProfileQuarantine.pendingOperations(profile).isEmpty(),
    )
    Assert.assertTrue(ProfileQuarantine.isBlocked(profile))
    Assert.assertEquals("no second result", 1, results)
    Assert.assertTrue("the timeout failure stands: $last", (last ?: "ok") != "ok")
    // A duplicate late answer changes nothing either.
    port.pendingCookies!!.invoke(CookieWipe.Answered(false))
    Assert.assertEquals(1, results)
    Assert.assertTrue(ProfileQuarantine.pendingOperations(profile).isEmpty())
    // One verified wipe after the settle reopens the site.
    val retry = FakeClearPort()
    var retryResult = "unset"
    SiteClearFlow.run(supported = true, profileName = profile, port = retry) {
      retryResult = it ?: "ok"
    }
    Assert.assertEquals("ok", retryResult)
    Assert.assertFalse(ProfileQuarantine.isBlocked(profile))
    Assert.assertNull("the gate reopens after a verified wipe", SiteOpenGate.check(profile))
  }

  @Test
  fun `a cookie wipe that ANSWERED false over a non-empty jar fails without blocking`() {
    val port = FakeClearPort(cookiesOk = false, cookieJarEmpty = false)
    var result = "unset"
    SiteClearFlow.run(supported = true, profileName = profile, port = port) { result = it ?: "ok" }
    Assert.assertNotEquals("ok", result)
    Assert.assertTrue(result.contains("hâlâ çerez var"))
    // The provider answered, so nothing is in flight: the block is for
    // UNVERIFIED destructive operations only, not for every failure.
    Assert.assertFalse(ProfileQuarantine.isBlocked(profile))
    Assert.assertNull("retry and reopen stay available", SiteOpenGate.check(profile))
  }

  @Test
  fun `a cookie wipe that ANSWERED false over an EMPTY jar is a verified success`() {
    // Chromium's `removeAllCookies` callback reports whether ANY cookie was
    // deleted, so an already-clean profile answers false for a wipe that fully
    // succeeded. Reading that as an error failed the whole clear: Rust kept the
    // encrypted session blobs and `delete_site_session` was never reached, so a
    // site the user had already cleaned could not drop its stored jar.
    val port = FakeClearPort(cookiesOk = false, cookieJarEmpty = true)
    var results = 0
    var result = "unset"
    SiteClearFlow.run(supported = true, profileName = profile, port = port) {
      results++
      result = it ?: "ok"
    }
    Assert.assertEquals("an empty jar is a clean profile", "ok", result)
    Assert.assertEquals(1, results)
    Assert.assertEquals(
      "every step still ran in order",
      listOf("exists", "cookies", "geo", "storage", "flush"),
      port.log,
    )
    Assert.assertEquals("the emptiness probe is the tie-breaker, once", 1, port.emptinessChecks)
    Assert.assertFalse(ProfileQuarantine.isBlocked(profile))
    Assert.assertNull("a verified success may back a new session", SiteOpenGate.check(profile))
  }

  @Test
  fun `a cookie wipe whose emptiness cannot be read stays a failure`() {
    // Indeterminate must never be laundered into success: claiming a wipe we
    // cannot see is the dangerous direction, a retryable failure is not.
    val port = FakeClearPort(cookiesOk = false, cookieJarEmpty = null)
    var result = "unset"
    SiteClearFlow.run(supported = true, profileName = profile, port = port) { result = it ?: "ok" }
    Assert.assertEquals(SiteClearFlow.COOKIE_STATE_UNKNOWN_REASON, result)
    Assert.assertFalse(ProfileQuarantine.isBlocked(profile))
    Assert.assertNull(SiteOpenGate.check(profile))
  }

  @Test
  fun `a cookie call that never reached the store is an error, never a clean profile`() {
    // No profile access at this step, or the provider threw: the call did not
    // reach the cookie store, so NOTHING was deleted. This is the case the old
    // boolean shape confused with an empty jar — it must stay a failure, must
    // not consult the emptiness probe, and must not be reported as success.
    val port = FakeClearPort(cookiesFailWith = "profil bu adımda erişilebilir değil")
    var results = 0
    var result = "unset"
    SiteClearFlow.run(supported = true, profileName = profile, port = port) {
      results++
      result = it ?: "ok"
    }
    Assert.assertEquals(1, results)
    Assert.assertTrue("the failure has a safe class code: $result", result.startsWith(SiteClearFlow.COOKIE_ACCESS_FAILED_PREFIX))
    Assert.assertTrue(result.endsWith(SiteClearFlow.PROVIDER_FAILURE))
    Assert.assertFalse("provider detail must not be reflected", result.contains("profil bu adımda erişilebilir değil"))
    Assert.assertEquals("no probe: the failure type already decided it", 0, port.emptinessChecks)
    Assert.assertTrue("later steps are still attempted", port.log.contains("storage"))
    // Nothing was issued, so nothing is in flight: no block, and the profile
    // stays retryable/reopenable.
    Assert.assertFalse(ProfileQuarantine.isBlocked(profile))
    Assert.assertNull(SiteOpenGate.check(profile))
  }

  @Test
  fun `a deletion the provider confirmed never consults the emptiness probe`() {
    val port = FakeClearPort(cookiesOk = true)
    var result = "unset"
    SiteClearFlow.run(supported = true, profileName = profile, port = port) { result = it ?: "ok" }
    Assert.assertEquals("ok", result)
    Assert.assertEquals(
      "no probe call, so nothing races a deletion that just completed",
      0,
      port.emptinessChecks,
    )
  }

  @Test
  fun `interpretCookieWipe decides every delivery shape exactly once`() {
    // Pure decision table of the production interpreter.
    var probes = 0
    fun probe(value: Boolean?): () -> Boolean? = { probes++; value }

    Assert.assertEquals(
      CookieStep(true, null),
      SiteClearFlow.interpretCookieWipe(CookieWipe.Answered(true), probe(true)),
    )
    Assert.assertEquals("a confirmed deletion needs no probe", 0, probes)

    Assert.assertEquals(
      CookieStep(true, null),
      SiteClearFlow.interpretCookieWipe(CookieWipe.Answered(false), probe(true)),
    )
    Assert.assertEquals(
      CookieStep(false, SiteClearFlow.COOKIE_STILL_PRESENT_REASON),
      SiteClearFlow.interpretCookieWipe(CookieWipe.Answered(false), probe(false)),
    )
    Assert.assertEquals(
      CookieStep(false, SiteClearFlow.COOKIE_STATE_UNKNOWN_REASON),
      SiteClearFlow.interpretCookieWipe(CookieWipe.Answered(false), probe(null)),
    )
    Assert.assertEquals("only the indeterminate answers probe", 3, probes)

    val failed =
      SiteClearFlow.interpretCookieWipe(
        CookieWipe.Failed("Exception: https://private.example/?token=TOP-SECRET"),
        probe(true),
      )
    Assert.assertFalse(failed.ok)
    Assert.assertEquals(
      "${SiteClearFlow.COOKIE_ACCESS_FAILED_PREFIX}: ${SiteClearFlow.PROVIDER_FAILURE}",
      failed.error,
    )
    Assert.assertFalse(failed.error!!.contains("private.example"))
    Assert.assertFalse(failed.error!!.contains("TOP-SECRET"))
    Assert.assertEquals("a typed failure never probes", 3, probes)
  }

  @Test
  fun `a final flush timeout fails the wipe but quarantines nothing`() {
    val port = FakeClearPort(flushNeverAnswers = true)
    var results = 0
    var last: String? = null
    SiteClearFlow.run(supported = true, profileName = profile, port = port) { results++; last = it }
    Assert.assertNull("pending until the flush watchdog fires", last)
    Assert.assertEquals(3, port.timeouts.size)
    port.timeouts[2].invoke()
    Assert.assertEquals(1, results)
    Assert.assertTrue("durability failure: $last", last!!.contains("kalıcılığı"))
    // flush() only persists the store that already exists: a late one cannot
    // delete or grant data a new session created, so no block is raised.
    Assert.assertFalse(ProfileQuarantine.isBlocked(profile))
    Assert.assertNull(SiteOpenGate.check(profile))
    // A late successful flush cannot flip the delivered failure either.
    port.pendingFlush!!.invoke(null)
    Assert.assertEquals(1, results)
    Assert.assertTrue((last ?: "ok") != "ok")
  }

  @Test
  fun `an absent profile is already clean and verifies the name without erasing pending work`() {
    // An earlier attempt left a destructive operation outstanding.
    val a = FakeClearPort(cookiesNeverAnswer = true)
    SiteClearFlow.run(supported = true, profileName = profile, port = a) {}
    a.timeouts[0].invoke()
    Assert.assertEquals(1, ProfileQuarantine.pendingOperations(profile).size)

    // The profile no longer exists, so the attempt succeeds without touching
    // the store — and it records a verification, NOT a blanket release.
    val port = FakeClearPort(exists = false)
    var result = "unset"
    SiteClearFlow.run(supported = true, profileName = profile, port = port) { result = it ?: "ok" }
    Assert.assertEquals("ok", result)
    Assert.assertEquals(listOf("exists"), port.log)
    Assert.assertTrue(
      "an outstanding operation of an earlier attempt survives this success",
      ProfileQuarantine.isBlocked(profile),
    )
    Assert.assertEquals(1, ProfileQuarantine.pendingOperations(profile).size)
    Assert.assertNotNull(SiteOpenGate.check(profile))
    // Once that operation settles, the absent-profile verification is stale...
    a.pendingCookies!!.invoke(CookieWipe.Answered(true))
    Assert.assertTrue(ProfileQuarantine.isBlocked(profile))
    // ...and one more verified pass (still absent) reopens the name.
    val retry = FakeClearPort(exists = false)
    var retryResult = "unset"
    SiteClearFlow.run(supported = true, profileName = profile, port = retry) {
      retryResult = it ?: "ok"
    }
    Assert.assertEquals("ok", retryResult)
    Assert.assertFalse(ProfileQuarantine.isBlocked(profile))
    Assert.assertNull(SiteOpenGate.check(profile))
  }

  @Test
  fun `an unsupported provider neither clears nor quarantines`() {
    val port = FakeClearPort()
    var result = "unset"
    SiteClearFlow.run(supported = false, profileName = profile, port = port) { result = it ?: "ok" }
    Assert.assertEquals(SiteProfileErrors.UNSUPPORTED, result)
    Assert.assertTrue(port.log.isEmpty())
    Assert.assertFalse(ProfileQuarantine.isBlocked(profile))
  }

  // ------------------------------------------------- guard / wipe units

  @Test
  fun `a step guard admits exactly one winner and stays closed afterwards`() {
    val guard = StepGuards.create()
    Assert.assertTrue(guard.open)
    Assert.assertTrue("the first claim wins", guard.claim())
    Assert.assertFalse(guard.open)
    Assert.assertFalse("every later claim loses", guard.claim())
    Assert.assertFalse(guard.claim())
    Assert.assertNotEquals(
      "operation ids are unique per step",
      guard.operationId,
      StepGuards.create().operationId,
    )
  }

  @Test
  fun `GeolocationWipe does not even enumerate once its step is settled`() {
    val guard = StepGuards.create()
    guard.claim() // the watchdog already won before the port armed the step
    val api = FakeGeolocationApi()
    var results = 0
    GeolocationWipe.start(guard, api) { results++ }
    Assert.assertEquals("no provider call at all", 0, api.getOriginsCalls)
    Assert.assertEquals(0, results)
    Assert.assertEquals(emptyList<String>(), api.cleared)
  }

  @Test
  fun `GeolocationWipe reports a failure when the provider refuses to enumerate`() {
    val api = FakeGeolocationApi(throwOnGet = true)
    var results = 0
    var last = true
    GeolocationWipe.start(StepGuards.create(), api) { ok -> results++; last = ok }
    Assert.assertEquals("the claim winner always reports", 1, results)
    Assert.assertFalse(last)
    Assert.assertEquals(emptyList<String>(), api.cleared)
  }

  @Test
  fun `GeolocationWipe reports false when a clear throws and never reports twice`() {
    val api = FakeGeolocationApi(clearThrows = true)
    var results = 0
    var last = true
    val guard = StepGuards.create()
    GeolocationWipe.start(guard, api) { ok -> results++; last = ok }
    Assert.assertEquals(1, results)
    Assert.assertFalse(last)
    Assert.assertEquals("it attempted the first origin only", 1, api.cleared.size)
    // A duplicate delivery after that is inert.
    api.deliver(setOf("https://a.example"))
    Assert.assertEquals(1, results)
    Assert.assertEquals(1, api.cleared.size)
  }

  @Test
  fun `GeolocationWipe succeeds with no stored origins and clears nothing`() {
    val api = FakeGeolocationApi(origins = emptySet())
    var results = 0
    var last = false
    GeolocationWipe.start(StepGuards.create(), api) { ok -> results++; last = ok }
    Assert.assertEquals(1, results)
    Assert.assertTrue("nothing stored is a success", last)
    Assert.assertEquals(emptyList<String>(), api.cleared)
  }

  @Test
  fun `GeolocationWipe reports a FAILURE for a null origin set`() {
    // `null` means the enumeration produced no answer, so the profile's
    // permission state is UNKNOWN. Reporting success here used to claim a
    // clean profile whose origins were never even listed.
    val api = FakeGeolocationApi(origins = null)
    var last = true
    GeolocationWipe.start(StepGuards.create(), api) { last = it }
    Assert.assertFalse("an unanswered enumeration is not a clean profile", last)
    Assert.assertEquals(emptyList<String>(), api.cleared)
  }

  @Test
  fun `an unanswered geolocation enumeration fails the whole clear`() {
    val port = FakeClearPort(geo = FakeGeolocationApi(origins = null))
    var result = "unset"
    SiteClearFlow.run(supported = true, profileName = profile, port = port) { result = it ?: "ok" }
    Assert.assertEquals("geolocation izinleri temizlenemedi", result)
    Assert.assertEquals(emptyList<String>(), port.geo.cleared)
    Assert.assertEquals(
      "the remaining steps still run",
      listOf("exists", "cookies", "geo", "storage", "flush"),
      port.log,
    )
    // Not a quarantine case: nothing was mutated and nothing is outstanding.
    Assert.assertFalse(ProfileQuarantine.isBlocked(profile))
    Assert.assertNull(SiteOpenGate.check(profile))
  }

  @Test
  fun `the quarantine ledger is per profile name and per operation`() {
    val other = "site-11111111111111111111111111111111-g0"
    ProfileQuarantine.quarantine(profile, 11L, SiteClearFlow.COOKIE_UNVERIFIED_REASON)
    ProfileQuarantine.quarantine(profile, 12L, SiteClearFlow.COOKIE_UNVERIFIED_REASON)
    Assert.assertTrue(ProfileQuarantine.isBlocked(profile))
    Assert.assertFalse("another site is unaffected", ProfileQuarantine.isBlocked(other))
    Assert.assertNull(SiteOpenGate.check(other))
    Assert.assertEquals(setOf(profile), ProfileQuarantine.names())
    Assert.assertEquals(listOf(11L, 12L), ProfileQuarantine.pendingOperations(profile))
    Assert.assertTrue(
      "the refusal names every outstanding operation",
      SiteOpenGate.check(profile)!!.contains("+1 bekleyen işlem daha"),
    )
    // settle is per-operation and refuses unknown or already-closed ids.
    Assert.assertTrue(ProfileQuarantine.settle(profile, 11L))
    Assert.assertFalse("a duplicate close is refused", ProfileQuarantine.settle(profile, 11L))
    Assert.assertFalse("an unknown id closes nothing", ProfileQuarantine.settle(profile, 99L))
    Assert.assertFalse("another name closes nothing", ProfileQuarantine.settle(other, 12L))
    Assert.assertEquals(listOf(12L), ProfileQuarantine.pendingOperations(profile))
    Assert.assertTrue(ProfileQuarantine.isBlocked(profile))
    // While an operation is outstanding its own reason is what gets reported.
    Assert.assertTrue(
      ProfileQuarantine.reasonFor(profile)!!.contains(SiteClearFlow.COOKIE_UNVERIFIED_REASON),
    )
    Assert.assertTrue(ProfileQuarantine.settle(profile, 12L))
    Assert.assertTrue(
      "no verification postdates the last settle",
      ProfileQuarantine.isBlocked(profile),
    )
    // With nothing outstanding the refusal says why the end state is unproven.
    Assert.assertTrue(
      ProfileQuarantine.reasonFor(profile)!!.contains(ProfileQuarantine.STALE_VERIFICATION_REASON),
    )
    // A verification after the last settle opens the profile, ledger included.
    ProfileQuarantine.verified(profile)
    Assert.assertFalse(ProfileQuarantine.isBlocked(profile))
    Assert.assertTrue(
      "an openable profile keeps no ledger",
      ProfileQuarantine.pendingOperations(profile).isEmpty(),
    )
    Assert.assertTrue("names() lists only blocked profiles", ProfileQuarantine.names().isEmpty())
    // verified() alone never erases somebody else's outstanding operation.
    ProfileQuarantine.quarantine(profile, 14L, SiteClearFlow.COOKIE_UNVERIFIED_REASON)
    ProfileQuarantine.verified(profile)
    Assert.assertTrue(
      "a verified wipe does not clear an outstanding operation",
      ProfileQuarantine.isBlocked(profile),
    )
    Assert.assertEquals(listOf(14L), ProfileQuarantine.pendingOperations(profile))
    // retire() is the only way outstanding records are dropped: a name Rust
    // retired for good (accepted deletion, generation bumped).
    Assert.assertEquals(1, ProfileQuarantine.retire(profile))
    Assert.assertFalse(ProfileQuarantine.isBlocked(profile))
    Assert.assertEquals("retiring an unknown name drops nothing", 0, ProfileQuarantine.retire(profile))
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

    /** How many armed watchdogs the flow cancelled after settling. */
    var cancelled = 0
    /** Arming counter, so a test can refuse one specific watchdog. */
    private var armings = 0

    /** How many armings the scheduler refused. */
    var refusals = 0

    /** Refuse every arming (`true`) or only the one with this index. */
    var armFails = false
    var refuseArmingAt: Int = -1
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

    override fun scheduleTimeout(millis: Long, onTimeout: () -> Unit): Watchdog {
      val index = armings++
      if (armFails || index == refuseArmingAt) {
        refusals++
        return Watchdog.Refused
      }
      scheduled += onTimeout
      return Watchdog.Armed({ cancelled++ })
    }
  }

  @Test
  fun `flush recovery warns early and replaces only after the hard threshold`() {
    Assert.assertFalse(FlushRecovery.shouldReplace(0L, 99_999L))
    Assert.assertFalse(FlushRecovery.shouldReplace(1_000L, 1_000L + FlushRecovery.WARN_AFTER_MS))
    Assert.assertFalse(
      FlushRecovery.shouldReplace(1_000L, 1_000L + FlushRecovery.REPLACE_AFTER_MS - 1),
    )
    Assert.assertTrue(
      FlushRecovery.shouldReplace(1_000L, 1_000L + FlushRecovery.REPLACE_AFTER_MS),
    )
    Assert.assertFalse(FlushRecovery.shouldReplace(5_000L, 1_000L))
    Assert.assertEquals(0L, FlushRecovery.stuckFor(0L, 50_000L))
    Assert.assertEquals(2_500L, FlushRecovery.stuckFor(10_000L, 12_500L))
  }

  @Test
  fun `flush worker replacement budget is bounded and refusals are explicit`() {
    // shutdownNow() cannot stop a blocked native call, so each replacement
    // leaks a thread; the budget stops the count from growing without bound.
    Assert.assertEquals(2, FlushRecovery.MAX_ABANDONED_WORKERS)
    Assert.assertTrue(FlushRecovery.replacementAllowed(0))
    Assert.assertTrue(FlushRecovery.replacementAllowed(1))
    Assert.assertFalse(FlushRecovery.replacementAllowed(2))
    Assert.assertFalse(FlushRecovery.replacementAllowed(3))
    // A negative count (caller bug) must not unlock extra threads.
    Assert.assertTrue(FlushRecovery.replacementAllowed(-5))
    Assert.assertEquals(0, FlushRecovery.workerBudgetAfter(0, false))
    Assert.assertEquals(1, FlushRecovery.workerBudgetAfter(0, true))
    Assert.assertEquals(2, FlushRecovery.workerBudgetAfter(1, true))
    // Consecutive stalls stop producing workers at the cap.
    var abandoned = 0
    var replacements = 0
    repeat(10) {
      if (FlushRecovery.replacementAllowed(abandoned)) {
        abandoned = FlushRecovery.workerBudgetAfter(abandoned, true)
        replacements += 1
      }
    }
    Assert.assertEquals(FlushRecovery.MAX_ABANDONED_WORKERS, replacements)
    Assert.assertEquals(FlushRecovery.MAX_ABANDONED_WORKERS, abandoned)
  }

  @Test
  fun `dns pre-check refuses private and changed public answers`() {
    val policy = SiteNavigationPolicy.ResolvedHostPolicy
    Assert.assertEquals(
      SiteNavigationPolicy.ResolvedHostPolicy.DnsVerdict.ALLOW,
      policy.classify(null, listOf("8.8.8.8", "1.1.1.1")),
    )
    Assert.assertEquals(
      SiteNavigationPolicy.ResolvedHostPolicy.DnsVerdict.PRIVATE_ADDRESS,
      policy.classify(null, listOf("8.8.8.8", "192.168.1.10")),
    )
    Assert.assertEquals(
      SiteNavigationPolicy.ResolvedHostPolicy.DnsVerdict.PRIVATE_ADDRESS,
      policy.classify(null, listOf("::1")),
    )
    // A public set that changes after the first check is a rebinding signal.
    Assert.assertEquals(
      SiteNavigationPolicy.ResolvedHostPolicy.DnsVerdict.CHANGED_ADDRESS,
      policy.classify(setOf("8.8.8.8"), listOf("1.1.1.1")),
    )
    // Same answer (order/duplicates irrelevant) stays allowed.
    Assert.assertEquals(
      SiteNavigationPolicy.ResolvedHostPolicy.DnsVerdict.ALLOW,
      policy.classify(setOf("8.8.8.8", "1.1.1.1"), listOf("1.1.1.1", "8.8.8.8")),
    )
    // Empty answer is fail-open at the caller; malformed entries fail closed.
    Assert.assertEquals(SiteNavigationPolicy.ResolvedHostPolicy.DnsVerdict.ALLOW, policy.classify(null, emptyList()))
    Assert.assertEquals(
      SiteNavigationPolicy.ResolvedHostPolicy.DnsVerdict.PRIVATE_ADDRESS,
      policy.classify(null, listOf("")),
    )
  }

  @Test
  fun `android blocklist mirrors the rust suffix wildcard and exact rules`() {
    val rules =
      BlocklistRules.parse(
        listOf(
          "# comment",
          "",
          "doubleclick.net",
          "*.wild.example",
          "=exact.example",
          "  MIXED.Example.  ",
        ),
      )
    Assert.assertTrue(rules.isBlocked("doubleclick.net"))
    Assert.assertTrue(rules.isBlocked("a.b.doubleclick.net"))
    Assert.assertFalse(rules.isBlocked("notdoubleclick.net"))
    Assert.assertFalse(rules.isBlocked("doubleclick.net.evil.example"))
    Assert.assertFalse(rules.isBlocked("wild.example"))
    Assert.assertTrue(rules.isBlocked("a.wild.example"))
    Assert.assertTrue(rules.isBlocked("exact.example"))
    Assert.assertFalse(rules.isBlocked("a.exact.example"))
    Assert.assertTrue(rules.isBlocked("mixed.example"))
    Assert.assertTrue(rules.isBlocked("MIXED.EXAMPLE."))
    Assert.assertEquals("::1", BlocklistRules.normalizeHost("[::1]"))
    Assert.assertFalse(rules.isBlocked(null))
    Assert.assertFalse(rules.isBlocked(""))
  }

  @Test
  fun `android blocklist snapshot can be replaced and cleared`() {
    SiteBlocklist.resetForTests()
    Assert.assertEquals(0, SiteBlocklist.size())
    Assert.assertFalse(SiteBlocklist.isBlocked("ads.example"))
    SiteBlocklist.update(listOf("ads.example"))
    Assert.assertEquals(1, SiteBlocklist.size())
    Assert.assertTrue(SiteBlocklist.isBlocked("x.ads.example"))
    SiteBlocklist.update(emptyList())
    Assert.assertFalse(SiteBlocklist.isBlocked("x.ads.example"))
    SiteBlocklist.resetForTests()
  }

  @Test
  fun `android blocklist replacement revokes the old snapshot for a live view`() {
    SiteBlocklist.resetForTests()
    SiteBlocklist.update(listOf("old-tracker.example"))
    Assert.assertTrue(SiteBlocklist.isBlocked("old-tracker.example"))

    // reset_blocklist pushes the default snapshot. A live WebView must see the
    // replacement, not keep consulting the previous process-wide rules.
    SiteBlocklist.update(listOf("new-tracker.example"))
    Assert.assertFalse(SiteBlocklist.isBlocked("old-tracker.example"))
    Assert.assertTrue(SiteBlocklist.isBlocked("new-tracker.example"))

    // The reset's disabled/default representation is an empty snapshot.
    SiteBlocklist.update(emptyList())
    Assert.assertEquals(0, SiteBlocklist.size())
    SiteBlocklist.resetForTests()
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
  fun `migration completes when all cookies were accepted and flushed`() {
    val port = FakeMigrationPort(cookieCount = 3)
    var outcome: MigrationOutcome? = null
    SiteMigrationFlow.run(listOf("a=1", "b=2", "c=3"), port) { outcome = it }
    val complete = outcome as? MigrationOutcome.Complete
    Assert.assertNotNull("expected Complete, got $outcome", complete)
    Assert.assertEquals(3, complete!!.accepted)
    Assert.assertEquals(0, complete.skipped)
    Assert.assertEquals(listOf("exists", "create", "cookie0", "cookie1", "cookie2", "flush"), port.log)
  }

  @Test
  fun `a rejected third-party cookie is skipped and the migration still completes`() {
    val port = FakeMigrationPort(acceptAll = false, cookieCount = 2)
    var outcome: MigrationOutcome? = null
    SiteMigrationFlow.run(listOf("a=1", "b=2"), port) { outcome = it }
    val complete = outcome as? MigrationOutcome.Complete
    Assert.assertNotNull("a rejection must not fail the upgrade: $outcome", complete)
    Assert.assertEquals(0, complete!!.accepted)
    Assert.assertEquals(2, complete.skipped)
    Assert.assertEquals(2, port.log.count { it.startsWith("cookie") })
    Assert.assertTrue("the profile is still flushed", port.log.contains("flush"))
  }

  @Test
  fun `a partially rejected migration keeps the accepted cookies and reports the skipped count`() {
    val port =
      object : SiteMigrationPort {
        var index = 0
        override fun profileExists(): Boolean = false
        override fun createProfile() {}
        override fun setCookie(cookie: String, onAccepted: (Boolean) -> Unit) {
          onAccepted(index++ == 0)
        }
        override fun flushCookies(onDone: (Throwable?) -> Unit) { onDone(null) }
        override fun scheduleTimeout(millis: Long, onTimeout: () -> Unit): Watchdog =
          Watchdog.Armed({})
      }
    var outcome: MigrationOutcome? = null
    SiteMigrationFlow.run(listOf("own", "foreign"), port) { outcome = it }
    val complete = outcome as? MigrationOutcome.Complete
    Assert.assertNotNull("expected Complete, got $outcome", complete)
    Assert.assertEquals(1, complete!!.accepted)
    Assert.assertEquals(1, complete.skipped)
  }

  @Test
  fun `createProfile failure fails the migration`() {
    val port = FakeMigrationPort(createThrows = true)
    var outcome: MigrationOutcome? = null
    SiteMigrationFlow.run(listOf("a=1"), port) { outcome = it }
    Assert.assertTrue(outcome is MigrationOutcome.Failed)
  }

  @Test
  fun `setCookie exception skips one entry and still flushes the rest`() {
    val port = FakeMigrationPort(setCookieThrowsAt = 1)
    var outcome: MigrationOutcome? = null
    SiteMigrationFlow.run(listOf("a=1", "b=2", "c=3"), port) { outcome = it }
    val complete = outcome as? MigrationOutcome.Complete
    Assert.assertNotNull("one malformed entry must not abort the upgrade: $outcome", complete)
    Assert.assertEquals(1, complete!!.skipped)
    Assert.assertEquals(2, port.log.count { it.startsWith("cookie") })
    Assert.assertTrue("the durable flush still ran", port.log.contains("flush"))
  }

  @Test
  fun `flush failure fails the migration even with all cookies accepted`() {
    val port = FakeMigrationPort(flushError = RuntimeException("io"))
    var outcome: MigrationOutcome? = null
    SiteMigrationFlow.run(listOf("a=1"), port) { outcome = it }
    val failed = outcome as? MigrationOutcome.Failed
    Assert.assertNotNull("flush failure must not be success, got $outcome", failed)
    Assert.assertEquals("provider", failed!!.reason)
    Assert.assertEquals("provider", failed!!.errorClass)
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

        override fun scheduleTimeout(millis: Long, onTimeout: () -> Unit): Watchdog {
          scheduled = onTimeout
          return Watchdog.Armed({})
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

        override fun scheduleTimeout(millis: Long, onTimeout: () -> Unit): Watchdog =
          Watchdog.Armed({})
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
