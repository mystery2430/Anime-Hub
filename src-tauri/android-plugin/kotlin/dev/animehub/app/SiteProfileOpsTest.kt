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

    override fun scheduleTimeout(millis: Long, onTimeout: () -> Unit) {
      timeouts += onTimeout
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

        override fun scheduleTimeout(millis: Long, onTimeout: () -> Unit) {}
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
    var cookiesOk: Boolean = true,
    var geoOk: Boolean = true,
    var storageThrows: Boolean = false,
    var flushError: Throwable? = null,
    /** The cookie wipe never answers: only its watchdog can settle the step. */
    var cookiesNeverAnswer: Boolean = false,
    /** The final flush never answers: only its watchdog can settle the wipe. */
    var flushNeverAnswers: Boolean = false,
    /** The geolocation enumeration is held so a test can deliver it LATE. */
    geoHoldsCallback: Boolean = false,
    val geo: FakeGeolocationApi = FakeGeolocationApi(hold = geoHoldsCallback),
  ) : SiteClearPort {
    val log = mutableListOf<String>()
    var timeouts = mutableListOf<() -> Unit>()

    /** The captured cookie callback, for late/duplicate delivery. */
    var pendingCookies: ((Boolean) -> Unit)? = null
    var pendingFlush: ((Throwable?) -> Unit)? = null

    override fun profileExists(): Boolean {
      log += "exists"
      return exists
    }

    override fun removeAllCookies(onDone: (Boolean) -> Unit) {
      log += "cookies"
      if (cookiesNeverAnswer) {
        pendingCookies = onDone
        return
      }
      onDone(cookiesOk)
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

    override fun scheduleTimeout(millis: Long, onTimeout: () -> Unit) {
      timeouts += onTimeout
    }
  }

  @Test
  fun `clear on an unsupported provider refuses instead of faking success`() {
    val port = FakeClearPort()
    var result = "unset"
    SiteClearFlow.run(supported = false, profileName = profile, port = port) { result = it ?: "ok" }
    Assert.assertEquals(SiteProfileErrors.UNSUPPORTED, result)
    Assert.assertTrue("no port operation may run", port.log.isEmpty())
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
  fun `cookie callback failure fails the clear but still attempts the rest`() {
    val port = FakeClearPort(cookiesOk = false)
    var result = "unset"
    SiteClearFlow.run(supported = true, profileName = profile, port = port) { result = it ?: "ok" }
    Assert.assertNotEquals("ok", result)
    Assert.assertTrue("storage must still run: ${port.log}", port.log.contains("storage"))
    Assert.assertTrue("flush must still run: ${port.log}", port.log.contains("flush"))
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
        override fun removeAllCookies(onDone: (Boolean) -> Unit) {
          onDone(true)
        }

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

        override fun scheduleTimeout(millis: Long, onTimeout: () -> Unit) {
          scheduled = onTimeout
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
      override fun scheduleTimeout(millis: Long, onTimeout: () -> Unit) {}
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
        override fun removeAllCookies(onDone: (Boolean) -> Unit) {
          onDone(true)
        }

        override fun deleteAllStorage() {}

        override fun clearGeolocationPermissions(guard: StepGuard, onDone: (Boolean) -> Unit) {
          onDone(true)
        }

        override fun flushCookies(onDone: (Throwable?) -> Unit) {
          // Never answers: only the watchdog can settle the final step.
          pendingFlush = onDone
        }

        override fun scheduleTimeout(millis: Long, onTimeout: () -> Unit) {
          timeouts += onTimeout
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
        override fun removeAllCookies(onDone: (Boolean) -> Unit) {
          onDone(true)
          onDone(false)
        }

        override fun deleteAllStorage() {}

        override fun clearGeolocationPermissions(guard: StepGuard, onDone: (Boolean) -> Unit) {
          onDone(true)
          onDone(false)
        }

        override fun flushCookies(onDone: (Throwable?) -> Unit) {
          onDone(null)
          onDone(RuntimeException("late"))
        }

        override fun scheduleTimeout(millis: Long, onTimeout: () -> Unit) {}
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

        override fun removeAllCookies(onDone: (Boolean) -> Unit) {
          onDone(true)
        }

        override fun deleteAllStorage() {}

        override fun clearGeolocationPermissions(guard: StepGuard, onDone: (Boolean) -> Unit) {
          report = onDone
          guard.claim() // wins the mutation window, then never reports
        }

        override fun flushCookies(onDone: (Throwable?) -> Unit) {
          onDone(null)
        }

        override fun scheduleTimeout(millis: Long, onTimeout: () -> Unit) {
          timeouts += onTimeout
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
    a.pendingCookies!!.invoke(true)
    val left = ProfileQuarantine.pendingOperations(profile)
    Assert.assertEquals("A closed exactly one record", 1, left.size)
    Assert.assertEquals("and it was A's, not B's", both[1], left[0])
    Assert.assertTrue(
      "B is still in flight, so the profile stays blocked",
      ProfileQuarantine.isBlocked(profile),
    )
    Assert.assertNotNull(SiteOpenGate.check(profile))

    // Only B's own answer closes B; then the verification is still stale.
    b.pendingCookies!!.invoke(true)
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

    a.pendingCookies!!.invoke(true)
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
    ports[2].pendingCookies!!.invoke(true)
    Assert.assertEquals(2, ProfileQuarantine.pendingOperations(profile).size)
    Assert.assertTrue(ProfileQuarantine.isBlocked(profile))
    ports[0].pendingCookies!!.invoke(false)
    Assert.assertEquals(1, ProfileQuarantine.pendingOperations(profile).size)
    Assert.assertTrue(ProfileQuarantine.isBlocked(profile))

    // A fully verified wipe in the middle changes nothing: one op outstanding.
    val mid = FakeClearPort()
    SiteClearFlow.run(supported = true, profileName = profile, port = mid) {}
    Assert.assertTrue(
      "a verified wipe cannot erase an outstanding operation",
      ProfileQuarantine.isBlocked(profile),
    )

    ports[1].pendingCookies!!.invoke(true)
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

    a.pendingCookies!!.invoke(true) // A settles
    a.pendingCookies!!.invoke(true) // duplicate delivery of the SAME callback
    a.pendingCookies!!.invoke(false)
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
    a.pendingCookies!!.invoke(true)
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
    port.pendingCookies!!.invoke(true)
    Assert.assertTrue(
      "nothing outstanding any more",
      ProfileQuarantine.pendingOperations(profile).isEmpty(),
    )
    Assert.assertTrue(ProfileQuarantine.isBlocked(profile))
    Assert.assertEquals("no second result", 1, results)
    Assert.assertTrue("the timeout failure stands: $last", (last ?: "ok") != "ok")
    // A duplicate late answer changes nothing either.
    port.pendingCookies!!.invoke(false)
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
  fun `a cookie wipe that ANSWERED false fails the clear without quarantining`() {
    val port = FakeClearPort(cookiesOk = false)
    var result = "unset"
    SiteClearFlow.run(supported = true, profileName = profile, port = port) { result = it ?: "ok" }
    Assert.assertNotEquals("ok", result)
    // The provider answered, so nothing is in flight: the quarantine is for
    // UNVERIFIED destructive operations only, not for every failure.
    Assert.assertFalse(ProfileQuarantine.isBlocked(profile))
    Assert.assertNull("retry and reopen stay available", SiteOpenGate.check(profile))
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
    a.pendingCookies!!.invoke(true)
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
  fun `GeolocationWipe tolerates a null origin set from the provider`() {
    val api = FakeGeolocationApi(origins = null)
    var last = false
    GeolocationWipe.start(StepGuards.create(), api) { last = it }
    Assert.assertTrue(last)
    Assert.assertEquals(emptyList<String>(), api.cleared)
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
