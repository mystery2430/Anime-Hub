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
import java.util.concurrent.Executors

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
 *  - Partially covered: DNS rebinding. BOTH checkpoints consult
 *    [NavigationDnsGate]: `shouldInterceptRequest` resolves the host (short,
 *    bounded cache) and refuses a private/loopback/link-local answer or a
 *    changed public answer, and `shouldOverrideUrlLoading` cancels a
 *    redirect/main-frame target whose *cached* answer is private or changed.
 *    On a cache miss the top-level check cannot block (it must answer
 *    synchronously and never resolve on the UI thread), so it allows the
 *    navigation and probes in the background. This is defence in depth, not
 *    connection pinning: the engine resolves again when it connects (TOCTOU).
 *    See `docs/security-residual-risks.md` — R-01 stays partial.
 *  - Blocklist: the user's Rust rules are pushed into [SiteBlocklist] before
 *    every open and after settings/reset changes; both the top-level and
 *    sub-resource checkpoints consult it. A failed transfer is handled in Rust
 *    (abort the open, close an already-open site, surface a restart error if the
 *    close cannot complete); Kotlin never treats a failed push as empty rules.
 *
 * Every other callback is forwarded unchanged, so wry's custom-protocol
 * handling, the IPC page-started tracking and the error recovery keep working.
 */
class SiteNavigationGuard(private val inner: WebViewClient) : WebViewClient() {

  /** Shared verdict cache for both checkpoints. */
  private val dnsGate = NavigationDnsGate()

  /**
   * One daemon thread for top-level probes. The UI thread must never resolve:
   * `shouldOverrideUrlLoading` has to answer synchronously, which is why an
   * unknown host is allowed AND probed instead of blocking the navigation.
   */
  private val probeExecutor =
    Executors.newSingleThreadExecutor { runnable ->
      Thread(runnable, "AnimeHubDnsProbe").apply { isDaemon = true }
    }

  override fun shouldOverrideUrlLoading(view: WebView, request: WebResourceRequest): Boolean {
    val uri = request.url
    val blocked = SiteNavigationPolicy.rejection(uri)
    if (blocked != null) {
      // The navigation remains blocked as before. Do not emit per-request or
      // per-page diagnostics; the target could expose a URL or site identity.
      return true
    }
    when (dnsGate.decide(uri.host, SystemClock.elapsedRealtime())) {
      // A redirect target whose answer was already seen to be private (or to
      // have changed) is cancelled HERE as well: redirect chains are not
      // guaranteed to reach `shouldInterceptRequest`.
      NavigationDnsGate.Decision.BLOCK -> return true
      NavigationDnsGate.Decision.ALLOW_AND_PROBE -> probeInBackground(uri.host)
      NavigationDnsGate.Decision.ALLOW -> {}
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
   * Resolve a top-level target off the UI thread purely to feed the cache.
   * The navigation that triggered it proceeds: blocking on a lookup here
   * would mean either a UI-thread resolver call or an ANR.
   */
  private fun probeInBackground(rawHost: String?) {
    // IP literals and empty hosts: the pure policy already decides those.
    val host = NavigationDnsGate.normalize(rawHost) ?: return
    try {
      probeExecutor.execute {
        try {
          val answer = InetAddress.getAllByName(host).mapNotNull { it.hostAddress }
          dnsGate.record(host, answer, SystemClock.elapsedRealtime())
        } catch (_: Exception) {
          // Resolver outage: fail-open, and never leave the host "probing".
          dnsGate.recordLookupFailure(host)
        }
      }
    } catch (_: Exception) {
      dnsGate.recordLookupFailure(host)
    }
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
    when (dnsGate.cachedVerdict(host, now)) {
      NavigationDnsGate.Verdict.PUBLIC -> return false
      NavigationDnsGate.Verdict.PRIVATE, NavigationDnsGate.Verdict.CHANGED -> return true
      null -> Unit
    }
    val answer =
      try {
        InetAddress.getAllByName(host).mapNotNull { it.hostAddress }
      } catch (_: Exception) {
        // Resolver unavailable: do not turn a network outage into a local block.
        dnsGate.recordLookupFailure(host)
        return false
      }
    if (answer.isEmpty()) {
      dnsGate.recordLookupFailure(host)
      return false
    }
    return dnsGate.record(host, answer, now) == NavigationDnsGate.Decision.BLOCK
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
