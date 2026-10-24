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
//   * two ceilings and no queue — one for the logical flights this class is
//     awaiting, one for the platform queries whose callback has not arrived
//     yet. Over either ceiling is a refusal, never a queue. The second ceiling
//     exists because `CancellationSignal.cancel()` is "used by the caller to
//     signal if the query should be cancelled" (the platform reference promises
//     neither a completion callback nor that the work has stopped), so the
//     logical ceiling alone would let a provider that never calls back start
//     one more query per timeout, without limit.
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
    // `DnsResolver.Callback<T>` is `onAnswer(answer: T, rcode: Int)` plus
    // `onError(error: DnsException)` — both parameters matter: a one-argument
    // `onAnswer` overrides nothing. The answer list arrives as a `MutableList`
    // (Java's `List` in a parameter position), so it is copied into the
    // read-only list this file returns.
    val callback =
      object : DnsResolver.Callback<MutableList<InetAddress>> {
        override fun onAnswer(answer: MutableList<InetAddress>, rcode: Int) {
          if (!settled.compareAndSet(false, true)) return
          try {
            onResult(dnsAnswerOrFailure(rcode, answer), null)
          } catch (e: IOException) {
            onResult(null, e)
          }
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
 * The one rule about `rcode`, in a form a JVM test can call.
 *
 * `DnsResolver.Callback.onAnswer` is documented as "invoked when the answer to
 * a query was successfully parsed", and `rcode` as "the response code in the
 * DNS response" (android.net.DnsResolver.Callback). Parsing and succeeding are
 * not the same thing, and the reference does not promise that the address list
 * is empty when the response code reports a failure — SERVFAIL/REFUSED answers
 * can be parsed. Only `0` (NOERROR) means "use these addresses"; anything else
 * is a resolution failure, which the proxy turns into a refusal. This is the
 * fail-closed reading of an under-specified contract, and it costs nothing:
 * ignoring a nonzero rcode could only ever *accept* an answer.
 */
internal fun dnsAnswerOrFailure(rcode: Int, answer: List<InetAddress>): List<InetAddress> {
  if (rcode != DNS_RCODE_NOERROR) throw IOException("dns rcode $rcode")
  return answer
}

/** NOERROR. The only DNS response code that means "the answer is usable". */
internal const val DNS_RCODE_NOERROR: Int = 0

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
  /** How long a platform slot is held after the app stops waiting for it. */
  private val platformGraceMs: Long = 60_000,
  /** Clock seam for the grace period; the deadline uses the latch's own wait. */
  private val nowMs: () -> Long = { System.nanoTime() / 1_000_000L },
) : (String) -> List<InetAddress> {
  /** One lookup, shared by every caller that asked for the same host. */
  private class Flight {
    /**
     * Released exactly once, when the flight settles — answer, failure or
     * cancel. The latch is also the publication edge for the fields below:
     * they are written under the lock before the count reaches zero.
     */
    val settled = CountDownLatch(1)
    var done = false
    var cancelled = false
    var answers: List<InetAddress>? = null
    var failure: Throwable? = null
    var handle: DnsLookup.Cancel? = null

    /** The platform reported something (answer, error or rcode failure). */
    var platformDone = false

    /**
     * When the platform slot may be reclaimed anyway. `Long.MAX_VALUE` while
     * the caller is still waiting: there is nothing to reclaim yet.
     */
    var reclaimAtMs = Long.MAX_VALUE
  }

  /** Guards the maps, the counters and every [Flight] field. */
  private val lock = Any()

  /** Logical flights currently awaited, one per hostname. */
  private val flights = HashMap<String, Flight>()

  /**
   * Flights whose platform query has not reported back yet, in start order.
   * Bounded by [maxInFlight]; a slot is reclaimed on a callback or after
   * [platformGraceMs] past the cancel, whichever comes first.
   */
  private val platform = LinkedHashSet<Flight>()
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
    sweep(nowMs())
    if (inFlight >= maxInFlight || platform.size >= maxInFlight) {
      // A refusal, never a queue. The proxy reports it as RESOLVE_FAILED (502):
      // it is an availability refusal, not a verdict about the host.
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
      platform.remove(flight)
      throw t
    }
    // `start` may answer synchronously: then `complete` already ran and the
    // slot was released before it was taken.
    if (!flight.platformDone) platform.add(flight)
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
      // released either way. The platform slot is NOT released here — see
      // `cancel`.
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
      // The platform query may still be running and may never call back, so
      // its slot is kept until a callback arrives or the grace period passes —
      // never awaited synchronously, which would let a silent provider wedge
      // the resolver for the life of the process.
      if (platform.contains(flight)) flight.reclaimAtMs = nowMs() + platformGraceMs
    }
    // Outside the lock: a provider is free to call back synchronously, and a
    // broken `Cancel` must not replace the caller's own verdict with its throw.
    runCatching { handle?.cancel() }
    flight.settled.countDown()
  }

  /**
   * Platform answer. A late answer for a cancelled or already-finished flight
   * is dropped here, so it can never turn a timeout into a success — but it
   * still releases the platform slot it was holding.
   */
  private fun complete(
    key: String,
    flight: Flight,
    answers: List<InetAddress>?,
    error: Throwable?,
  ) {
    synchronized(lock) {
      flight.platformDone = true
      platform.remove(flight)
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

  /** Reclaim platform slots whose grace period has passed. Lock held. */
  private fun sweep(now: Long) {
    val iterator = platform.iterator()
    while (iterator.hasNext()) {
      if (iterator.next().reclaimAtMs <= now) iterator.remove()
    }
  }

  /** Logical flights being awaited, for the tests and the coarse debug log. */
  internal fun inFlightCount(): Int = synchronized(lock) { inFlight }

  /**
   * Platform queries whose callback has not arrived yet. At most
   * [maxInFlight]; a value that stays at the ceiling means the provider is not
   * answering at all, which is exactly what the grace period bounds.
   */
  internal fun platformCount(): Int = synchronized(lock) { platform.size }
}
