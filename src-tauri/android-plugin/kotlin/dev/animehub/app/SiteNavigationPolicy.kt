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

  private val IPV4 = Regex("""\d{1,3}(\.\d{1,3}){3}""")
  private val NUMERIC_LABEL = Regex("""(0x[0-9a-f]*|[0-9]+)""", RegexOption.IGNORE_CASE)

  fun allows(uri: Uri): Boolean {
    val scheme = uri.scheme?.lowercase() ?: return false
    return when (scheme) {
      "https" -> hostAllowed(uri.host)
      "blob", "data", "about" -> true
      else -> false
    }
  }

  private fun hostAllowed(rawHost: String?): Boolean {
    val host = rawHost?.lowercase()?.trimEnd('.')
    if (host.isNullOrEmpty()) return false
    val bare = host.removePrefix("[").removeSuffix("]")
    if (bare.contains(':')) return isPublicIpv6(bare)
    if (NUMERIC_LABEL.matches(bare.substringAfterLast('.'))) {
      return IPV4.matches(bare) && isPublicIpv4(bare)
    }
    if (IPV4.matches(bare)) return isPublicIpv4(bare)
    if (host in BLOCKED_HOSTS) return false
    return BLOCKED_SUFFIXES.none { host.endsWith(it) }
  }

  private fun isPublicIp(ip: String): Boolean = IpPolicy.isPublicIp(ip)
  private fun isPublicIpv4(ip: String): Boolean = IpPolicy.isPublicIpv4(ip)
  private fun isPublicIpv6(ip: String): Boolean = IpPolicy.isPublicIpv6(ip)

  fun isPublicIpForTest(ip: String): Boolean = isPublicIp(ip)
}
