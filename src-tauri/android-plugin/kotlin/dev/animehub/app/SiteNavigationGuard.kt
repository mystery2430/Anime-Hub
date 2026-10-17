// SPDX-License-Identifier: MIT
//
// Top-level navigation guard for the shared site WebView (Android only).
// Extracted from AnimeHubPlugin.kt so the CI unit-test source set can compile
// the site WebView stack without the Tauri plugin classes — the guard itself
// only depends on the Android SDK and SiteNavigationPolicy.

package dev.animehub.app

import android.graphics.Bitmap
import android.util.Log
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
 *  - Not covered: `WebView.loadUrl` calls made by the app itself (the app's
 *    `open_on_mobile` already validates the first URL), subframes, and
 *    sub-resources.
 *  - Not covered: the blocklist and DNS rebinding checks. Those live in Rust;
 *    running DNS on the UI thread from this callback is not acceptable.
 *
 * Every other callback is forwarded unchanged, so wry's custom-protocol
 * handling, the IPC page-started tracking and the error recovery keep working.
 */
class SiteNavigationGuard(private val inner: WebViewClient) : WebViewClient() {

  override fun shouldOverrideUrlLoading(view: WebView, request: WebResourceRequest): Boolean {
    val uri = request.url
    val blocked = SiteNavigationPolicy.rejection(uri)
    if (blocked != null) {
      // NO host, NO path, NO query: logcat is readable by other apps on older
      // Android releases and by anyone with adb, and the host alone identifies
      // which site the user is watching. Scheme + reason code + frame kind is
      // everything a developer needs to understand the decision.
      val message = SiteNavigationPolicy.blockedMessage(uri.scheme, blocked, request.isForMainFrame)
      Log.w(LOG_TAG, message)
      AnimeHubDebugLog.add("nav", message)
      return true
    }
    return inner.shouldOverrideUrlLoading(view, request)
  }

  override fun shouldInterceptRequest(view: WebView, request: WebResourceRequest): WebResourceResponse? =
    inner.shouldInterceptRequest(view, request)

  override fun onPageStarted(view: WebView, url: String, favicon: Bitmap?) =
    inner.onPageStarted(view, url, favicon)

  override fun onPageFinished(view: WebView, url: String) =
    inner.onPageFinished(view, url)

  override fun onReceivedError(view: WebView, request: WebResourceRequest, error: WebResourceError) {
    // Error code and frame only: the description and URL are left out.
    if (request.isForMainFrame) {
      AnimeHubDebugLog.add("web", "main frame error code=${error.errorCode}")
    }
    inner.onReceivedError(view, request, error)
  }

  companion object {
    private const val LOG_TAG = "AnimeHubNavGuard"

  }
}
