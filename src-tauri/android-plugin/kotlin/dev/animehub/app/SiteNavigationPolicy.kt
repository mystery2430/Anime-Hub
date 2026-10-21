package dev.animehub.app

import android.net.Uri

/**
 * The desktop site policy, restated for Kotlin. Keep it in step with
 * `navigation_allowed` and `is_private_host` in `src-tauri/src/sites/url_policy.rs`;
 * `tests/gaps.test.js` checks the shared host suffixes.
 *
 * Deliberately has no exception for the launcher's own origin: a site must not
 * be able to load `tauri.localhost` into the WebView, because that origin is
 * the one that receives IPC. The launcher returns through `loadUrl`, which is
 * not routed through this guard.
 *
 * Saf IP kontrolü IpPolicy'ye delege eder – aynı dosya testte ve APK'da kullanılır.
 */
internal object SiteNavigationPolicy {
  private val BLOCKED_HOSTS = setOf(
    "localhost",
    "localhost.localdomain",
    "ip6-localhost",
    "ip6-loopback",
    "metadata.google.internal",
  )

  private val BLOCKED_SUFFIXES = listOf(".localhost", ".local", ".internal", ".lan", ".home")


  /**
   * Reason codes, identical to Rust `NavBlock::code`. Shared with
   * `tests/fixtures/navigation-policy.tsv`, which both test suites load: the two
   * implementations must answer the same decision AND the same reason.
   */
  const val INSECURE_SCHEME = "insecure_scheme"
  const val UNSUPPORTED_SCHEME = "unsupported_scheme"
  const val DATA_NAVIGATION = "data_navigation"
  const val UNKNOWN_ABOUT = "unknown_about"
  const val PRIVATE_HOST = "private_host"

  /**
   * `null` when the navigation is allowed, otherwise the reason code.
   *
   * This is the TOP-LEVEL checkpoint (`shouldOverrideUrlLoading`), which is why
   * `blob:` stays allowed — MSE players navigate the main frame to blob object
   * URLs — while `data:` is refused: as a top-level document it is an
   * opaque-origin HTML/script carrier that no site legitimately needs. `about:`
   * is narrowed to `about:blank`.
   */
  /**
   * Everything the policy needs to know about a navigation target, with no
   * Android types — so the decision itself is unit-testable. `android.net.Uri`
   * is a stub in unit tests (calling it throws `RuntimeException("Stub!")`, and
   * `returnDefaultValues` would silently turn every answer into a default), so
   * the ONE untestable line is the extraction below, not the rule.
   *
   * @param scheme URI scheme, as `Uri.getScheme` returns it (may be mixed case)
   * @param host hierarchical URIs only; IPv6 literals keep their brackets, as
   *   `Uri.getHost` returns them. Null/empty for opaque URIs.
   * @param opaquePart `Uri.getSchemeSpecificPart`: for `blob:`/`data:`/`about:`
   *   this is everything after the colon.
   */
  data class NavTarget(val scheme: String?, val host: String?, val opaquePart: String?)

  /** Read a platform [Uri] into the pure decision input. Device-verifiable only. */
  fun rejection(uri: Uri): String? = rejection(NavTarget(uri.scheme, uri.host, uri.schemeSpecificPart))

  fun rejection(target: NavTarget): String? {
    val scheme = target.scheme?.lowercase() ?: return UNSUPPORTED_SCHEME
    return when (scheme) {
      "https" -> if (hostAllowed(target.host)) null else PRIVATE_HOST
      "http" -> INSECURE_SCHEME
      "blob" ->
        if ((target.opaquePart ?: "").lowercase().startsWith("http://")) INSECURE_SCHEME else null
      "data" -> DATA_NAVIGATION
      "about" -> if (target.opaquePart == "blank") null else UNKNOWN_ABOUT
      else -> UNSUPPORTED_SCHEME
    }
  }

  fun allows(uri: Uri): Boolean = rejection(uri) == null

  fun allows(target: NavTarget): Boolean = rejection(target) == null

  /**
   * SUB-RESOURCE checkpoint (`shouldInterceptRequest`), which is deliberately
   * NOT the same rule as the top-level one above:
   *
   *  - `data:` stays ALLOWED here. An inline image, font or media source is
   *    normal; only a top-level data: document is a fake page.
   *  - `wss:` is allowed (live sites use WebSockets) and `ws:` refused as the
   *    cleartext it is.
   *  - private/loopback/reserved hosts and cleartext are refused for EVERY
   *    request, main frame or not, so a page cannot make the WebView fetch
   *    `https://192.168.1.1/admin` or `https://169.254.169.254/…`.
   *
   * WHAT THIS DOES AND DOES **NOT** DO (see docs/security-residual-risks.md):
   *
   *  - DNS is a PRE-CHECK, not connection pinning. Both checkpoints share
   *    [NavigationDnsGate]: `shouldInterceptRequest` resolves off the UI thread
   *    before handing the request to the engine and refuses a private, loopback,
   *    link-local or changed answer. `shouldOverrideUrlLoading` must answer
   *    synchronously, so a cache miss returns `ALLOW_AND_PROBE`: the navigation
   *    proceeds and the answer is cached for later requests. The engine resolves
   *    again when it connects, so a rebinding host can answer public to the gate
   *    and private to the socket (TOCTOU). That remains an OPEN risk; neither
   *    this comment nor a unit test closes it.
   *  - The user blocklist IS connected. Rust pushes the snapshot before every
   *    open and after every settings change or reset; [SiteBlocklist] is a
   *    process-wide atomic snapshot, so a replacement applies to a live WebView
   *    immediately. If the Rust→Kotlin transfer fails, Rust aborts the open and
   *    closes an already-open site on the normal lifecycle path before returning
   *    an error. If that close also fails, the caller gets a restart-required
   *    error instead of a claim that the old snapshot is still safe.
   *  - `blob:` inherits its creator's origin and is allowed, because refusing it
   *    stops MSE playback dead.
   */
  fun subresourceRejection(uri: Uri): String? =
    subresourceRejection(NavTarget(uri.scheme, uri.host, uri.schemeSpecificPart))

  fun subresourceRejection(target: NavTarget): String? {
    val scheme = target.scheme?.lowercase() ?: return UNSUPPORTED_SCHEME
    return when (scheme) {
      "https",
      "wss" -> if (hostAllowed(target.host)) null else PRIVATE_HOST

      "http",
      "ws" -> INSECURE_SCHEME
      // Inline assets and object URLs: legitimate sub-resources.
      "data",
      "blob",
      "about" -> null

      else -> UNSUPPORTED_SCHEME
    }
  }

  /**
   * The single log line for a blocked navigation: scheme, reason code and frame
   * kind — NO host, NO path, NO query. logcat is readable by any app holding
   * READ_LOGS on older releases and by anyone with adb, and the host alone says
   * which site the user is watching.
   *
   * Lives here and not in [SiteNavigationGuard] because the guard extends
   * `android.webkit.WebViewClient`, whose stub methods have no code attribute —
   * the class cannot even be LOADED in a JVM unit test (`ClassFormatError`), so
   * anything worth asserting about redaction has to sit in pure code.
   */
  fun blockedMessage(scheme: String?, reason: String, forMainFrame: Boolean): String {
    val frame = if (forMainFrame) "main" else "sub"
    return "navigation blocked reason=$reason scheme=${scheme ?: "none"} frame=$frame"
  }

  /** Test accessor for the shared host vectors; calls the production rule. */
  fun hostAllowedForTest(rawHost: String?): Boolean = hostAllowed(rawHost)

  private fun hostAllowed(rawHost: String?): Boolean {
    val host = rawHost?.lowercase()?.trimEnd('.')
    if (host.isNullOrEmpty()) return false
    // User blocklist (pushed from Rust before the open). Must be evaluated
    // before any built-in allow rule so a listed tracker is refused in the
    // main frame AND in sub-resources.
    if (SiteBlocklist.isBlocked(host)) return false
    val bare = host.removePrefix("[").removeSuffix("]")
    if (bare.contains(':')) return isPublicIpv6(bare)
    // Every numeric host form — a canonical dotted quad included — goes through
    // the WHATWG IPv4 parser, so `8.8.8.8`, `010.0.0.1` (octal → 8.0.0.1),
    // `0x7f000001` and `127.1` are decided by the same algorithm the desktop
    // side uses. A host the parser refuses (`example.0x7f`, `1.2.3.4.5`) is
    // blocked, never treated as public.
    if (IpPolicy.endsInANumber(bare)) {
      val canonical = IpPolicy.parseIpv4(bare) ?: return false
      return isPublicIpv4(canonical)
    }
    if (host in BLOCKED_HOSTS) return false
    return BLOCKED_SUFFIXES.none { host.endsWith(it) }
  }

  private fun isPublicIp(ip: String): Boolean = IpPolicy.isPublicIp(ip)
  private fun isPublicIpv4(ip: String): Boolean = IpPolicy.isPublicIpv4(ip)
  private fun isPublicIpv6(ip: String): Boolean = IpPolicy.isPublicIpv6(ip)

  fun isPublicIpForTest(ip: String): Boolean = isPublicIp(ip)

  /**
   * Decision for the DNS PRE-CHECK in [SiteNavigationGuard].
   *
   * A hostname that resolves to a private/loopback/link-local address is
   * refused before the request is handed back to the engine. Once a host has
   * answered with public addresses, a later DIFFERENT public set is refused
   * too (rebinding detection). This is deliberately NOT a complete fix: the
   * engine resolves again when it connects (TOCTOU), and the user blocklist is
   * pushed separately. The only complete answer is a network layer that pins
   * the resolved address; see docs/security-residual-risks.md.
   */
  object ResolvedHostPolicy {
    /** How one DNS answer relates to the previous pinned answer. */
    enum class DnsVerdict { ALLOW, PRIVATE_ADDRESS, CHANGED_ADDRESS }

    fun anyPrivate(answer: List<String>): Boolean =
      answer.any { raw ->
        val ip = raw.substringBefore('%').trim()
        if (ip.isEmpty()) return@any true
        !IpPolicy.isPublicIp(ip)
      }

    /**
     * [anyPrivate] plus flip detection. An empty answer is fail-open at the
     * caller (a resolver outage must not brick the app); a malformed entry
     * fails closed. This cannot make the engine's connection use the checked
     * address; that residual TOCTOU window stays documented.
     */
    fun classify(previousPublic: Set<String>?, answer: List<String>): DnsVerdict {
      if (answer.isEmpty()) return DnsVerdict.ALLOW
      if (anyPrivate(answer)) return DnsVerdict.PRIVATE_ADDRESS
      val normalized = answer.map { it.substringBefore('%').trim() }.toSet()
      if (previousPublic != null && normalized != previousPublic) {
        return DnsVerdict.CHANGED_ADDRESS
      }
      return DnsVerdict.ALLOW
    }
  }
}
