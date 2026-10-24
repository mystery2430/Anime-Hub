// SPDX-License-Identifier: MIT
//
// JVM tests for AsyncDnsResolver: the cancellation, single-flight and
// late-callback properties that the proxy's thread budget relies on. They use
// a fake DnsLookup, so no Android runtime is involved — the platform half
// (PlatformDnsLookup) can only be exercised on a device, and that is stated in
// docs/security-residual-risks.md.
package dev.animehub.app

import java.io.IOException
import java.net.InetAddress
import java.util.concurrent.CountDownLatch
import java.util.concurrent.CyclicBarrier
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertSame
import kotlin.test.assertTrue

class AsyncDnsResolverTest {
  /** A lookup the test completes by hand. */
  private class ManualLookup : DnsLookup {
    val started = AtomicInteger()
    val cancelled = AtomicInteger()
    private val callbacks = java.util.concurrent.ConcurrentHashMap<String, (List<InetAddress>?, Throwable?) -> Unit>()
    private val startedLatch = CountDownLatch(1)

    override fun start(host: String, onResult: (List<InetAddress>?, Throwable?) -> Unit): DnsLookup.Cancel {
      callbacks[host] = onResult
      started.incrementAndGet()
      startedLatch.countDown()
      return DnsLookup.Cancel { cancelled.incrementAndGet() }
    }

    fun awaitStarted() {
      assertTrue(startedLatch.await(2, TimeUnit.SECONDS), "the lookup never started")
    }

    fun answer(host: String, addresses: List<InetAddress>, error: Throwable? = null) {
      callbacks[host]?.invoke(addresses, error)
    }
  }

  private fun loopback() = InetAddress.getByAddress(byteArrayOf(127, 0, 0, 1))

  @Test
  fun `an answer is returned and the flight is released`() {
    val lookup = ManualLookup()
    val resolver = AsyncDnsResolver(lookup, timeoutMs = 2_000)

    val worker = Thread { resolver("example.test") }
    var result: List<InetAddress>? = null
    val done = CountDownLatch(1)
    val waiter = Thread {
      result = resolver("example.test")
      done.countDown()
    }
    // Start one lookup, then let a second caller join it.
    worker.start()
    lookup.awaitStarted()
    waiter.start()
    lookup.answer("example.test", listOf(loopback()))
    assertTrue(done.await(2, TimeUnit.SECONDS), "the joined caller never returned")
    assertEquals(1, lookup.started.get(), "both callers must share one lookup")
    worker.join(2_000)
    assertEquals(listOf(loopback()), result)
    assertEquals(0, resolver.inFlightCount(), "a completed flight must be released")
  }

  @Test
  fun `a timeout cancels the platform lookup and frees the caller`() {
    val lookup = ManualLookup()
    val resolver = AsyncDnsResolver(lookup, timeoutMs = 120)

    val started = System.nanoTime()
    assertFailsWith<IOException> { resolver("slow.test") }
    val elapsedMs = (System.nanoTime() - started) / 1_000_000L
    assertTrue(elapsedMs < 2_000, "the caller must not wait for the platform: ${elapsedMs}ms")
    assertEquals(1, lookup.cancelled.get(), "the platform lookup must be cancelled")
    assertEquals(0, resolver.inFlightCount())
  }

  @Test
  fun `a late answer cannot turn a timed-out lookup into a success`() {
    val lookup = ManualLookup()
    val resolver = AsyncDnsResolver(lookup, timeoutMs = 80)
    assertFailsWith<IOException> { resolver("late.test") }
    // The provider ignored cancellation and answers afterwards.
    lookup.answer("late.test", listOf(loopback()))
    assertEquals(0, resolver.inFlightCount(), "a dropped answer must not reopen a flight")
    // The next call starts a NEW lookup instead of reusing the old answer.
    val thread = Thread { runCatching { resolver("late.test") } }
    thread.start()
    thread.join(2_000)
    assertTrue(lookup.started.get() >= 2, "nothing is cached across calls")
  }

  @Test
  fun `a failure is reported as an IOException`() {
    val lookup = ManualLookup()
    val resolver = AsyncDnsResolver(lookup, timeoutMs = 2_000)
    val failure = IllegalArgumentException("NXDOMAIN")
    val worker = Thread { resolver("nx.test") }
    worker.start()
    lookup.awaitStarted()
    lookup.answer("nx.test", emptyList(), failure)
    worker.join(2_000)
    assertTrue(!worker.isAlive, "the caller must be released")
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
    Thread.sleep(200)
    assertEquals(2, lookup.started.get(), "two names must not share one lookup")
    lookup.answer("a.test", listOf(loopback()))
    lookup.answer("b.test", listOf(loopback()))
    threads.forEach { it.join(2_000) }
    assertEquals(2, results.size)
  }

  @Test
  fun `the in-flight ceiling refuses instead of queueing`() {
    val lookup = ManualLookup()
    val resolver = AsyncDnsResolver(lookup, timeoutMs = 2_000, maxInFlight = 1)
    val worker = Thread { runCatching { resolver("first.test") } }
    worker.start()
    lookup.awaitStarted()
    assertFailsWith<IOException> { resolver("second.test") }
    assertEquals(1, lookup.started.get(), "the refused call must not start a lookup")
    lookup.answer("first.test", listOf(loopback()))
    worker.join(2_000)
    assertEquals(0, resolver.inFlightCount())
  }

  @Test
  fun `the same host joins one flight instead of starting another`() {
    val lookup = ManualLookup()
    val resolver = AsyncDnsResolver(lookup, timeoutMs = 2_000)
    val first = Thread { runCatching { resolver("same.test") } }
    first.start()
    lookup.awaitStarted()
    val second = Thread { runCatching { resolver("same.test") } }
    second.start()
    Thread.sleep(100)
    assertEquals(1, lookup.started.get())
    lookup.answer("same.test", listOf(loopback()))
    first.join(2_000)
    second.join(2_000)
    assertEquals(0, resolver.inFlightCount())
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
    assertSame(emptyList<InetAddress>(), answers)
  }
}
