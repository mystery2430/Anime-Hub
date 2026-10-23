// SPDX-License-Identifier: MIT
//
// Android wiring for the connection-pinning proxy.
//
// This is the only file that touches androidx.webkit's ProxyController. It
// starts `DnsPinnedProxy` on loopback, points every WebView in the process at
// it, and only reports ready once the provider's callback says the override is
// applied.
//
// PROCESS-WIDE BY DESIGN. `ProxyController.setProxyOverride` governs every
// WebView in the app: the launcher (Tauri/wry, default profile) and the site
// WebViews. Installing it per site and clearing it on close would leave the
// process unpinned between opens — and a site WebView that survives one
// failed close would keep navigating directly. So:
//
//   * install once, on the first site open, and keep it for the life of the
//     process;
//   * never call `clearProxyOverride`, not on close, not on a failed open, not
//     on Activity destroy — a cleared override means "all connections direct";
//   * if the override cannot be installed (no PROXY_OVERRIDE, the proxy cannot
//     bind, the callback never arrives) the site open is REFUSED. There is no
//     direct-connection fallback and no "allow and probe" path left.
//
// The launcher keeps working because its own origin is served locally: wry's
// `RustWebViewClient.shouldInterceptRequest` answers the asset-loader domain
// (and the Tauri IPC path) before any network I/O, so those requests never
// reach the proxy. They are not bypassed — if such a request ever did reach
// the proxy it would be classified like any other and refused as loopback
// (fail closed, never fail open). The device checklist verifies this.
package dev.animehub.app

import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.util.Log
import androidx.webkit.ProxyConfig
import androidx.webkit.ProxyController
import androidx.webkit.WebViewFeature
import java.util.concurrent.Executors

object SiteProxyController {
  private const val LOG_TAG = "AnimeHubProxy"

  /**
   * Sentinel `AnimeHubSiteController.open` returns when the pinning proxy is
   * not confirmed applied. Rust maps it to user-facing guidance (see
   * `PROXY_UNAVAILABLE_MSG` in `android_bridge.rs`); it is never a warning to
   * "continue anyway".
   */
  const val ERR_PROXY_UNAVAILABLE: String = SiteProxyPolicy.ERR_PROXY_UNAVAILABLE

  private val mainHandler = Handler(Looper.getMainLooper())
  private val gate = SiteProxyGate()

  /** The proxy this process points every WebView at; never stopped while alive. */
  private var proxy: DnsPinnedProxy? = null

  /** The listener executor. One thread is enough for a one-shot callback. */
  private val installExecutor = Executors.newSingleThreadExecutor { runnable ->
    Thread(runnable, "AnimeHubProxyInstall").apply { isDaemon = true }
  }

  /**
   * Ensure the process proxy is applied, then report on the UI thread.
   *
   * `false` means the caller MUST refuse the site open.
   *
   * Called on the UI thread from `AnimeHubSiteController.open`, i.e. before the
   * site WebView exists and before its first `loadUrl`.
   */
  fun ensureReady(operationId: String, onReady: (Boolean) -> Unit) {
    // A wedged earlier install must not wait forever on a later open: expire it
    // first, then decide from the resulting state.
    gate.expireIfOverdue(SystemClock.elapsedRealtime())
    if (gate.canOpen()) {
      onReady(true)
      return
    }
    when (gate.openDecision()) {
      SiteProxyPolicy.ERR_PROXY_UNAVAILABLE -> {
        AnimeHubDebugLog.record(
          "proxy", "ready", "refused", operationId, errorClass = "unavailable",
        )
        onReady(false)
        return
      }
      else -> Unit
    }

    // Already installing: wait for the transition this install will publish.
    gate.observe { state -> mainHandler.post { onReady(state == SiteProxyGate.State.READY) } }
      ?.let { settled ->
        // Registered nothing (already settled): answer from the settled state.
        mainHandler.post { onReady(settled == SiteProxyGate.State.READY) }
        return
      }

    if (!gate.begin(SystemClock.elapsedRealtime())) {
      // Another caller won the transition and will publish it; the observer
      // above is already registered for that.
      return
    }

    AnimeHubDebugLog.record("proxy", "install", "started", operationId)
    // Bound the callback: a provider that never calls back must not leave the
    // open pending forever (and must never be treated as ready).
    mainHandler.postDelayed({
      if (gate.expireIfOverdue(SystemClock.elapsedRealtime())) {
        AnimeHubDebugLog.record(
          "proxy", "install", "timeout", operationId, errorClass = "unavailable",
        )
        logCoarse("proxy install timed out")
      }
    }, SiteProxyPolicy.INSTALL_DEADLINE_MS)

    if (!WebViewFeature.isFeatureSupported(WebViewFeature.PROXY_OVERRIDE)) {
      AnimeHubDebugLog.record(
        "proxy", "install", "unsupported", operationId, errorClass = "unsupported",
      )
      logCoarse("PROXY_OVERRIDE unsupported by this WebView provider")
      gate.markUnsupported()
      return
    }

    val started = try {
      proxy = DnsPinnedProxy().also { it.start() }
    } catch (e: Exception) {
      logCoarse("proxy could not bind loopback")
      gate.markFailed()
      return
    }
    val rule = SiteProxyPolicy.proxyRule(started.port)
    if (rule == null || !SiteProxyPolicy.isLoopbackRule(rule)) {
      logCoarse("proxy rule rejected")
      gate.markFailed()
      return
    }

    // The ONLY configuration this app builds: one loopback proxy rule, the
    // implicit localhost/link-local bypass removed, and no bypass list, no
    // addDirect(), no bypassSimpleHostnames(). Anything else would leave a
    // direct path that skips classification.
    val config = ProxyConfig.Builder()
      .addProxyRule(rule)
      .removeImplicitRules()
      .build()

    try {
      ProxyController.getInstance().setProxyOverride(config, installExecutor) {
        mainHandler.post {
          AnimeHubDebugLog.record("proxy", "install", "ok", operationId)
          gate.markReady()
        }
      }
    } catch (e: Exception) {
      // IllegalArgumentException (invalid config) or UnsupportedOperationException
      // (feature vanished between the check and the call): both are refusals.
      logCoarse("setProxyOverride rejected")
      gate.markFailed()
    }
  }

  /**
   * Whether a site WebView may navigate right now.
   *
   * Only a confirmed install counts. Used by the controller's `open` as a final
   * check immediately before `loadUrl`, so a state that regressed between the
   * gate check and the build is caught.
   */
  fun isPinned(): Boolean = gate.canOpen()

  private fun logCoarse(message: String) {
    // Never the rule, a host, a URL or a port: those are policy inputs.
    Log.w(LOG_TAG, message)
  }
}
