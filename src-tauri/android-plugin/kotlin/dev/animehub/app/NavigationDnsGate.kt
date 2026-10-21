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
 */
class NavigationDnsGate(
  private val cacheMs: Long = CACHE_MS,
  private val maxHosts: Int = MAX_HOSTS,
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
    if (probing.add(host)) return Decision.ALLOW_AND_PROBE
    return Decision.ALLOW
  }

  /** Record a resolver answer; returns the decision it implies for this host. */
  @Synchronized
  fun record(rawHost: String, answer: List<String>, now: Long): Decision {
    val host = normalize(rawHost) ?: return Decision.BLOCK
    probing.remove(host)
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
    // An empty answer pins nothing: there is no address to compare later.
    val pinned =
      if (answer.isEmpty()) {
        emptySet()
      } else {
        answer.map { it.substringBefore('%').trim() }.toSet()
      }
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

  private fun isFresh(checkedAt: Long, now: Long): Boolean =
    now >= checkedAt && now - checkedAt < cacheMs

  companion object {
    const val CACHE_MS: Long = 60_000L
    const val MAX_HOSTS: Int = 256

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
