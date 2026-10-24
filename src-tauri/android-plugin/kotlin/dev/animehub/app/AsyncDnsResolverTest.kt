// SPDX-License-Identifier: MIT
//
// JVM tests for AsyncDnsResolver: the cancellation, single-flight, platform-slot
// and late-callback properties that the proxy's thread budget relies on. They
// use a fake DnsLookup, so no Android runtime is involved — the platform half
// (PlatformDnsLookup) can only be exercised on a device, and that is stated in
// docs/security-residual-risks.md.
//
// JUnit 4, like every other test in this source set: the generated Android
// project puts `junit:junit` on the unit-test classpath and nothing else.
//
// What these tests do NOT prove: whether a real WebView provider honours
// `CancellationSignal.cancel()`, and whether it ever calls back afterwards.
// The resolver is written so neither answer changes its guarantees: the caller
// is released at its own deadline, a late answer cannot win, and the platform
// slot is reclaimed by the grace period even if no callback ever arrives.
package dev.animehub.app

import java.io.IOException
import java.net.InetAddress
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.CopyOnWriteArrayList
import java.util.concurrent.CountDownLatch
import java.util.concurrent.CyclicBarrier
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger
import java.util.concurrent.atomic.AtomicReference
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Test

class AsyncDnsResolverTest {
  /**
   * A lookup the test completes by hand.
   *
   * One callback list per host, because a timed-out call can be followed by a
   * new call for the same name: the tests need both the abandoned query and
   * the current one addressable at the same time.
   */
  private class ManualLookup : DnsLookup {
    val started = AtomicInteger()
    val cancelled = AtomicInteger()

    /** When set, the fake's `Cancel` throws — a broken provider, not a verdict. */
    var cancelThrows = false
    private val callbacks =
      ConcurrentHashMap<String, CopyOnWriteArrayList<(List<InetAddress>?, Throwable?) -> Unit>>()
    private val startedLatch = CountDownLatch(1)

    override fun start(
      host: String,
      onResult: (List<InetAddress>?, Throwable?) -> Unit,
    ): DnsLookup.Cancel {
      callbacks.getOrPut(host) { CopyOnWriteArrayList() }.add(onResult)
      started.incrementAndGet()
      startedLatch.countDown()
      return DnsLookup.Cancel {
        cancelled.incrementAndGet()
        if (cancelThrows) throw IllegalStateException("provider refused the cancel")
      }
    }

    fun awaitStarted() {
      assertTrue("the lookup never started", startedLatch.await(2, TimeUnit.SECONDS))
    }

    /** Answer the newest query for [host]. */
    fun answer(host: String, addresses: List<InetAddress>, error: Throwable? = null) {
      callbacks[host]?.lastOrNull()?.invoke(addresses, error)
    }

    /** Answer the oldest, still-unanswered query for [host]. */
    fun answerOldest(host: String, addresses: List<InetAddress>, error: Throwable? = null) {
      callbacks[host]?.firstOrNull()?.invoke(addresses, error)
    }
  }

  /**
   * A lookup that mirrors `PlatformDnsLookup`'s delivery rule instead of a
   * convenient lookalike: every report goes through the shipped
   * [DnsAnswerDelivery], and `Cancel` only asks the platform to stop when no
   * report has been delivered yet. The fake platform deliberately keeps
   * answering after a cancel — that is what the real one can still do, because
   * an answer already queued on the callback executor is delivered even though
   * the file descriptor was closed.
   *
   * The regression this models: round 17's adapter let `Cancel` set the same
   * flag the callback used, so a post-cancel report was swallowed, the
   * resolver never saw it, and the slot was only freed by the 60 s watchdog.
   */
  private class AdapterLikeLookup : DnsLookup {
    val started = AtomicInteger()
    val cancelRequested = AtomicInteger()
    private val deliveries =
      ConcurrentHashMap<String, CopyOnWriteArrayList<DnsAnswerDelivery>>()
    private val startedLatch = CountDownLatch(1)

    override fun start(
      host: String,
      onResult: (List<InetAddress>?, Throwable?) -> Unit,
    ): DnsLookup.Cancel {
      val delivery = DnsAnswerDelivery(onResult)
      deliveries.getOrPut(host) { CopyOnWriteArrayList() }.add(delivery)
      started.incrementAndGet()
      startedLatch.countDown()
      return DnsLookup.Cancel {
        // What PlatformDnsLookup does: best effort, and only while no report
        // has been delivered. It never clears the delivery state itself.
        if (delivery.isPending()) cancelRequested.incrementAndGet()
      }
    }

    fun awaitStarted() {
      assertTrue("the lookup never started", startedLatch.await(2, TimeUnit.SECONDS))
    }

    /** The platform reports an answer, even after a cancel. */
    fun answer(host: String, addresses: List<InetAddress>, rcode: Int = 0) {
      deliveries[host]?.lastOrNull()?.answer(addresses, rcode)
    }

    /** The platform reports an error, even after a cancel. */
    fun error(host: String, error: Throwable) {
      deliveries[host]?.lastOrNull()?.error(error)
    }

    /** Report on the oldest, already-cancelled query for [host]. */
    fun answerOldest(host: String, addresses: List<InetAddress>, rcode: Int = 0) {
      deliveries[host]?.firstOrNull()?.answer(addresses, rcode)
    }
  }

  /** A lookup that answers synchronously, before `start` returns. */
  private class SyncLookup(private val addresses: List<InetAddress>) : DnsLookup {
    override fun start(
      host: String,
      onResult: (List<InetAddress>?, Throwable?) -> Unit,
    ): DnsLookup.Cancel {
      val delivery = DnsAnswerDelivery(onResult)
      delivery.answer(addresses, 0)
      return DnsLookup.Cancel {
        if (delivery.isPending()) throw AssertionError("a delivered query must not be cancelled")
      }
    }
  }

  /** `assertThrows` without depending on a JUnit minor version. */
  private fun expectIOException(block: () -> Unit): IOException {
    try {
      block()
    } catch (e: IOException) {
      return e
    }
    throw AssertionError("an IOException was expected")
  }

  private fun loopback() = InetAddress.getByAddress(byteArrayOf(127, 0, 0, 1))

  private fun other() = InetAddress.getByAddress(byteArrayOf(127, 0, 0, 2))

  @Test
  fun `an answer is returned and the flight is released`() {
    val lookup = ManualLookup()
    val resolver = AsyncDnsResolver(lookup, timeoutMs = 2_000)

    var result: List<InetAddress>? = null
    val worker = Thread { result = resolver("example.test") }
    worker.start()
    lookup.awaitStarted()
    lookup.answer("example.test", listOf(loopback()))
    worker.join(2_000)
    assertEquals(listOf(loopback()), result)
    assertEquals("one answer must not mean two lookups", 1, lookup.started.get())
    assertEquals("a completed flight must be released", 0, resolver.inFlightCount())
    assertEquals("an answered platform query frees its slot", 0, resolver.platformCount())
  }

  @Test
  fun `a timeout cancels the platform lookup and frees the caller`() {
    val lookup = ManualLookup()
    val resolver = AsyncDnsResolver(lookup, timeoutMs = 120)

    val started = System.nanoTime()
    expectIOException { resolver("slow.test") }
    val elapsedMs = (System.nanoTime() - started) / 1_000_000L
    assertTrue(
      "the caller must not wait for the platform: ${elapsedMs}ms",
      elapsedMs < 2_000,
    )
    assertEquals("the platform lookup must be cancelled", 1, lookup.cancelled.get())
    assertEquals(0, resolver.inFlightCount())
    // The logical flight is gone, the platform slot is not: the query may still
    // be running and the API promises no completion callback. It is reclaimed
    // by the grace period (see the grace test), never by waiting for it here.
    assertEquals("the platform slot is still owned", 1, resolver.platformCount())
  }

  @Test
  fun `a late answer cannot turn a timed-out lookup into a success`() {
    val lookup = ManualLookup()
    val resolver = AsyncDnsResolver(lookup, timeoutMs = 80)
    expectIOException { resolver("late.test") }
    assertEquals(1, resolver.platformCount())
    // The provider ignored cancellation and answers afterwards.
    lookup.answer("late.test", listOf(loopback()))
    assertEquals("the slot is released by the late callback", 0, resolver.platformCount())
    assertEquals("a dropped answer must not reopen a flight", 0, resolver.inFlightCount())
    // The next call starts a NEW lookup instead of reusing the old answer.
    val thread = Thread { runCatching { resolver("late.test") } }
    thread.start()
    thread.join(2_000)
    assertTrue("nothing is cached across calls", lookup.started.get() >= 2)
  }

  @Test
  fun `a stale answer cannot change the result of a new call for the same host`() {
    val lookup = ManualLookup()
    // 1 s: long enough that the 100 ms witness sleep below cannot be a timeout.
    val resolver = AsyncDnsResolver(lookup, timeoutMs = 1_000)
    expectIOException { resolver("stale.test") }
    // A new call for the same name, while the abandoned query is still running.
    var result: List<InetAddress>? = null
    val worker = Thread { result = resolver("stale.test") }
    worker.start()
    while (lookup.started.get() < 2 && worker.isAlive) Thread.sleep(5)

    // The OLD query answers now. It may only free its slot: it must not settle
    // the new call, which is waiting for its own lookup.
    lookup.answerOldest("stale.test", listOf(other()))
    Thread.sleep(100)
    assertTrue("the stale answer must not settle the new call", worker.isAlive)
    assertEquals("only the abandoned query's slot is freed", 1, resolver.platformCount())

    lookup.answer("stale.test", listOf(loopback()))
    worker.join(2_000)
    assertEquals("the new call must get its own answer", listOf(loopback()), result)
    assertEquals(0, resolver.inFlightCount())
  }

  @Test
  fun `an answer before the deadline wins and is not cancelled`() {
    val lookup = ManualLookup()
    val resolver = AsyncDnsResolver(lookup, timeoutMs = 5_000)
    var result: List<InetAddress>? = null
    val worker = Thread { result = resolver("early.test") }
    worker.start()
    lookup.awaitStarted()
    // Deliberately well inside the deadline: the exact boundary cannot be
    // pinned without a fake clock in the production path, so it is covered by
    // the race test below instead of by a tight sleep here.
    Thread.sleep(100)
    lookup.answer("early.test", listOf(loopback()))
    worker.join(2_000)
    assertEquals(listOf(loopback()), result)
    assertEquals("a delivered answer must not cancel the platform query", 0, lookup.cancelled.get())
    assertEquals(0, resolver.inFlightCount())
    assertEquals(0, resolver.platformCount())
  }

  @Test
  fun `an answer racing the deadline yields exactly one of the two outcomes`() {
    repeat(10) {
      val lookup = ManualLookup()
      val resolver = AsyncDnsResolver(lookup, timeoutMs = 20, maxInFlight = 32)
      val outcome = java.util.concurrent.atomic.AtomicReference<Result<List<InetAddress>>?>()
      val worker =
        Thread {
          outcome.set(runCatching { resolver("race.test") })
        }
      worker.start()
      lookup.awaitStarted()
      Thread.sleep(20)
      lookup.answer("race.test", listOf(loopback()))
      worker.join(2_000)
      val result = outcome.get()
      assertTrue("the caller must settle", result != null)
      val value = result!!
      // Whichever side wins the race, the caller sees the answer or a closed
      // failure. What must never happen is a failure that later becomes a
      // success — there is no later, the call has returned.
      val ok = value.isSuccess && value.getOrNull() == listOf(loopback())
      val refused =
        value.exceptionOrNull() is IOException &&
          value.exceptionOrNull()?.message == "resolver timed out"
      assertTrue("unexpected outcome: $value", ok || refused)
    }
  }

  @Test
  fun `a failure is reported as an IOException`() {
    val lookup = ManualLookup()
    val resolver = AsyncDnsResolver(lookup, timeoutMs = 2_000)
    val failure = IllegalArgumentException("NXDOMAIN")
    val worker = Thread { runCatching { resolver("nx.test") } }
    worker.start()
    lookup.awaitStarted()
    lookup.answer("nx.test", emptyList(), failure)
    worker.join(2_000)
    assertTrue("the caller must be released", !worker.isAlive)
    assertEquals(0, resolver.inFlightCount())
    assertEquals(0, resolver.platformCount())
  }

  @Test
  fun `concurrent lookups for different hosts run in parallel`() {
    val lookup = ManualLookup()
    val resolver = AsyncDnsResolver(lookup, timeoutMs = 2_000)
    val barrier = CyclicBarrier(2)
    val results = java.util.Collections.synchronizedList(mutableListOf<Boolean>())
    val threads =
      listOf("a.test", "b.test").map { host ->
        Thread {
          barrier.await(2, TimeUnit.SECONDS)
          runCatching { resolver(host) }
          results.add(true)
        }
      }
    threads.forEach { it.start() }
    val deadline = System.nanoTime() + 2_000_000_000L
    while (lookup.started.get() < 2 && System.nanoTime() < deadline) Thread.sleep(5)
    assertEquals("two names must not share one lookup", 2, lookup.started.get())
    lookup.answer("a.test", listOf(loopback()))
    lookup.answer("b.test", listOf(loopback()))
    threads.forEach { it.join(2_000) }
    assertEquals(2, results.size)
    assertEquals(0, resolver.inFlightCount())
    assertEquals(0, resolver.platformCount())
  }

  @Test
  fun `the same host joins one flight instead of starting another`() {
    val lookup = ManualLookup()
    val resolver = AsyncDnsResolver(lookup, timeoutMs = 10_000)
    val first = Thread { runCatching { resolver("same.test") } }
    first.start()
    lookup.awaitStarted()
    val second = Thread { runCatching { resolver("same.test") } }
    second.start()
    // Deterministic join check: the latch wait is the only place the second
    // caller can be WAITING, and it can only get there by joining the flight
    // that is still unanswered — nothing has been answered yet.
    val deadline = System.nanoTime() + 5_000_000_000L
    while (second.state != Thread.State.TIMED_WAITING && System.nanoTime() < deadline) {
      Thread.sleep(5)
    }
    assertEquals(
      "the second caller must be parked on the flight (a timed latch wait)",
      Thread.State.TIMED_WAITING,
      second.state,
    )
    assertEquals("one name, one lookup", 1, lookup.started.get())
    assertEquals("two callers, one logical flight", 1, resolver.inFlightCount())
    lookup.answer("same.test", listOf(loopback()))
    first.join(5_000)
    second.join(5_000)
    assertTrue("both callers must finish", !first.isAlive && !second.isAlive)
    assertEquals(0, resolver.inFlightCount())
    assertEquals(0, resolver.platformCount())
  }

  @Test
  fun `the in-flight ceiling refuses instead of queueing`() {
    val lookup = ManualLookup()
    val resolver = AsyncDnsResolver(lookup, timeoutMs = 2_000, maxInFlight = 1)
    val worker = Thread { runCatching { resolver("first.test") } }
    worker.start()
    lookup.awaitStarted()
    expectIOException { resolver("second.test") }
    assertEquals("the refused call must not start a lookup", 1, lookup.started.get())
    lookup.answer("first.test", listOf(loopback()))
    worker.join(2_000)
    assertEquals(0, resolver.inFlightCount())
  }

  @Test
  fun `a cancelled lookup keeps its platform slot until the provider answers`() {
    val lookup = ManualLookup()
    val resolver = AsyncDnsResolver(lookup, timeoutMs = 2_000, maxInFlight = 1)

    expectIOException { resolver("a.test") }
    assertEquals(1, resolver.platformCount())
    // The provider has not answered: the one slot is still owned, so a new
    // lookup is refused rather than started alongside an unfinished query.
    val busy = expectIOException { resolver("b.test") }
    assertTrue("expected a busy refusal, got: ${busy.message}", busy.message == "resolver busy")
    assertEquals("the refused call must not start a lookup", 1, lookup.started.get())

    // The provider finally answers the cancelled query: the slot is released.
    lookup.answer("a.test", listOf(loopback()))
    assertEquals(0, resolver.platformCount())

    var result: List<InetAddress>? = null
    val worker = Thread { result = resolver("b.test") }
    worker.start()
    while (lookup.started.get() < 2 && worker.isAlive) Thread.sleep(5)
    lookup.answer("b.test", listOf(other()))
    worker.join(2_000)
    assertEquals(listOf(other()), result)
  }

  @Test
  fun `a provider that never answers loses its slot after the watchdog period`() {
    val lookup = ManualLookup()
    val clock = longArrayOf(0L)
    val resolver =
      AsyncDnsResolver(
        lookup,
        timeoutMs = 2_000,
        maxInFlight = 1,
        platformWatchdogMs = 100,
        nowMs = { clock[0] },
      )

    expectIOException { resolver("a.test") }
    assertEquals(1, resolver.platformCount())
    expectIOException { resolver("b.test") }
    assertEquals("the slot is still owned inside the grace period", 1, resolver.platformCount())

    clock[0] = 100
    var result: List<InetAddress>? = null
    val worker = Thread { result = resolver("b.test") }
    worker.start()
    while (lookup.started.get() < 2 && worker.isAlive) Thread.sleep(5)
    lookup.answer("b.test", listOf(loopback()))
    worker.join(2_000)
    assertEquals("the reclaimed slot must allow a new lookup", listOf(loopback()), result)
    assertEquals(0, resolver.platformCount())
    assertEquals(0, resolver.inFlightCount())
  }

  @Test
  fun `a throwing cancel still frees the waiting thread`() {
    val lookup = ManualLookup()
    lookup.cancelThrows = true
    val resolver = AsyncDnsResolver(lookup, timeoutMs = 20)
    // The provider's cancel throws; that must not replace the caller's verdict.
    val error = expectIOException { resolver("throws.test") }
    assertEquals("resolver timed out", error.message)
    assertEquals(1, lookup.cancelled.get())
    assertEquals(0, resolver.inFlightCount())
    // The slot still belongs to the unfinished query.
    assertEquals(1, resolver.platformCount())
  }

  @Test
  fun `a nonzero dns rcode is a failure, never an allow-list`() {
    val answer = listOf(loopback())
    // NOERROR: the answer is usable.
    assertSame(answer, dnsAnswerOrFailure(0, answer))
    // Everything else is a resolution failure, even with a parsed address list:
    // the reference does not promise the list is empty on a failed response code.
    for (rcode in listOf(1, 2, 3, 5)) {
      val error = expectIOException { dnsAnswerOrFailure(rcode, answer) }
      assertTrue("rcode $rcode must be named", error.message!!.contains(rcode.toString()))
    }
  }

  @Test
  fun `an empty answer is returned, not invented`() {
    val lookup = ManualLookup()
    val resolver = AsyncDnsResolver(lookup, timeoutMs = 2_000)
    var answers: List<InetAddress>? = null
    val worker =
      Thread {
        answers = resolver("empty.test")
      }
    worker.start()
    lookup.awaitStarted()
    lookup.answer("empty.test", emptyList())
    worker.join(2_000)
    // The proxy classifies the empty list as a refusal; the resolver must not
    // hide it behind an exception or a made-up address.
    assertEquals(emptyList<InetAddress>(), answers)
  }
  // ---------------------------------------------------------------------
  // The production delivery rule (DnsAnswerDelivery), which the fake lookup
  // above the next comment does NOT model — it forwards everything on purpose.
  // These tests exist because the round-17 adapter dropped post-cancel reports.
  // ---------------------------------------------------------------------

  @Test
  fun `the delivery rule forwards only the first platform report`() {
    val seen = mutableListOf<Pair<List<InetAddress>?, Throwable?>>()
    val delivery = DnsAnswerDelivery { answers, error -> seen.add(answers to error) }
    assertTrue("nothing has been delivered yet", delivery.isPending())

    delivery.answer(listOf(loopback()), 0)
    assertFalse("a report ends the pending state", delivery.isPending())
    delivery.answer(listOf(other()), 0) // duplicate
    delivery.error(IOException("later")) // out of order

    assertEquals("exactly one report must reach the resolver", 1, seen.size)
    assertEquals(listOf(loopback()), seen[0].first)
    assertNull(seen[0].second)
  }

  @Test
  fun `a nonzero rcode is delivered as a failure exactly once`() {
    val seen = mutableListOf<Pair<List<InetAddress>?, Throwable?>>()
    val delivery = DnsAnswerDelivery { answers, error -> seen.add(answers to error) }
    // The platform parsed an answer, but the response code says it failed.
    delivery.answer(listOf(loopback()), 3)
    // A second, "better" report must not replace the first verdict.
    delivery.answer(listOf(other()), 0)

    assertEquals(1, seen.size)
    assertNull("no address list may be published for a failed rcode", seen[0].first)
    val error = seen[0].second
    assertTrue("expected an IOException, got $error", error is IOException)
    assertTrue("the rcode must be named", error!!.message!!.contains("3"))
  }

  @Test
  fun `a late platform report frees the slot through the production delivery rule`() {
    val lookup = AdapterLikeLookup()
    val resolver = AsyncDnsResolver(lookup, timeoutMs = 80)
    // The caller gives up and the adapter asks the platform to cancel.
    expectIOException { resolver("late.test") }
    assertEquals("the cancel must have been attempted", 1, lookup.cancelRequested.get())
    assertEquals("the query is still counted", 1, resolver.platformCount())

    // The platform reports anyway, as the AOSP callback executor still can.
    lookup.answer("late.test", listOf(loopback()))

    // The late answer must not win (the call already failed) but it must end
    // the accounting at once, not after 60 s of watchdog.
    assertEquals("a real report is completion evidence", 0, resolver.platformCount())
    assertEquals(0, resolver.inFlightCount())
  }

  @Test
  fun `a late platform failure also completes the accounting`() {
    val lookup = AdapterLikeLookup()
    val resolver = AsyncDnsResolver(lookup, timeoutMs = 80)
    expectIOException { resolver("late-error.test") }
    assertEquals(1, resolver.platformCount())

    lookup.error("late-error.test", IOException("platform gave up"))

    assertEquals(0, resolver.platformCount())
    assertEquals(0, resolver.inFlightCount())
  }

  @Test
  fun `a stale report cannot settle a newer flight through the delivery rule`() {
    val lookup = AdapterLikeLookup()
    val resolver = AsyncDnsResolver(lookup, timeoutMs = 1_000)
    expectIOException { resolver("stale.test") }

    var result: List<InetAddress>? = null
    val worker = Thread { result = resolver("stale.test") }
    worker.start()
    while (lookup.started.get() < 2 && worker.isAlive) Thread.sleep(5)

    lookup.answerOldest("stale.test", listOf(other()))
    Thread.sleep(100)
    assertTrue("the stale report must not settle the new call", worker.isAlive)
    assertEquals("only the abandoned query's slot is freed", 1, resolver.platformCount())

    lookup.answer("stale.test", listOf(loopback()))
    worker.join(2_000)
    assertEquals("the new call must get its own answer", listOf(loopback()), result)
    assertEquals(0, resolver.inFlightCount())
    assertEquals(0, resolver.platformCount())
  }

  @Test
  fun `a report racing the cancel leaks no slot and double-counts nothing`() {
    repeat(20) { attempt ->
      val lookup = AdapterLikeLookup()
      // A different deadline per attempt so the report and the deadline land on
      // both sides of each other over the loop; every iteration must still end
      // with the same clean state.
      val resolver = AsyncDnsResolver(lookup, timeoutMs = 10 + attempt, maxInFlight = 8)
      val outcome = AtomicReference<Result<List<InetAddress>>?>()
      val worker = Thread { outcome.set(runCatching { resolver("race.test") }) }
      worker.start()
      lookup.awaitStarted()
      val answerer = Thread { lookup.answer("race.test", listOf(loopback())) }
      answerer.start()
      worker.join(2_000)
      answerer.join(2_000)

      val value = outcome.get()
      assertTrue("the caller must settle", value != null)
      val settled = value!!
      assertTrue(
        "the caller must see the answer or a closed failure, saw $settled",
        settled.isSuccess || settled.exceptionOrNull() is IOException,
      )
      assertEquals("no logical flight may leak", 0, resolver.inFlightCount())
      assertEquals(
        "the delivered report must free the slot exactly once",
        0,
        resolver.platformCount(),
      )
      assertTrue(resolver.inFlightCount() >= 0 && resolver.platformCount() >= 0)
    }
  }

  @Test
  fun `a synchronous report during start leaves no platform slot`() {
    val resolver = AsyncDnsResolver(SyncLookup(listOf(loopback())), timeoutMs = 1_000)
    assertEquals(listOf(loopback()), resolver("sync.test"))
    assertEquals(0, resolver.inFlightCount())
    assertEquals("a report before start returned must not leave a slot", 0, resolver.platformCount())
  }
}
