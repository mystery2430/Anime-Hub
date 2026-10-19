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
import android.webkit.WebView
import android.webkit.WebViewClient

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
 *  - Not covered: DNS rebinding and the user blocklist. No DNS runs here — a
 *    pre-check would be TOCTOU against the engine's own resolution — and the
 *    Rust blocklist is not plumbed to the WebView yet. Both are documented open
 *    limitations, not protection.
 *
 * Every other callback is forwarded unchanged, so wry's custom-protocol
 * handling, the IPC page-started tracking and the error recovery keep working.
 */
class SiteNavigationGuard(private val inner: WebViewClient) : WebViewClient() {

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
   * Deliberately NO DNS lookup here. Returning null hands the request back to
   * the engine, which resolves and connects by itself, so a lookup would be a
   * TOCTOU check a rebinding hostname defeats — plus latency on every request.
   * What is caught: IP literals and the host rules in [SiteNavigationPolicy].
   * What is not: a hostname that resolves to a private address, and the user
   * blocklist (not plumbed to the WebView yet). Both are documented as open
   * limitations instead of being claimed as protection.
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
    return inner.shouldInterceptRequest(view, request)
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
