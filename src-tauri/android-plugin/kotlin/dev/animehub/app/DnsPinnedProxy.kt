// SPDX-License-Identifier: MIT
//
// THE connection pinning boundary for Android site WebViews.
//
// WHY THIS EXISTS
// ---------------
// `NavigationDnsGate` only *predicts* which address the engine will use: it
// resolves a hostname, classifies the answer, and then the WebView resolves
// the same name again when it connects. A rebinding name can therefore answer
// a public address to the check and a private one to the socket. No amount of
// checking in `shouldInterceptRequest` can close that window, because the
// decision and the connection are made by two different components.
//
// This file moves the decision to the place the connection is made. It is a
// loopback HTTP proxy that only speaks `CONNECT` (the request a browser sends
// to a proxy for an HTTPS/WSS origin):
//
//   WebView --CONNECT host:port--> DnsPinnedProxy --resolve ONCE--> classify
//                                                       |
//                                                       +--> TCP connect to
//                                                            that InetAddress
//                                                            (no lookup)
//
// The TLS session stays between the WebView and the origin: this proxy never
// terminates TLS, never presents a certificate, never disables verification
// and never reads the plaintext. It copies bytes.
//
// FAIL-CLOSED BY CONSTRUCTION: every path that cannot prove a public address,
// or cannot open a socket to exactly that address, closes the client socket
// without a 200. There is no "allow and probe", no retry over DNS and no
// direct-connection fallback.
//
// This file is pure JVM (java.net only, no android.*) so the real decision
// paths — the CONNECT parser, the classification, the pinned connect, the
// bounded tunnel — run in CI, not only on a device.
package dev.animehub.app

import java.io.ByteArrayOutputStream
import java.io.IOException
import java.io.InputStream
import java.io.OutputStream
import java.net.InetAddress
import java.net.InetSocketAddress
import java.net.ServerSocket
import java.net.Socket
import java.util.Collections
import java.util.concurrent.Callable
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.ExecutionException
import java.util.concurrent.RejectedExecutionException
import java.util.concurrent.SynchronousQueue
import java.util.concurrent.ThreadFactory
import java.util.concurrent.ThreadPoolExecutor
import java.util.concurrent.TimeUnit
import java.util.concurrent.TimeoutException
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicInteger
import java.util.concurrent.atomic.AtomicLong

/**
 * A loopback-only HTTP `CONNECT` proxy that pins every tunnel to the address it
 * classified.
 *
 * @param limits timeouts, buffer size and the connection ceiling.
 * @param resolver the ONLY place a hostname is turned into addresses. Called
 *   exactly once per connection, and never for an IP literal.
 * @param connector opens a TCP socket to a numeric [InetAddress]. Production is
 *   [PinnedConnect]: it uses `InetSocketAddress(address, port)`, which cannot
 *   re-resolve, and then verifies the socket's own remote address.
 */
class DnsPinnedProxy(
  private val limits: Limits = Limits(),
  private val resolver: (String) -> List<InetAddress> = { host ->
    InetAddress.getAllByName(host).toList()
  },
  private val connector: (InetAddress, Int, Int) -> Socket = { address, port, timeoutMs ->
    PinnedConnect.connect(address, port, timeoutMs)
  },
  /**
   * Test seam for [start]: [before] runs immediately before each phase, and a
   * test can make that one phase fail. Production passes the no-op default, so
   * the phases themselves stay exactly what they are — what the tests measure is
   * the rollback `start()` performs when a phase throws, not a mock of `start()`.
   */
  private val startHook: StartHook = StartHook { },
) {
  /** The phases of [start], in order, that a [StartHook] can interrupt. */
  enum class StartPhase {
    /** Before the `ServerSocket` is created and bound. */
    BIND,

    /** Before the handler pool is created. */
    HANDLER_POOL,

    /** Before the tunnel (pump) pool is created. */
    PUMP_POOL,

    /**
     * Before the accept thread is started. Every field has been published by
     * then, so a failure here has to put the whole object back: this is the
     * phase that proves the rollback, not just the cleanup.
     */
    ACCEPT_THREAD,
  }

  /** Test seam: `before` is called with the phase about to run; throwing aborts. */
  fun interface StartHook {
    fun before(phase: StartPhase)
  }

  /** Why a connection was refused. Never sent to the client, never logged with a host. */
  enum class Refusal {
    /** Anything other than `CONNECT`, e.g. a cleartext HTTP request through the proxy. */
    METHOD,

    /** Malformed request line, authority, host or port. */
    TARGET,

    /** The resolver threw, timed out or answered nothing. */
    RESOLVE_FAILED,

    /**
     * Every resolver thread is stuck inside a lookup that cannot be cancelled,
     * and no further thread may be created. This is a DNS outage for the whole
     * process, not a property of the host that was asked for: nothing is
     * remembered about that host, and service returns by itself only if a
     * stuck lookup eventually returns (see [resolveThreadCap]).
     */
    RESOLVE_EXHAUSTED,

    /** A real answer, but at least one address is private/loopback/reserved. */
    NON_PUBLIC,

    /** No allowed address could be connected to. */
    CONNECT_FAILED,

    /** The socket's remote address was not the classified one: never tunnelled. */
    ADDRESS_CHANGED,

    /** The connection ceiling or the handler pool was exhausted. */
    BUSY,

    /** The `CONNECT` head never arrived (or was too large). */
    HANDSHAKE,
  }

  /** Bound ceiling: the tunnel copies in [bufferBytes] chunks, never unbounded. */
  data class Limits(
    val handshakeMs: Int = 5_000,
    val connectMs: Int = 10_000,
    /**
     * How long one `CONNECT` may spend waiting for the resolver.
     *
     * `InetAddress.getAllByName` has no timeout and cannot be cancelled, so a
     * stuck DNS answer would otherwise block a handler thread for as long as
     * the platform resolver takes (minutes, on a captive portal). The wait is
     * bounded here instead. The abandoned lookup keeps its resolver thread
     * until the platform call returns, which is why threads are counted against
     * [resolveThreadCap] (never one thread per connection).
     */
    val resolveMs: Int = 10_000,
    /**
     * Resolver threads per generation. `InetAddress.getAllByName` cannot be
     * cancelled, so a lookup that never returns owns its thread forever.
     *
     * Generations are **reused**: a lookup is submitted to the current
     * generation and, if that one is full, to any older generation that still
     * has a free thread. Only when every generation is full is another one
     * created (see [resolveThreadCap]). A rejected submission is a refusal,
     * never a queue.
     */
    val resolveThreads: Int = 4,
    /**
     * Hard ceiling on resolver threads for this proxy object, across every
     * generation **and across `stop()`/`start()` cycles**, because a platform
     * call that ignores the interrupt outlives both.
     *
     * The budget is spent on threads that can still run a lookup: `resolveThreads`
     * per live generation plus the threads of shut-down generations that are
     * still executing an abandoned call. A shut-down generation whose calls
     * have all returned cannot run anything again, so it stops counting.
     *
     * It must leave room for at least one full spare generation (enforced in
     * [Limits]), which is what gives the proxy its one automatic recovery.
     * Capacity is spent in whole generations, so a cap that is not a multiple
     * of [resolveThreads] leaves the remainder unused rather than creating a
     * half generation. When the budget is spent, [Refusal.RESOLVE_EXHAUSTED] is
     * the answer and DNS stays refused until the process restarts. **This is a
     * documented availability residual, not automatic recovery.**
     */
    val resolveThreadCap: Int = 8,
    val idleMs: Int = 180_000,
    val headerBytes: Int = 8 * 1024,
    val bufferBytes: Int = 16 * 1024,
    val maxConnections: Int = 16,
    val backlog: Int = 16,
  ) {
    init {
      validate()
    }

    companion object {
      /**
       * Largest accepted `CONNECT` head, and the largest tunnel copy buffer.
       *
       * These are not policy knobs: they bound an allocation that would
       * otherwise follow a configuration value straight into `ByteArray(...)`.
       */
      const val MAX_HEADER_BYTES: Int = 64 * 1024
      const val MAX_BUFFER_BYTES: Int = 1024 * 1024
    }

    /**
     * Reject a configuration that cannot work, **before** anything is created.
     *
     * Every value here ends up as a socket option, a pool size or an array
     * length. A zero/negative timeout or pool size either makes the proxy
     * useless or throws from inside `start()` after a socket already exists, and
     * an absurd size is an allocation an attacker-chosen default could turn into
     * an OOM. The checks are `init`-time so a bad `Limits` cannot even be
     * constructed (including through `copy()`); `start()` calls this again as a
     * cheap invariant assertion.
     */
    internal fun validate() {
      require(handshakeMs > 0) { "handshakeMs must be positive" }
      require(connectMs > 0) { "connectMs must be positive" }
      require(resolveMs > 0) { "resolveMs must be positive" }
      require(idleMs > 0) { "idleMs must be positive" }
      require(maxConnections > 0) { "maxConnections must be positive" }
      require(backlog > 0) { "backlog must be positive" }
      require(headerBytes in 1..MAX_HEADER_BYTES) {
        "headerBytes must be in 1..$MAX_HEADER_BYTES"
      }
      require(bufferBytes in 1..MAX_BUFFER_BYTES) {
        "bufferBytes must be in 1..$MAX_BUFFER_BYTES"
      }
      require(resolveThreads > 0) { "resolveThreads must be positive" }
      // `2 * resolveThreads` is the spare-generation requirement below; past
      // Int.MAX_VALUE / 2 it would overflow to a negative number and let a cap
      // that cannot fit two generations pass.
      require(resolveThreads <= Int.MAX_VALUE / 2) {
        "resolveThreads must be at most ${Int.MAX_VALUE / 2}"
      }
      // The one automatic recovery this design has is the spare generation, and
      // a cap that cannot fit it would read as a bound while behaving as
      // "one stuck generation and DNS is gone". Reject the combination instead
      // of shipping a cap that silently disables the recovery path.
      require(resolveThreadCap >= 2 * resolveThreads) {
        "resolveThreadCap must leave room for one spare generation " +
          "(${2 * resolveThreads} threads with resolveThreads = $resolveThreads)"
      }
      // Capacity is spent in whole generations, so a cap that is not a multiple
      // of resolveThreads cannot be honoured exactly. The remainder stays
      // unused: 4/10 runs two generations (8 threads) and never a half one.
      // Nothing is reserved, so nothing is lost — the docs say the remainder is
      // unused rather than pretending the bound is 10.
    }
  }

  /** What one `CONNECT` asks for. Already validated: no userinfo, no zone id. */
  data class Target(val host: String, val port: Int)

  private val lock = Any()
  private var server: ServerSocket? = null
  private var acceptThread: Thread? = null
  private var handlerPool: ThreadPoolExecutor? = null
  private var pumpPool: ThreadPoolExecutor? = null

  /**
   * The generation created for the current `start()`/`stop()` cycle. It is the
   * first one a lookup is offered to, but never the only one: older generations
   * are reused while they still have a free thread (see [submitLookup]).
   */
  @Volatile
  private var resolvePool: ThreadPoolExecutor? = null

  /**
   * Every generation this object created, in creation order, **including the
   * ones from earlier `start()`/`stop()` cycles**. The list is not cleared by
   * [stop]: a call that ignores the interrupt keeps running, so clearing the
   * list would let the next [start] spend the thread budget again on threads
   * that still exist. Generations that have terminated are dropped here.
   */
  private val resolveGenerations: MutableList<ThreadPoolExecutor> =
    Collections.synchronizedList(mutableListOf<ThreadPoolExecutor>())

  private val resolveRotation = Any()

  /**
   * Every socket this proxy owns: accepted clients and upstream connections.
   *
   * [stop] closes exactly this set. `shutdownNow()` interrupts a thread, but a
   * thread blocked in `Socket.inputStream.read()` does not respond to an
   * interrupt — it only returns when the socket is closed. Without this set a
   * stop would leave the handshake reads and both tunnel copies running, with
   * their file descriptors, until the peer or the idle timeout gave up.
   */
  private val live: MutableSet<Socket> = ConcurrentHashMap.newKeySet()

  @Volatile
  private var boundPort: Int? = null

  @Volatile
  private var running = false

  private val active = AtomicInteger(0)
  private val accepted = AtomicLong(0)
  private val resolutions = AtomicLong(0)
  private val refusals = Array(Refusal.values().size) { AtomicLong(0) }

  /** Loopback port the proxy listens on; `null` before [start] and after [stop]. */
  val port: Int?
    get() = boundPort

  /**
   * Bind `127.0.0.1:0` and start accepting. Idempotent.
   *
   * The bind address is a literal and never `0.0.0.0`. That is an **egress**
   * control, not an isolation boundary: it keeps the listener off the Wi-Fi,
   * hotspot, USB-tether and Docker interfaces, i.e. nothing outside this
   * device can reach it. It does **not** hide the port from other apps on the
   * same device. Android's loopback is shared, so any app that finds or guesses
   * the ephemeral port can send `CONNECT` to it, and `ProxyConfig` has no
   * credential mechanism with which this proxy could tell our WebViews from
   * anyone else. That residual is open and is only *bounded* here — public
   * destinations only, 16 concurrent clients, a 5 s handshake deadline, no
   * queue — never authenticated. `ProxyController` points the WebViews at it,
   * and `docs/security-residual-risks.md` (round 16.1 §2) states the same
   * thing; do not describe the loopback bind as app isolation.
   *
   * ALL-OR-NOTHING: either this returns the port the listener is accepting on,
   * or it throws and the object is exactly as it was before the call — no
   * listener, no pool, no accept thread, `port == null`, and the resolver
   * generation back on the budget. The configuration is validated first, so a
   * bad `Limits` cannot reach a socket at all, and every subsequent phase
   * (bind, handler pool, tunnel pool, accept thread) unwinds through the same
   * rollback, including on an `Error`.
   *
   * @throws IOException when the loopback listener cannot be bound **or** when
   *   no resolver generation can be created because the threads of an earlier
   *   lifecycle are still owned by calls that ignored the interrupt. Both are
   *   refusals: a proxy that cannot resolve anything must not look ready. The
   *   caller (`SiteProxyController`) maps it to `proxy-unavailable`, so the
   *   site is not opened instead of opening onto a proxy that can only fail.
   */
  fun start(): Int = synchronized(lock) {
    boundPort?.let { return it }
    // 1. Validate the configuration before a socket, a thread or a resolver
    //    generation exists. `Limits` already validates itself; this is the
    //    invariant asserted at the point of use, so a `Limits` built by any
    //    other route cannot reach `ThreadPoolExecutor(0, 0, ...)` or a negative
    //    backlog after the listener is up.
    limits.validate()

    // 2. Take the resolver generation first. A call from an earlier lifecycle
    //    that ignores the interrupt still owns its thread, so a start that
    //    cannot get a generation must not bind and must not look ready: throwing
    //    here surfaces as `proxy-unavailable`, which refuses to open the site
    //    instead of opening it onto a proxy that can only answer 502.
    val generation = try {
      newResolverGeneration()
    } catch (e: ResolverExhausted) {
      throw IOException(e.message)
    }

    // 3. Bind the listener. A failure here releases the generation and nothing
    //    else exists yet.
    val socket = ServerSocket()
    try {
      startHook.before(StartPhase.BIND)
      socket.reuseAddress = true
      socket.bind(InetSocketAddress(InetAddress.getByName(LOOPBACK), 0), limits.backlog)
    } catch (e: Exception) {
      closeQuietly(socket)
      releaseUnusedGeneration(generation)
      throw e
    }

    // 4. Everything that can still fail, with a rollback that puts the object
    //    back into "not started": no listener, no pool, no accept thread, no
    //    bound port, and the resolver generation back on the budget. A start
    //    that throws must leave nothing behind that the next start could leak
    //    or that would make the instance look ready.
    var handler: ThreadPoolExecutor? = null
    var pump: ThreadPoolExecutor? = null
    var accept: Thread? = null
    var started = false
    try {
      startHook.before(StartPhase.HANDLER_POOL)
      handler = pool("AnimeHubProxy", limits.maxConnections)
      startHook.before(StartPhase.PUMP_POOL)
      pump = pool("AnimeHubProxyTunnel", limits.maxConnections)
      accept = Thread({ acceptLoop(socket) }, "AnimeHubProxyAccept").apply { isDaemon = true }

      // Publish only once every piece exists, so a concurrent `handle()` can
      // never observe a half-built proxy.
      server = socket
      handlerPool = handler
      pumpPool = pump
      resolvePool = generation
      boundPort = socket.localPort
      running = true
      acceptThread = accept
      // Last chance to fail: everything above is already visible on the object,
      // so the rollback below is what makes the failed start look untouched.
      startHook.before(StartPhase.ACCEPT_THREAD)
      accept.start()
      started = true
      return socket.localPort
    } finally {
      // `finally`, not `catch (Exception)`: an Error thrown by a phase (an OOM
      // while creating a pool, for instance) must unwind through the same
      // rollback, or the listener and the generation would leak.
      if (!started) {
        running = false
        server = null
        handlerPool = null
        pumpPool = null
        resolvePool = null
        acceptThread = null
        boundPort = null
        closeQuietly(socket)
        handler?.shutdownNow()
        pump?.shutdownNow()
        // An accept thread that was created but never started owns nothing; one
        // that did start is stopped by the listener close above plus `running`.
        accept?.interrupt()
        releaseUnusedGeneration(generation)
      }
    }
  }

  /**
   * Put a generation that never served a lookup back on the budget.
   *
   * Shutting it down is enough to make it terminate (it has no workers yet), and
   * discarding terminated generations is what returns the threads to the cap.
   * A generation that somehow still has a lookup inside it is *not* discarded:
   * [liveResolverThreads] keeps counting its `activeCount`, because that call
   * cannot be reclaimed. Nothing here pretends a stuck platform call is gone.
   */
  private fun releaseUnusedGeneration(generation: ThreadPoolExecutor) {
    generation.shutdownNow()
    discardTerminatedGenerations()
  }

  /**
   * Stop accepting and close every live connection. Safe to call twice.
   *
   * `shutdownNow()` only interrupts idle workers; a worker inside
   * `InputStream.read()` or `Socket.connect()` stays there until its socket
   * changes state. So the interrupt is followed by closing every tracked
   * socket, which is what actually ends the handshake reads and both tunnel
   * copies. Callers therefore observe EOF promptly, and no descriptor or
   * handler slot survives the stop.
   */
  fun stop() = synchronized(lock) {
    running = false
    closeQuietly(server)
    server = null
    acceptThread?.interrupt()
    acceptThread = null
    handlerPool?.shutdownNow()
    handlerPool = null
    pumpPool?.shutdownNow()
    pumpPool = null
    resolvePool = null
    // Every generation, including the abandoned ones: a stuck lookup keeps its
    // thread until the platform call returns, and interrupting is the only
    // thing that can be done about it. The rotation lock is held so a handler
    // that is rotating right now cannot add a generation after this loop — it
    // re-reads `running` (already false) and refuses instead.
    //
    // The list is deliberately NOT cleared: those threads still exist, so a
    // following [start] has to see them in the budget. A generation that has
    // really terminated is dropped on the next accounting pass.
    synchronized(resolveRotation) {
      synchronized(resolveGenerations) {
        for (generation in resolveGenerations) generation.shutdownNow()
      }
    }
    // Snapshot-free iteration on purpose: `live.toList()` walks the key set of
    // a ConcurrentHashMap, and `next()` there can throw when a handler removes
    // its own socket between `hasNext()` and `next()`. A stop must close every
    // descriptor it can, so a vanished entry is skipped instead of aborting the
    // loop halfway (the handler that removed it already closed it).
    val sockets = live.iterator()
    while (true) {
      val socket = try {
        if (!sockets.hasNext()) break
        sockets.next()
      } catch (_: java.util.NoSuchElementException) {
        continue
      }
      closeQuietly(socket)
    }
    live.clear()
    boundPort = null
  }

  /** Diagnostics and tests: connections currently inside [handle]. */
  fun activeConnections(): Int = active.get()

  /** Diagnostics and tests: every `CONNECT` this proxy accepted. */
  fun acceptedConnections(): Long = accepted.get()

  /**
   * Diagnostics and tests: how many times a hostname was resolved.
   *
   * This is the invariant that closes the TOCTOU window — one resolution per
   * connection, and the socket is opened from that answer.
   */
  fun resolutionCount(): Long = resolutions.get()

  fun refusalCount(reason: Refusal): Long = refusals[reason.ordinal].get()

  /**
   * Diagnostics and tests: generations still tracked by this object.
   *
   * It includes shut-down generations whose calls are still running (they are
   * part of the thread budget) and drops the ones that really terminated. A
   * failed `start()` must leave this at zero.
   */
  internal fun resolverGenerationCount(): Int =
    synchronized(resolveGenerations) { resolveGenerations.size }

  // -------------------------------------------------------------------------
  // Accept loop
  // -------------------------------------------------------------------------

  private fun acceptLoop(socket: ServerSocket) {
    while (running) {
      val client = try {
        socket.accept()
      } catch (_: IOException) {
        // `stop()` closed the listener; if it did not, keep accepting.
        if (!running) return
        continue
      }
      // Tracked from the first moment: a connection that is still sending its
      // head when `stop()` runs must be closed too, or the handshake read
      // would sit there for the whole handshake timeout.
      track(client)
      val pool = handlerPool
      if (pool == null) {
        refuseBusy(client)
        continue
      }
      if (active.incrementAndGet() > limits.maxConnections) {
        active.decrementAndGet()
        refuseBusy(client)
        continue
      }
      try {
        pool.execute { handle(client) }
      } catch (_: RejectedExecutionException) {
        active.decrementAndGet()
        refuseBusy(client)
      }
    }
  }

  // -------------------------------------------------------------------------
  // One connection
  // -------------------------------------------------------------------------

  private fun handle(client: Socket) {
    var remote: Socket? = null
    var established = false
    try {
      accepted.incrementAndGet()
      client.soTimeout = limits.handshakeMs
      val head = readHead(client.getInputStream()) ?: run {
        refuse(client, Refusal.HANDSHAKE)
        return
      }
      val target = parseConnectHead(head) ?: run {
        // A well-formed request that is not CONNECT is answered 405, so a
        // cleartext HTTP request through the proxy dies here and nowhere else.
        refuse(
          client,
          if (isConnectMethod(head)) Refusal.TARGET else Refusal.METHOD,
        )
        return
      }

      val addresses = try {
        resolveOnce(target.host)
      } catch (_: ResolverExhausted) {
        // Not a verdict about this host: the process has no resolver thread
        // left that a lookup could run on.
        refuse(client, Refusal.RESOLVE_EXHAUSTED)
        return
      } catch (_: Exception) {
        refuse(client, Refusal.RESOLVE_FAILED)
        return
      }
      // Every address in the answer must be public: a mixed answer is refused
      // as a whole, never "the public half".
      val allowed = ArrayList<InetAddress>(addresses.size)
      for (address in addresses) {
        val text = address.hostAddress?.substringBefore('%') ?: run {
          refuse(client, Refusal.RESOLVE_FAILED)
          return
        }
        if (!IpPolicy.isPublicIp(text)) {
          refuse(client, Refusal.NON_PUBLIC)
          return
        }
        allowed.add(address)
      }
      if (allowed.isEmpty()) {
        refuse(client, Refusal.RESOLVE_FAILED)
        return
      }

      // One tunnel attempt per allowed address, each pinned to that address.
      var mismatch = false
      for (address in allowed) {
        val socket = try {
          connector(address, target.port, limits.connectMs)
        } catch (_: Exception) {
          continue
        }
        if (!sameAddress(socket, address)) {
          mismatch = true
          closeQuietly(socket)
          continue
        }
        remote = socket
        track(socket)
        break
      }
      if (remote == null) {
        refuse(client, if (mismatch) Refusal.ADDRESS_CHANGED else Refusal.CONNECT_FAILED)
        return
      }

      writeConstant(client, ESTABLISHED)
      established = true
      tunnel(client, remote)
    } catch (_: Exception) {
      // Any failure before the 200: the client socket is closed below and the
      // request simply fails. Nothing is retried and nothing goes direct.
      // After the 200 the client is already talking TLS, so writing a status
      // line there would corrupt a tunnel that is being torn down anyway.
      if (!established) refuse(client, Refusal.HANDSHAKE)
    } finally {
      drop(remote)
      drop(client)
      active.decrementAndGet()
    }
  }

  /**
   * Resolve [host] exactly once, with a bounded wait. IP literals are parsed,
   * never resolved.
   *
   * A timeout is a REFUSAL, never a verdict: the abandoned lookup may still be
   * running inside the platform resolver (there is no cancellation API), but
   * its answer is dropped, no address is classified from it, and no socket is
   * opened for it. A later attempt for the same name resolves again instead of
   * reusing the failure — there is no cache here in either direction, so
   * neither a broken answer nor a good one can become a stale public verdict.
   */
  private fun resolveOnce(host: String): List<InetAddress> {
    literal(host)?.let { return listOf(it) }
    resolutions.incrementAndGet()
    val future = submitLookup(host)
    val answer = try {
      future.get(limits.resolveMs.toLong(), TimeUnit.MILLISECONDS)
    } catch (_: TimeoutException) {
      // Best effort only: `cancel(true)` cannot interrupt a blocking
      // `getAllByName`. The task keeps its (bounded) resolver thread, and its
      // answer is dropped even if it arrives later — nothing is read from a
      // future that already timed out.
      future.cancel(true)
      throw IOException("resolver timed out")
    } catch (e: ExecutionException) {
      throw IOException("resolver failed", e.cause)
    } catch (e: InterruptedException) {
      future.cancel(true)
      Thread.currentThread().interrupt()
      throw IOException("interrupted")
    }
    if (answer.isEmpty()) throw IOException("empty answer")
    return answer
  }

  /**
   * Submit one lookup, reusing any generation that still has a free thread.
   *
   * A generation whose threads are all owned by an uncancellable call rejects
   * the submission (`SynchronousQueue`, so nothing queues). That rejection says
   * nothing about the *other* generations: a wedged generation can become
   * usable again the moment one of its calls returns, and an older one may be
   * idle while the current one is full. So the lookup is offered to the current
   * generation first and then to every older generation, and a new generation
   * is created only when all of them are full — at most once per call, and only
   * while [Limits.resolveThreadCap] allows it.
   *
   * Reuse changes nothing about the guarantee: each submission gets its OWN
   * future, its answer is read only by the caller that submitted it, a timed-out
   * future is cancelled and never read again, and no answer is cached. When no
   * generation can take the lookup, [ResolverExhausted] is thrown and the
   * caller refuses the connection.
   */
  private fun submitLookup(host: String): java.util.concurrent.Future<List<InetAddress>> {
    var rotated = false
    while (true) {
      val current = resolvePool ?: throw IOException("proxy not started")
      for (generation in generationOrder(current)) {
        if (generation.isShutdown) continue
        try {
          return generation.submit(Callable { resolver(host) })
        } catch (_: RejectedExecutionException) {
          // This generation is at its thread limit; try the next one.
        }
      }
      if (rotated || !rotateResolver(current)) throw ResolverExhausted()
      rotated = true
    }
  }

  /**
   * The current generation first, then every older one in creation order.
   *
   * `stop()` leaves the list in place for accounting, so shut-down generations
   * can still be in it; they are filtered by the caller.
   */
  private fun generationOrder(current: ThreadPoolExecutor): List<ThreadPoolExecutor> =
    synchronized(resolveGenerations) {
      val ordered = ArrayList<ThreadPoolExecutor>(resolveGenerations.size + 1)
      ordered.add(current)
      for (generation in resolveGenerations) {
        if (generation !== current) ordered.add(generation)
      }
      ordered
    }

  /** True when [stuck] was replaced by a fresh generation, so the caller retries. */
  private fun rotateResolver(stuck: ThreadPoolExecutor): Boolean = synchronized(resolveRotation) {
    // Another handler may already have rotated while this one was rejected; the
    // retry then re-reads the generation order instead of creating a second one.
    if (resolvePool !== stuck) return true
    if (!running) return false
    val generation = try {
      newResolverGeneration()
    } catch (_: ResolverExhausted) {
      return false
    }
    resolvePool = generation
    return true
  }

  /**
   * Threads that can still be running a lookup.
   *
   * A live generation is counted at its capacity: that is what it may still
   * spawn, and counting capacity rather than the current `poolSize` keeps the
   * bound true for a thread that is being created right now. A shut-down
   * generation is counted at its `activeCount` — the calls that are still
   * running and cannot be reclaimed — because an idle worker in a shut-down
   * pool has nothing left to execute and exits on its own.
   *
   * The distinction matters across `stop()`/`start()`: the stuck call from the
   * previous lifecycle is counted, an idle thread that is merely exiting is not.
   */
  private fun liveResolverThreads(): Int = synchronized(resolveGenerations) {
    var live = 0
    for (generation in resolveGenerations) {
      live += if (generation.isShutdown) generation.activeCount else limits.resolveThreads
    }
    live
  }

  private fun discardTerminatedGenerations() {
    synchronized(resolveGenerations) {
      resolveGenerations.removeAll { it.isTerminated }
    }
  }

  /**
   * Create a generation if the budget allows it, otherwise [ResolverExhausted].
   *
   * Callers hold [resolveRotation] when this is a rotation; `start()` calls it
   * for the first generation of a lifecycle.
   */
  private fun newResolverGeneration(): ThreadPoolExecutor = synchronized(resolveRotation) {
    discardTerminatedGenerations()
    if (liveResolverThreads() + limits.resolveThreads > limits.resolveThreadCap) {
      throw ResolverExhausted()
    }
    val generation = pool("AnimeHubProxyResolve", limits.resolveThreads)
    resolveGenerations.add(generation)
    generation
  }

  /** Internal signal: no generation can take a lookup and none may be created. */
  private class ResolverExhausted : IOException("resolver thread budget exhausted")

  // -------------------------------------------------------------------------
  // Tunnel
  // -------------------------------------------------------------------------

  private fun tunnel(client: Socket, remote: Socket) {
    client.soTimeout = limits.idleMs
    remote.soTimeout = limits.idleMs
    val closed = AtomicBoolean(false)
    val pool = pumpPool
    if (pool == null) throw IOException("proxy stopped")
    try {
      pool.execute { copy(client, remote, closed, fromRemote = true) }
    } catch (_: RejectedExecutionException) {
      throw IOException("tunnel pool exhausted")
    }
    copy(client, remote, closed, fromRemote = false)
  }

  /**
   * Copy one direction until EOF, error or close, then close BOTH sockets.
   *
   * Closing both is what makes the teardown symmetric: whichever side hangs up
   * first — the page, the origin, an idle timeout — the other copy unblocks
   * with a closed socket instead of holding a thread and a file descriptor.
   * TLS is untouched: these are opaque bytes in both directions.
   */
  private fun copy(
    client: Socket,
    remote: Socket,
    closed: AtomicBoolean,
    fromRemote: Boolean,
  ) {
    try {
      // The streams are taken INSIDE the try: a peer that vanished between the
      // 200 and this line would otherwise throw out of the pump task itself,
      // escaping the `finally` below and leaking both sockets.
      val from = if (fromRemote) remote.getInputStream() else client.getInputStream()
      val to = if (fromRemote) client.getOutputStream() else remote.getOutputStream()
      val buffer = ByteArray(limits.bufferBytes)
      while (!closed.get()) {
        val read = from.read(buffer)
        if (read < 0) return
        to.write(buffer, 0, read)
        to.flush()
      }
    } catch (_: Exception) {
      // EOF, reset, timeout, no stream: all of them end the tunnel.
    } finally {
      if (closed.compareAndSet(false, true)) {
        drop(client)
        drop(remote)
      }
    }
  }

  // -------------------------------------------------------------------------
  // Wire helpers
  // -------------------------------------------------------------------------

  /** Read up to and including the blank line; `null` when it never arrives. */
  private fun readHead(input: InputStream): String? {
    val out = ByteArrayOutputStream(128)
    var cr = false
    var seenBlank = 0
    while (out.size() < limits.headerBytes) {
      val byte = input.read()
      if (byte < 0) return null
      out.write(byte)
      when (byte.toChar()) {
        '\r' -> cr = true
        '\n' -> {
          // CRLFCRLF ends the head. A bare CR is tolerated because the only
          // thing read here is the request line we then validate.
          if (cr) seenBlank++ else seenBlank = 0
          if (seenBlank >= 2) return out.toString("ISO-8859-1")
          cr = false
        }
        else -> {
          cr = false
          seenBlank = 0
        }
      }
    }
    return null
  }

  private fun writeConstant(socket: Socket, response: String) {
    try {
      socket.getOutputStream().write(response.toByteArray(Charsets.ISO_8859_1))
      socket.getOutputStream().flush()
    } catch (_: Exception) {
      // The client is gone; the socket is closed by the caller.
    }
  }

  private fun refuse(client: Socket, reason: Refusal) {
    refusals[reason.ordinal].incrementAndGet()
    writeConstant(
      client,
      when (reason) {
        Refusal.METHOD -> METHOD_NOT_ALLOWED
        Refusal.BUSY -> UNAVAILABLE
        // "Could not reach a vetted address" is a gateway failure, not a
        // policy refusal; neither answer tells the page why.
        Refusal.CONNECT_FAILED, Refusal.RESOLVE_FAILED, Refusal.RESOLVE_EXHAUSTED -> BAD_GATEWAY
        else -> FORBIDDEN
      },
    )
  }

  private fun refuseBusy(client: Socket) {
    accepted.incrementAndGet()
    refuse(client, Refusal.BUSY)
    drop(client)
  }

  /**
   * Register a socket so [stop] can close it.
   *
   * A socket that appears after [stop] started (a handler that was inside
   * `connect()` when the listener closed) is closed immediately instead of
   * being added to a set that has already been drained — otherwise it would
   * live on until its idle timeout.
   */
  private fun track(socket: Socket) {
    live.add(socket)
    if (!running) {
      live.remove(socket)
      closeQuietly(socket)
    }
  }

  /** Close a socket this proxy owns and forget it. Idempotent. */
  private fun drop(socket: Socket?) {
    if (socket == null) return
    live.remove(socket)
    closeQuietly(socket)
  }

  private fun pool(name: String, threads: Int): ThreadPoolExecutor {
    val factory = ThreadFactory { runnable ->
      Thread(runnable, name).apply { isDaemon = true }
    }
    // SynchronousQueue + a fixed maximum: work never queues behind a blocked
    // handler. A rejected submission is a refusal, which is the fail-closed
    // answer — an unbounded queue would instead turn a connection flood into
    // unbounded memory.
    return ThreadPoolExecutor(0, threads, 30L, TimeUnit.SECONDS, SynchronousQueue(), factory)
  }

  companion object {
    /** The only address this proxy ever binds or accepts a target on. */
    const val LOOPBACK: String = "127.0.0.1"

    private const val ESTABLISHED = "HTTP/1.1 200 Connection Established\r\n\r\n"
    private const val FORBIDDEN =
      "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    private const val BAD_GATEWAY =
      "HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    private const val METHOD_NOT_ALLOWED =
      "HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    private const val UNAVAILABLE =
      "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"

    private fun isConnectMethod(head: String): Boolean = head.startsWith("CONNECT ")

    /**
     * Parse the request line of a proxy request.
     *
     * `null` for everything that is not a well-formed `CONNECT authority
     * HTTP/1.x`, including a cleartext `GET` (this proxy exists for TLS and
     * WSS only) and any authority this policy cannot reason about: a missing
     * or non-numeric port, an out-of-range port, userinfo, a path, quotes,
     * control bytes, an unbalanced IPv6 bracket or an IPv6 zone id.
     */
    fun parseConnectHead(head: String): Target? {
      val line = head.substringBefore("\r\n").substringBefore("\n")
      if (line.isEmpty()) return null
      val parts = line.split(' ')
      if (parts.size != 3) return null
      if (parts[0] != "CONNECT") return null
      if (!parts[2].startsWith("HTTP/1.")) return null
      return parseAuthority(parts[1])
    }

    /** Parse `host:port` / `[v6]:port`. See [parseConnectHead]. */
    fun parseAuthority(raw: String): Target? {
      if (raw.isEmpty() || raw.length > 300) return null
      // Printable ASCII only: no spaces, no controls, no IDN (the browser
      // already punycodes), nothing that could be smuggled into a header.
      if (raw.any { it.code < 0x21 || it.code > 0x7e }) return null
      if (raw.any { it == '@' || it == '/' || it == '?' || it == '#' || it == '\\' }) return null

      val host: String
      val portText: String
      if (raw[0] == '[') {
        val end = raw.indexOf(']')
        if (end <= 1) return null
        host = raw.substring(1, end)
        if (end + 1 >= raw.length || raw[end + 1] != ':') return null
        portText = raw.substring(end + 2)
        // Zone ids are ambiguous in an authority and never needed here.
        if (host.contains('%')) return null
        if (!host.contains(':')) return null
        if (!host.all { it.isDigit() || it in 'a'..'f' || it in 'A'..'F' || it == ':' || it == '.' }) {
          return null
        }
        if (host.length > 45) return null
      } else {
        val separator = raw.lastIndexOf(':')
        if (separator <= 0) return null
        host = raw.substring(0, separator)
        portText = raw.substring(separator + 1)
        if (host.contains(':') || host.contains('[') || host.contains(']')) return null
        if (host.length > 253) return null
      }

      val normalized = host.trimEnd('.').lowercase()
      if (normalized.isEmpty()) return null
      if (normalized.split('.').any { it.isEmpty() }) return null

      val port = parsePort(portText) ?: return null
      return Target(normalized, port)
    }

    /** 1..65535, digits only — no sign, no whitespace, no default. */
    fun parsePort(raw: String): Int? {
      if (raw.isEmpty() || raw.length > 5) return null
      if (!raw.all { it in '0'..'9' }) return null
      val port = raw.toIntOrNull() ?: return null
      return if (port in 1..65535) port else null
    }

    /**
     * The address an IP literal denotes, or `null` when [host] is a hostname.
     *
     * Parsing is deliberately strict so that a literal can never turn into a
     * lookup: IPv4 goes through the WHATWG parser (the same one the URL
     * standard uses, so `0177.0.0.1` and `0x7f000001` are 127.0.0.1 here too),
     * and a bracketed/colon form is only handed to `getByName` when every byte
     * is a hex digit, a colon or a dot — a shape Java parses without DNS.
     */
    fun literal(host: String): InetAddress? {
      if (!host.contains(':')) {
        val quad = IpPolicy.parseIpv4(host) ?: return null
        val parts = quad.split('.').map { it.toInt() }
        return InetAddress.getByAddress(
          byteArrayOf(parts[0].toByte(), parts[1].toByte(), parts[2].toByte(), parts[3].toByte()),
        )
      }
      if (!host.all { it.isDigit() || it in 'a'..'f' || it in 'A'..'F' || it == ':' || it == '.' }) {
        return null
      }
      return try {
        val address = InetAddress.getByName(host)
        // A v6-shaped literal must parse as v6; anything else is refused
        // rather than silently resolved (getByName would otherwise fall back
        // to a lookup for a malformed literal).
        if (address.address.size == 16) address else null
      } catch (_: Exception) {
        null
      }
    }

    private fun sameAddress(socket: Socket, expected: InetAddress): Boolean {
      val actual = try {
        socket.inetAddress
      } catch (_: Exception) {
        return false
      }
      return actual.address.contentEquals(expected.address)
    }

    fun closeQuietly(socket: Socket?) {
      try {
        socket?.close()
      } catch (_: Exception) {
        // Already closed.
      }
    }

    fun closeQuietly(server: ServerSocket?) {
      try {
        server?.close()
      } catch (_: Exception) {
        // Already closed.
      }
    }
  }
}
