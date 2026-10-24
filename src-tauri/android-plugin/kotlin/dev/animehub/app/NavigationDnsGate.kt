// SPDX-License-Identifier: MIT
//
// DNS verdicts for the Android site WebView, shared by the top-level
// navigation checkpoint (`shouldOverrideUrlLoading`) and the request
// checkpoint (`shouldInterceptRequest`). Pure JVM code: no android.* import, so
// the same decisions are unit-tested in CI.
//
// WHY THIS EXISTS: a redirect chain does not always pass through
// `shouldInterceptRequest`, and the top-level checkpoint previously looked at
// the URL text only. A site could therefore navigate the main frame to a
// hostname that resolves to 127.0.0.1 / 10.x and the private-host rule (which
// only sees IP literals and suffixes) would not notice.
//
// WHAT THIS IS NOT: connection pinning. The engine resolves again when it
// connects, so a rebinding name can answer public addresses to the check and a
// private one to the socket. This narrows the window and blocks every redirect
// whose ANSWER was already seen to be private/changing; it does not close the
// TOCTOU window and is not reported as fixed (docs/security-residual-risks.md).
package dev.animehub.app

import java.util.LinkedHashMap

/**
 * Cache + verdict policy for one process.
 *
 * A top-level navigation cannot wait for a resolver (it runs on the UI thread),
 * so [decide] only ever answers from the cache and asks the caller to probe on
 * a miss. The request path resolves synchronously — it is already off the UI
 * thread — and feeds the answer back through [record], which is what makes the
 * NEXT top-level navigation block a redirect to a private address.
 *
 * TWO THINGS THIS DELIBERATELY REFUSES TO DO:
 *
 *  * cache an answer it does not have. [record] writes nothing for an empty
 *    answer (see there): "no address" is not "a public address", and storing it
 *    would make the next [decide] answer ALLOW from a verdict nobody proved.
 *  * queue an unbounded amount of resolver work. [probing] holds at most
 *    [maxProbing] hosts, so a page that fires many distinct hostnames at the
 *    top-level checkpoint cannot grow the set — and, through it, the single
 *    resolver executor's task queue — without bound. Reaching the budget is
 *    fail-open, exactly like a cache miss: the navigation proceeds without a
 *    probe, and the request checkpoint still resolves synchronously.
 */
class NavigationDnsGate(
  private val cacheMs: Long = CACHE_MS,
  private val maxHosts: Int = MAX_HOSTS,
  private val maxProbing: Int = MAX_PROBING,
) {
  enum class Verdict { PUBLIC, PRIVATE, CHANGED }

  enum class Decision {
    /** Cached public answer: let the navigation continue. */
    ALLOW,

    /** Nothing fresh cached; let this navigation through AND resolve in background. */
    ALLOW_AND_PROBE,

    /** A known-private or changed answer: cancel the navigation. */
    BLOCK,
  }

  data class Entry(val verdict: Verdict, val pinned: Set<String>, val checkedAt: Long)

  /** Insertion-ordered so eviction is oldest-first, not arbitrary. */
  private val entries = LinkedHashMap<String, Entry>()

  /** Hosts with a probe already running: prevents a resolution stampede. */
  private val probing = HashSet<String>()

  /** Cached verdict for [rawHost], or null when nothing fresh is known. */
  @Synchronized
  fun cachedVerdict(rawHost: String?, now: Long): Verdict? {
    val host = normalize(rawHost) ?: return null
    val entry = entries[host] ?: return null
    if (!isFresh(entry.checkedAt, now)) return null
    return entry.verdict
  }

  /**
   * Decision for a top-level navigation, using cached knowledge only.
   *
   * A missing host is BLOCKED: a hierarchical HTTPS navigation always has one,
   * and "no host" is not a destination this policy can reason about.
   */
  @Synchronized
  fun decide(rawHost: String?, now: Long): Decision {
    val host = normalize(rawHost) ?: return Decision.BLOCK
    val entry = entries[host]
    if (entry != null && isFresh(entry.checkedAt, now)) {
      return when (entry.verdict) {
        Verdict.PUBLIC -> Decision.ALLOW
        Verdict.PRIVATE, Verdict.CHANGED -> Decision.BLOCK
      }
    }
    // Unknown is fail-open, deliberately: a resolver outage must not brick
    // every navigation. The caller probes and the verdict lands here.
    if (probing.contains(host)) return Decision.ALLOW
    if (probing.size >= maxProbing.coerceAtLeast(0)) {
      // Probe budget exhausted: do NOT add another host. This is the back
      // pressure that keeps `probing` and the resolver executor's task queue
      // bounded when a page navigates through many distinct hostnames; the
      // navigation is treated like a normal cache miss (allowed, unprobed) and
      // the request checkpoint still resolves it synchronously.
      return Decision.ALLOW
    }
    return if (probing.add(host)) Decision.ALLOW_AND_PROBE else Decision.ALLOW
  }

  /** Record a resolver answer; returns the decision it implies for this host. */
  @Synchronized
  fun record(rawHost: String, answer: List<String>, now: Long): Decision {
    val host = normalize(rawHost) ?: return Decision.BLOCK
    // Whatever the answer, this host is no longer being probed.
    probing.remove(host)
    if (answer.isEmpty()) {
      // An EMPTY answer is not evidence of a public address, so it must not be
      // cached: [SiteNavigationPolicy.ResolvedHostPolicy.classify] reports it as
      // ALLOW only because the caller fails open on a resolver outage, and
      // storing that as `PUBLIC` (with an empty `pinned` set) manufactured a
      // "safe" verdict for a host that was never resolved. Write nothing and
      // let the next navigation probe again.
      return Decision.ALLOW
    }
    val previous = entries[host]?.takeIf { isFresh(it.checkedAt, now) }?.pinned
    val verdict = SiteNavigationPolicy.ResolvedHostPolicy.classify(previous, answer)
    val decision =
      when (verdict) {
        SiteNavigationPolicy.ResolvedHostPolicy.DnsVerdict.PRIVATE_ADDRESS -> Decision.BLOCK
        SiteNavigationPolicy.ResolvedHostPolicy.DnsVerdict.CHANGED_ADDRESS -> Decision.BLOCK
        SiteNavigationPolicy.ResolvedHostPolicy.DnsVerdict.ALLOW -> Decision.ALLOW
      }
    val mapped =
      when (verdict) {
        SiteNavigationPolicy.ResolvedHostPolicy.DnsVerdict.PRIVATE_ADDRESS -> Verdict.PRIVATE
        SiteNavigationPolicy.ResolvedHostPolicy.DnsVerdict.CHANGED_ADDRESS -> Verdict.CHANGED
        SiteNavigationPolicy.ResolvedHostPolicy.DnsVerdict.ALLOW -> Verdict.PUBLIC
      }
    // Only a non-empty answer reaches here, so `pinned` always has at least one
    // address to compare a later answer against (see the empty-answer return
    // above). The scope id suffix of a link-local IPv6 address is stripped
    // because it is per-interface and would otherwise read as a "change".
    val pinned = answer.map { it.substringBefore('%').trim() }.toSet()
    if (entries.size >= maxHosts && !entries.containsKey(host)) {
      entries.remove(entries.keys.first())
    }
    entries[host] = Entry(mapped, pinned, now)
    return decision
  }

  /** A failed lookup must not be cached and must not leave the host "probing". */
  @Synchronized
  fun recordLookupFailure(rawHost: String?) {
    val host = normalize(rawHost) ?: return
    probing.remove(host)
  }

  @Synchronized
  fun size(): Int = entries.size

  /** Hosts currently holding a probe slot (diagnostics and budget tests). */
  @Synchronized
  fun probingCount(): Int = probing.size

  private fun isFresh(checkedAt: Long, now: Long): Boolean =
    now >= checkedAt && now - checkedAt < cacheMs

  companion object {
    const val CACHE_MS: Long = 60_000L
    const val MAX_HOSTS: Int = 256

    /**
     * Hard cap on hosts with a probe in flight.
     *
     * The probe executor is a single-thread queue, so an uncapped `probing` set
     * was also an uncapped task queue: a page that walks many hostnames (ad
     * redirector chains do exactly that) could enqueue one resolver task per
     * name, each of which blocks for the resolver timeout. The cap bounds both
     * the set and the queue; a host that is refused a slot is simply not
     * probed, which is the same "unknown, fail open" state as a cache miss.
     */
    const val MAX_PROBING: Int = 32

    /**
     * Lower-case, strip the DNS root dot and IPv6 brackets. IP literals are
     * left to [SiteNavigationPolicy] (which fails closed on them) and are not
     * resolved here.
     */
    fun normalize(rawHost: String?): String? {
      val trimmed = rawHost?.trim()?.lowercase()?.trimEnd('.') ?: return null
      if (trimmed.isEmpty()) return null
      val bare = trimmed.removePrefix("[").removeSuffix("]")
      if (bare.contains(':')) return null
      if (IpPolicy.endsInANumber(bare)) return null
      return bare
    }
  }
}
