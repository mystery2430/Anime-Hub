// SPDX-License-Identifier: MIT
//
// Top-level navigation guard for the shared site WebView (Android only).
// Extracted from AnimeHubPlugin.kt so the CI unit-test source set can compile
// the site WebView stack without the Tauri plugin classes — the guard itself
// only depends on the Android SDK and SiteNavigationPolicy.

package dev.animehub.app

import android.graphics.Bitmap
import android.webkit.WebResourceError
import android.webkit.WebResourceRequest
import android.webkit.WebResourceResponse
import android.os.SystemClock
import android.webkit.WebView
import android.webkit.WebViewClient
import java.net.InetAddress
import java.util.concurrent.ConcurrentHashMap

/**
 * Top-level navigation guard for the shared site WebView (Android only).
 *
 * Desktop enforces the site policy in Tauri's `on_navigation` hook. Android has
 * no equivalent for the single launcher WebView: wry's navigation handler is
 * fixed when the WebView is created and is not reachable from app code. So this
 * class wraps wry's `WebViewClient` and cancels the navigations that the desktop
 * policy would refuse.
 *
 * Scope (verified against wry 0.55.1 source and the Android WebView docs, not on
 * a device):
 *  - Covered: top-level navigations that the WebView routes through
 *    `shouldOverrideUrlLoading` (link clicks, `location` changes, main-frame
 *    redirects as delivered by the WebView).
 *  - Also covered, at request level: `shouldInterceptRequest` refuses cleartext
 *    and private/loopback/reserved hosts for sub-resources and subframes too
 *    (see `SiteNavigationPolicy.subresourceRejection` for exactly what that
 *    does and does not catch).
 *  - Not covered: `WebView.loadUrl` calls made by the app itself (the app's
 *    `open_on_mobile` already validates the first URL).
 *  - Partially covered: DNS rebinding. `shouldInterceptRequest` resolves the
 *    host (short, bounded cache) and refuses a non-empty answer containing a
 *    private/loopback/link-local address, or a public answer that differs from
 *    the pinned first answer. This is defence in depth, not a complete fix:
 *    the engine resolves again when it connects (TOCTOU). See
 *    `docs/security-residual-risks.md`.
 *  - Blocklist: the user's Rust rules are pushed into [SiteBlocklist] before
 *    every open; both the top-level and sub-resource checkpoints consult it.
 *
 * Every other callback is forwarded unchanged, so wry's custom-protocol
 * handling, the IPC page-started tracking and the error recovery keep working.
 */
class SiteNavigationGuard(private val inner: WebViewClient) : WebViewClient() {

  /** Hostname → (blocked?, monotonic check time, pinned public set). */
  private val resolutions = ConcurrentHashMap<String, Resolution>()

  override fun shouldOverrideUrlLoading(view: WebView, request: WebResourceRequest): Boolean {
    val uri = request.url
    val blocked = SiteNavigationPolicy.rejection(uri)
    if (blocked != null) {
      // The navigation remains blocked as before. Do not emit per-request or
      // per-page diagnostics; the target could expose a URL or site identity.
      return true
    }
    return inner.shouldOverrideUrlLoading(view, request)
  }

  /**
   * Sub-resource checkpoint: refuse cleartext and private/loopback/reserved
   * hosts for every request the WebView makes, not just top-level navigations.
   *
   * A hostname is resolved before the request is handed to the engine, and
   * requests whose answer contains ANY private/loopback/link-local address —
   * or a different public set after the first check — are refused. The
   * engine's own later resolution keeps a TOCTOU window, documented in
   * `docs/security-residual-risks.md` and NOT claimed as fixed.
   *
   * The cache is per-process, keyed by hostname, and bounded. A resolver
   * failure is fail-open: a DNS outage must not brick every site.
   *
   * Runs on the WebView's background thread, never the UI thread.
   */
  override fun shouldInterceptRequest(
    view: WebView,
    request: WebResourceRequest,
  ): WebResourceResponse? {
    val uri = request.url
    val blocked = SiteNavigationPolicy.subresourceRejection(uri)
    if (blocked != null) {
      // Keep the existing fail-closed response, without per-request logging.
      return blockedResponse()
    }
    if (resolvesToPrivateAddress(uri)) {
      return blockedResponse()
    }
    return inner.shouldInterceptRequest(view, request)
  }

  /**
   * Best-effort DNS pre-check. `shouldInterceptRequest` runs on a WebView
   * background thread, so the resolver call cannot block the UI.
   */
  private fun resolvesToPrivateAddress(uri: android.net.Uri): Boolean {
    val scheme = uri.scheme?.lowercase() ?: return false
    if (scheme != "https" && scheme != "wss") return false
    val host = uri.host?.lowercase()?.trimEnd('.') ?: return false
    if (host.isEmpty()) return false
    // IP literals are already covered by the pure policy; skip the resolver.
    val bare = host.removePrefix("[").removeSuffix("]")
    if (bare.contains(':') || bare.all { it.isDigit() || it == '.' }) return false
    val now = SystemClock.elapsedRealtime()
    cachedResolution(host)?.let { cached ->
      if (now - cached.checkedAt < DNS_CACHE_MS) return cached.privateAddress
    }
    val previous = cachedResolution(host)?.pinnedPublic
    val answer =
      try {
        InetAddress.getAllByName(host).mapNotNull { it.hostAddress }
      } catch (_: Exception) {
        // Resolver unavailable: do not turn a network outage into a local block.
        return false
      }
    if (answer.isEmpty()) return false
    val verdict = SiteNavigationPolicy.ResolvedHostPolicy.classify(previous, answer)
    val blocked = verdict != SiteNavigationPolicy.ResolvedHostPolicy.DnsVerdict.ALLOW
    val pinned =
      if (verdict == SiteNavigationPolicy.ResolvedHostPolicy.DnsVerdict.ALLOW) {
        previous ?: answer.map { it.substringBefore('%').trim() }.toSet()
      } else {
        previous
      }
    if (resolutions.size > MAX_CACHED_HOSTS) resolutions.clear()
    resolutions[host] = Resolution(blocked, now, pinned)
    return blocked
  }

  private fun cachedResolution(host: String): Resolution? = resolutions[host]

  private data class Resolution(
    val privateAddress: Boolean,
    val checkedAt: Long,
    val pinnedPublic: Set<String>? = null,
  )

  private companion object {
    const val DNS_CACHE_MS = 60_000L
    const val MAX_CACHED_HOSTS = 256
  }

  /**
   * An empty 403. No body, no headers: nothing about the request or the reason
   * is handed back to the page, which only sees a failed fetch.
   */
  private fun blockedResponse(): WebResourceResponse =
    WebResourceResponse(
      "text/plain",
      "utf-8",
      403,
      "Blocked",
      emptyMap(),
      java.io.ByteArrayInputStream(ByteArray(0)),
    )

  override fun onPageStarted(view: WebView, url: String, favicon: Bitmap?) =
    inner.onPageStarted(view, url, favicon)

  override fun onPageFinished(view: WebView, url: String) =
    inner.onPageFinished(view, url)

  override fun onReceivedError(view: WebView, request: WebResourceRequest, error: WebResourceError) {
    // Forward the original callback unchanged; page errors are not diagnostics.
    inner.onReceivedError(view, request, error)
  }
}
