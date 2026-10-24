// SPDX-License-Identifier: MIT
//
// Asynchronous host resolution for the pinning proxy.
//
// WHY THIS EXISTS
//
// `InetAddress.getAllByName` is blocking, has no timeout and cannot be
// cancelled. A lookup that never returns owns its thread until the process
// dies, and the proxy's bounded thread budget (`Limits.resolveThreadCap`) is a
// ceiling on that leak, not a cure: once every generation is full the proxy
// answers `RESOLVE_EXHAUSTED` until the platform call finally returns.
//
// `android.net.DnsResolver.query(...)` (API 29+, this app's `minSdk`) hands the
// lookup to the platform asynchronously and returns a `CancellationSignal`.
// Around it this class adds what the platform contract does not guarantee:
//
//   * a deadline owned by the caller, so a thread is released even when the
//     provider ignores the cancellation;
//   * a settled flag, so a callback that arrives after the deadline cannot
//     flip a cancelled request into a success (and nothing is ever cached, so
//     it cannot be written into a stale verdict either);
//   * per-host single-flight: concurrent CONNECTs for one name share one
//     lookup instead of each starting their own;
//   * a ceiling on in-flight lookups, and no queue: over the ceiling is a
//     refusal, which the proxy maps to `RESOLVE_EXHAUSTED`.
//
// WHAT IT DOES NOT DO
//
// Nothing here decides whether an address may be used — `DnsPinnedProxy`
// classifies every answer, and an empty, failed or mixed answer is refused
// there. This file only moves the lookup off the blocking path.
package dev.animehub.app

import android.net.DnsResolver
import android.os.CancellationSignal
import java.io.IOException
import java.net.InetAddress
import java.util.concurrent.CountDownLatch
import java.util.concurrent.Executor
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean

/**
 * The platform lookup, as the resolver needs it: asynchronous, cancellable,
 * and callable from a JVM unit test without an Android runtime.
 */
internal interface DnsLookup {
  fun interface Cancel {
    /**
     * Best-effort. Providers are allowed to ignore it; the resolver's own
     * deadline is what releases the calling thread either way.
     */
    fun cancel()
  }

  /**
   * Start one lookup for [host].
   *
   * [onResult] is called at most once, with either the answers or the failure.
   * Implementations must not call it synchronously before returning when they
   * can avoid it (the resolver tolerates both, but a re-entrant call would run
   * on the caller's thread).
   */
  fun start(host: String, onResult: (List<InetAddress>?, Throwable?) -> Unit): Cancel
}

/**
 * [DnsLookup] over the platform's asynchronous resolver.
 *
 * The callback executor is a small fixed pool: completions are bookkeeping, not
 * work, and an unbounded executor here would just move the thread problem.
 */
internal class PlatformDnsLookup(
  private val executor: Executor = DEFAULT_EXECUTOR,
) : DnsLookup {
  override fun start(host: String, onResult: (List<InetAddress>?, Throwable?) -> Unit): DnsLookup.Cancel {
    val signal = CancellationSignal()
    val settled = AtomicBoolean(false)
    val callback =
      object : DnsResolver.Callback<List<InetAddress>> {
        override fun onAnswer(answer: List<InetAddress>) {
          if (settled.compareAndSet(false, true)) onResult(answer, null)
        }

        override fun onError(error: DnsResolver.DnsException) {
          if (settled.compareAndSet(false, true)) onResult(null, error)
        }
      }
    try {
      // `null` network = the process default, so the system resolver, Private
      // DNS, VPN and split-DNS behaviour stay exactly what the platform
      // decides. Flags 0 = no special handling.
      DnsResolver.getInstance()
        .query(null as android.net.Network?, host, DnsResolver.FLAG_EMPTY, executor, signal, callback)
    } catch (t: Throwable) {
      if (settled.compareAndSet(false, true)) onResult(null, t)
      return DnsLookup.Cancel { }
    }
    return DnsLookup.Cancel {
      if (settled.compareAndSet(false, true)) {
        try {
          signal.cancel()
        } catch (_: Throwable) {
          // A provider that refuses the cancel is exactly the case the
          // resolver's own deadline handles.
        }
      }
    }
  }

  private companion object {
    val DEFAULT_EXECUTOR: Executor =
      Executors.newFixedThreadPool(2) { runnable ->
        Thread(runnable, "AnimeHubDns").apply { isDaemon = true }
      }
  }
}

/**
 * The resolver the proxy uses: `(String) -> List<InetAddress>`, like the
 * blocking default it replaces, but with a deadline it can rely on.
 *
 * The coordination uses a per-flight [CountDownLatch] rather than
 * `wait`/`notifyAll`: Kotlin's `Any` does not expose the monitor methods, and a
 * `java.lang.Object` cast for them is exactly the pattern the Kotlin docs
 * discourage in favour of the concurrency utilities.
 */
internal class AsyncDnsResolver(
  private val lookup: DnsLookup,
  private val timeoutMs: Int = 10_000,
  private val maxInFlight: Int = 8,
) : (String) -> List<InetAddress> {
  /** One lookup, shared by every caller that asked for the same host. */
  private class Flight {
    /**
     * Released exactly once, when the flight settles — answer, failure or
     * cancel. The latch is also the publication edge for the three fields
     * below: they are written under the lock before the count reaches zero.
     */
    val settled = CountDownLatch(1)
    var done = false
    var cancelled = false
    var answers: List<InetAddress>? = null
    var failure: Throwable? = null
    var handle: DnsLookup.Cancel? = null
  }

  /** Guards the map, the counter and every [Flight] field. */
  private val lock = Any()

  private val flights = HashMap<String, Flight>()
  private var inFlight = 0

  override fun invoke(host: String): List<InetAddress> {
    val key = host.trim().lowercase()
    if (key.isEmpty()) throw IOException("empty host")
    val flight = synchronized(lock) { flightFor(key) }
    return await(key, flight)
  }

  /**
   * The flight for [key], starting one if nothing is in flight for it.
   *
   * Must be called with [lock] held.
   */
  private fun flightFor(key: String): Flight {
    val existing = flights[key]
    if (existing != null) {
      // Single-flight: a second CONNECT for a name already being resolved
      // waits for that lookup instead of starting another one.
      return existing
    }
    if (inFlight >= maxInFlight) {
      // A refusal, never a queue: the proxy answers RESOLVE_EXHAUSTED.
      throw IOException("resolver busy")
    }
    inFlight += 1
    val flight = Flight()
    flights[key] = flight
    try {
      // Started under the lock so a cancel racing with start cannot lose the
      // handle that makes cancellation possible.
      flight.handle = lookup.start(key) { answers, error -> complete(key, flight, answers, error) }
    } catch (t: Throwable) {
      flights.remove(key)
      inFlight -= 1
      throw t
    }
    return flight
  }

  private fun await(key: String, flight: Flight): List<InetAddress> {
    val settled =
      try {
        flight.settled.await(timeoutMs.toLong(), TimeUnit.MILLISECONDS)
      } catch (e: InterruptedException) {
        Thread.currentThread().interrupt()
        cancel(key, flight)
        throw IOException("resolver interrupted", e)
      }
    if (!settled) {
      // The provider is allowed to ignore the cancellation; the caller is
      // released either way, which is what keeps the thread budget bounded.
      cancel(key, flight)
      throw IOException("resolver timed out")
    }
    val failure = flight.failure
    if (failure != null) {
      throw failure as? IOException ?: IOException("resolver failed", failure)
    }
    // An answer list that is empty is still an answer: the proxy refuses it as
    // "no routable address", which is a different (and equally closed) verdict.
    return flight.answers ?: throw IOException("resolver answered nothing")
  }

  /** Mark [flight] cancelled, drop it from the map and cancel the platform call. */
  private fun cancel(key: String, flight: Flight) {
    val handle: DnsLookup.Cancel?
    synchronized(lock) {
      if (flight.done) return
      flight.done = true
      flight.cancelled = true
      flight.failure = IOException("resolver cancelled")
      release(key, flight)
      handle = flight.handle
    }
    // Outside the lock: a provider is free to call back synchronously.
    handle?.cancel()
    flight.settled.countDown()
  }

  /**
   * Platform answer. A late answer for a cancelled or already-finished flight
   * is dropped here, so it can never turn a timeout into a success.
   */
  private fun complete(
    key: String,
    flight: Flight,
    answers: List<InetAddress>?,
    error: Throwable?,
  ) {
    synchronized(lock) {
      if (flight.done || flight.cancelled) return
      flight.done = true
      flight.answers = answers
      flight.failure = error
      release(key, flight)
    }
    flight.settled.countDown()
  }

  /** Must be called with [lock] held. */
  private fun release(key: String, flight: Flight) {
    if (flights[key] === flight) {
      flights.remove(key)
      inFlight -= 1
    }
  }

  /** In-flight lookups, for the tests and the coarse debug log. */
  internal fun inFlightCount(): Int = synchronized(lock) { inFlight }
}
