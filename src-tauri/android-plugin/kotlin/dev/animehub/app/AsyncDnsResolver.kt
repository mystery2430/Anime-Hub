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
//   * accounting that a real callback completes even after the caller gave
//     up: a late answer cannot win, but it does prove the platform query is
//     over, so the slot it held is released at once. Only a query that never
//     reports anything falls back to the watchdog (see `platformWatchdogMs`);
//   * per-host single-flight: concurrent CONNECTs for one name share one
//     lookup instead of each starting their own;
//   * two ceilings and no queue — one for the logical flights this class is
//     awaiting, one for the platform queries whose result has not been
//     observed yet. Over either ceiling is a refusal, never a queue. The
//     second one is a watchdog over what this class can observe, NOT proof
//     that the platform stopped: `CancellationSignal.cancel()` is documented
//     as "used by the caller to signal if the query should be cancelled" and
//     the reference promises neither a completion callback nor that the work
//     has stopped. The AOSP implementation (API 30 sources) removes the file
//     descriptor listener and calls `resNetworkCancel`, which closes the
//     descriptor — but it can still deliver an answer that was already queued
//     on the callback executor, and no public API reports that cancellation
//     finished.
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
   *
   * A cancelled lookup MUST still report if the platform reports: this class
   * is what decides that a late answer cannot win, and it needs the report to
   * finish its own accounting. An implementation that drops the report after
   * `Cancel.cancel()` makes a real platform completion indistinguishable from
   * a provider that never answers — that was a defect in round 17's adapter
   * and is what [DnsAnswerDelivery] now prevents.
   */
  fun start(host: String, onResult: (List<InetAddress>?, Throwable?) -> Unit): Cancel
}

/**
 * The adapter's one-shot delivery rule, kept out of [PlatformDnsLookup] so the
 * JVM tests can drive the code that actually ships.
 *
 * Two states that used to share one flag are separate here:
 *
 *   * **delivered** — the platform has reported a result, and the first report
 *     was handed to the resolver. This is the only thing that suppresses a
 *     second report (`onAnswer` then `onError`, or a duplicate answer).
 *   * **cancelled** — the caller asked the platform to stop. It is NOT a
 *     reason to drop a report, and it is not proof that the query is over.
 *
 * The round-17 adapter cancelled by setting the same flag its callback used,
 * so a cancelled query's late answer was swallowed, `AsyncDnsResolver` never
 * saw it, and the platform slot could only be reclaimed by the 60 s watchdog.
 * The JVM tests could not see that, because the fake lookup forwarded every
 * callback. Now the swallowing decision lives here, in one place the tests
 * call directly, and the rule is: deliver the platform's first report
 * whatever the cancellation state; let the resolver decide what it means.
 */
internal class DnsAnswerDelivery(
  private val onResult: (List<InetAddress>?, Throwable?) -> Unit,
) {
  private val delivered = AtomicBoolean(false)

  /** The platform answered. Only the first report reaches [onResult]. */
  fun answer(answer: List<InetAddress>, rcode: Int) {
    if (!delivered.compareAndSet(false, true)) return
    val settled =
      try {
        dnsAnswerOrFailure(rcode, answer)
      } catch (e: IOException) {
        onResult(null, e)
        return
      }
    onResult(settled, null)
  }

  /** The platform failed. Only the first report reaches [onResult]. */
  fun error(error: Throwable) {
    if (delivered.compareAndSet(false, true)) onResult(null, error)
  }

  /**
   * True while the platform has not reported yet, i.e. while asking it to
   * cancel can still change something. `false` means "a result was delivered";
   * it never means "the platform query is finished" — a query whose caller
   * cancelled and which never reports stays pending forever.
   */
  fun isPending(): Boolean = !delivered.get()
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
    // The production delivery rule; see [DnsAnswerDelivery] for why it is not
    // shared with the cancellation flag any more.
    val delivery = DnsAnswerDelivery(onResult)
    // `DnsResolver.Callback<T>` is `onAnswer(answer: T, rcode: Int)` plus
    // `onError(error: DnsException)` — both parameters matter: a one-argument
    // `onAnswer` overrides nothing. The answer list arrives as a `MutableList`
    // (Java's `List` in a parameter position), so it is copied into the
    // read-only list this file returns.
    val callback =
      object : DnsResolver.Callback<MutableList<InetAddress>> {
        override fun onAnswer(answer: MutableList<InetAddress>, rcode: Int) {
          delivery.answer(answer, rcode)
        }

        override fun onError(error: DnsResolver.DnsException) {
          delivery.error(error)
        }
      }
    try {
      // `null` network = the process default, so the system resolver, Private
      // DNS, VPN and split-DNS behaviour stay exactly what the platform
      // decides. Flags 0 = no special handling.
      DnsResolver.getInstance()
        .query(null as android.net.Network?, host, DnsResolver.FLAG_EMPTY, executor, signal, callback)
    } catch (t: Throwable) {
      delivery.error(t)
      return DnsLookup.Cancel { }
    }
    return DnsLookup.Cancel {
      // Best effort, and deliberately NOT a completion signal. `cancel()` only
      // "signals" the query (CancellationSignal.cancel); the AOSP client-side
      // implementation removes the fd listener and closes the fd, but an
      // answer already queued on the executor can still be delivered, and it
      // is forwarded by `delivery` because cancellation never clears the
      // delivery latch. A provider that refuses the cancel is handled by the
      // resolver's own deadline and watchdog.
      if (delivery.isPending()) {
        try {
          signal.cancel()
        } catch (_: Throwable) {
          // Swallowed on purpose: a broken cancel must not replace the
          // caller's own verdict with this throw.
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
  /**
   * How long a platform query stays counted after the app stops waiting for
   * it, when no report ever arrives.
   *
   * This is a **watchdog over what this class can observe**, not a measurement
   * of the platform. It is only the fallback for a query whose callback never
   * comes; a query that does report — even after the caller timed out — is
   * released by that report immediately (see [complete]). What the number does
   * NOT mean is documented on [platform]: after it passes, the query is no
   * longer counted here, and nothing in the public Android API proves that the
   * platform stopped working on it.
   */
  private val platformWatchdogMs: Long = 60_000,
  /** Clock seam for the watchdog; the deadline uses the latch's own wait. */
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
     * When this class stops counting the query even though nothing reported.
     * `Long.MAX_VALUE` while the caller is still waiting: there is nothing to
     * watch yet.
     */
    var watchdogAtMs = Long.MAX_VALUE
  }

  /** Guards the maps, the counters and every [Flight] field. */
  private val lock = Any()

  /** Logical flights currently awaited, one per hostname. */
  private val flights = HashMap<String, Flight>()

  /**
   * Flights whose platform result has not been observed yet, in start order.
   *
   * A slot leaves this set when the adapter reports (answer, failure or rcode
   * refusal) — whether or not the caller is still waiting — or when
   * [platformWatchdogMs] passes since the cancel.
   *
   * **What the ceiling does and does not mean.** It bounds the queries this
   * class is still waiting on, so it bounds the callbacks it may yet receive.
   * It does NOT prove that at most [maxInFlight] platform queries exist: the
   * watchdog only removes our own bookkeeping. The public `DnsResolver`/
   * `CancellationSignal` contract gives no completion signal, and no API
   * reports that a cancelled query has stopped, so a provider that ignores the
   * cancel and never calls back can leave work behind that this class can no
   * longer see. The AOSP client implementation (API 30 sources) closes the
   * query's file descriptor in `cancelQuery` and drops the fd listener, which
   * makes that unlikely, but it is implementation detail, not a contract —
   * and even there an answer already queued on the callback executor can still
   * be delivered. The residual is documented in
   * docs/security-residual-risks.md (round 19) and item 36–38 of the device
   * checklist; it is an availability cost, not a security decision: no address
   * from an untracked query can be used, because a report that arrives after
   * the flight settled is refused by [complete].
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
      // its slot is kept until a report arrives (which [complete] handles for
      // cancelled flights too) or the watchdog passes — never awaited
      // synchronously, which would let a silent provider wedge the resolver
      // for the life of the process. The watchdog is bookkeeping, not proof.
      if (platform.contains(flight)) flight.watchdogAtMs = nowMs() + platformWatchdogMs
    }
    // Outside the lock: a provider is free to call back synchronously, and a
    // broken `Cancel` must not replace the caller's own verdict with its throw.
    runCatching { handle?.cancel() }
    flight.settled.countDown()
  }

  /**
   * The adapter reported. A late answer for a cancelled or already-finished
   * flight is dropped here, so it can never turn a timeout into a success —
   * but it still releases the platform slot it was holding, because a report
   * is the only evidence this class has that the platform query is over.
   *
   * The adapter guarantees it calls this for the FIRST platform report even
   * after the caller cancelled (`DnsAnswerDelivery`): swallowing a post-cancel
   * report there would make a completed query indistinguishable from a silent
   * one and push every cancelled lookup onto the watchdog.
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

  /** Stop counting queries whose watchdog has passed. Lock held. */
  private fun sweep(now: Long) {
    val iterator = platform.iterator()
    while (iterator.hasNext()) {
      if (iterator.next().watchdogAtMs <= now) iterator.remove()
    }
  }

  /** Logical flights being awaited, for the tests and the coarse debug log. */
  internal fun inFlightCount(): Int = synchronized(lock) { inFlight }

  /**
   * Queries whose result has not been observed yet — the watchdog set. At most
   * [maxInFlight]; a value that stays at the ceiling means reports are not
   * arriving at all, which is what [platformWatchdogMs] bounds in *our*
   * bookkeeping. It is not a measurement of platform-side work.
   */
  internal fun platformCount(): Int = synchronized(lock) { platform.size }
}
