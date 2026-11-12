// SPDX-License-Identifier: MIT
//
// Readiness policy for the process-wide WebView proxy.
//
// `ProxyController.setProxyOverride` applies to EVERY WebView in the app and
// reports completion through a callback; the AndroidX docs are explicit that
// "network connections are not guaranteed to immediately use the new proxy
// setting; wait for the listener before loading a page". A site WebView that
// navigates before that callback would make its first requests without the
// pinning proxy — exactly the window this feature exists to remove. So the
// open path consults this gate and REFUSES the open unless the proxy is
// confirmed applied.
//
// Pure JVM: the state machine, the config the adapter is allowed to build and
// the refusal are all testable without a WebView.
package dev.animehub.app

/**
 * The one proxy configuration this app will ever ask for.
 *
 * Deliberately has no bypass list, no `addDirect()` and no
 * `bypassSimpleHostnames()`: any of those directs a request to the origin
 * WITHOUT classification, which is the whole risk being removed. The implicit
 * localhost/link-local bypass must be removed too (`removeImplicitRules()`),
 * because "localhost" is a URL host an attacker can rebind a name to.
 */
object SiteProxyPolicy {
  /** The proxy binds and is addressed on loopback only. */
  const val LOOPBACK_HOST: String = "127.0.0.1"

  /**
   * Kotlin's refusal sentinel for a missing/failed proxy.
   *
   * `AnimeHubSiteController` returns it instead of opening a site, and Rust
   * maps it to user-facing guidance (never to "open anyway"). It contains no
   * device, host or URL detail.
   */
  const val ERR_PROXY_UNAVAILABLE: String = "proxy-unavailable"

  /**
   * How long [SiteProxyGate] waits for the `setProxyOverride` callback before
   * giving up. The callback normally arrives in milliseconds; this only bounds
   * the pathological case so a wedged provider cannot leave the open pending
   * forever.
   */
  const val INSTALL_DEADLINE_MS: Long = 5_000L

  /** `host:port` for `ProxyConfig.Builder.addProxyRule`, or `null` if unusable. */
  fun proxyRule(port: Int?): String? {
    if (port == null || port !in 1..65535) return null
    return "$LOOPBACK_HOST:$port"
  }

  /**
   * Whether a proxy rule is safe to hand to the WebView: loopback only, an
   * explicit ephemeral port, and never a rule that could mean "direct".
   */
  fun isLoopbackRule(rule: String): Boolean {
    val prefix = "$LOOPBACK_HOST:"
    if (!rule.startsWith(prefix)) return false
    val port = rule.substring(prefix.length)
    if (port.isEmpty() || port.length > 5 || !port.all { it in '0'..'9' }) return false
    return (port.toIntOrNull() ?: 0) in 1..65535
  }
}

/**
 * The install state of the process-wide proxy override.
 *
 * Terminal states are terminal on purpose: a provider that reports "unsupported"
 * or whose install callback never arrived is not retried on the next open,
 * because flipping between "refused" and "maybe pinned" per tap is how a user
 * ends up browsing on a WebView that silently is not pinned.
 */
class SiteProxyGate(
  private val deadlineMs: Long = SiteProxyPolicy.INSTALL_DEADLINE_MS,
) {
  enum class State {
    /** No install attempted yet. */
    NOT_STARTED,

    /** `setProxyOverride` was called; waiting for its listener. */
    INSTALLING,

    /** The provider confirmed the override. Site WebViews may navigate. */
    READY,

    /** `PROXY_OVERRIDE` is not supported by this WebView provider. */
    UNSUPPORTED,

    /** The proxy could not start, or the callback never arrived. */
    FAILED,
  }

  @Volatile
  var state: State = State.NOT_STARTED
    private set

  /** Deadline for the current install, or `null` when nothing is in flight. */
  @Volatile
  var deadlineAt: Long? = null
    private set

  private val waiting = ArrayList<(State) -> Unit>(MAX_WAITERS)

  /** `true` while an install is in flight and its deadline has not passed. */
  @Synchronized
  fun isInstalling(now: Long): Boolean =
    state == State.INSTALLING && (deadlineAt ?: 0L) > now

  /** Only the caller that won this transition may perform the install. */
  @Synchronized
  fun begin(now: Long): Boolean {
    if (state != State.NOT_STARTED) return false
    state = State.INSTALLING
    deadlineAt = now + deadlineMs
    return true
  }

  fun markReady() = settle(State.READY)

  fun markUnsupported() = settle(State.UNSUPPORTED)

  fun markFailed() = settle(State.FAILED)

  /**
   * Expire a pending install. Returns `true` when this call changed the state,
   * i.e. when the caller must now refuse the open.
   */
  fun expireIfOverdue(now: Long): Boolean {
    synchronized(this) {
      val deadline = deadlineAt ?: return false
      if (state != State.INSTALLING || now < deadline) return false
    }
    settle(State.FAILED)
    return true
  }

  /** Only a confirmed install may be navigated through. */
  fun canOpen(): Boolean = state == State.READY

  /**
   * What to do with an open in this state: `null` to build the view,
   * [SiteProxyPolicy.ERR_PROXY_UNAVAILABLE] to refuse, or [WAIT] while the
   * install callback is still outstanding.
   */
  fun openDecision(): String? = when (state) {
    State.READY -> null
    State.NOT_STARTED, State.INSTALLING -> WAIT
    State.UNSUPPORTED, State.FAILED -> SiteProxyPolicy.ERR_PROXY_UNAVAILABLE
  }

  /**
   * Register [onSettled] for the next terminal transition.
   *
   * Returns the already-settled state when there is nothing to wait for (the
   * caller then acts immediately and nothing is registered), or `null` when the
   * callback will be invoked. Past [MAX_WAITERS] the answer is [State.FAILED]:
   * callers stop being queued, which is a refusal, never an unbounded list.
   */
  @Synchronized
  fun observe(onSettled: (State) -> Unit): State? {
    val current = state
    if (isSettled(current)) return current
    if (waiting.size >= MAX_WAITERS) return State.FAILED
    waiting.add(onSettled)
    return null
  }

  companion object {
    /** Not a sentinel: tells the open path to wait for the callback. */
    const val WAIT: String = "proxy-installing"

    /** Bounded: a tap storm cannot grow the waiter list. */
    const val MAX_WAITERS: Int = 8

    private fun isSettled(state: State): Boolean =
      state == State.READY || state == State.UNSUPPORTED || state == State.FAILED
  }

  /** Move to [next] if it is a legal transition, then notify waiters OUTSIDE the lock. */
  private fun settle(next: State) {
    val notify: List<(State) -> Unit>
    synchronized(this) {
      if (state != State.INSTALLING) return
      state = next
      deadlineAt = null
      notify = ArrayList(waiting)
      waiting.clear()
    }
    notify.forEach { callback -> callback(next) }
  }
}
