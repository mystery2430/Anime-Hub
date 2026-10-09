// SPDX-License-Identifier: MIT
//
// Controlled Picture-in-Picture entry for AnimeHub's single Android WebView.
//
// WHY THIS EXISTS
// ---------------
// Android PiP puts the whole Activity — and therefore the whole site WebView —
// into the small PiP window. Site pages are not bare players, so entering PiP
// directly shows the site chrome instead of the video. The fix is a handshake:
//
//   PiP request  (onPictureInPictureRequested / onUserLeaveHint / the manual
//                 `enter_pip` command)
//        |
//        v  evaluateJavascript(): install the controller if this document does
//           not have it yet, then run window.__animehubPreparePip()
//           (JS lives in src-tauri/src/web/pip_controller.js)
//        |
//        v  the JS builds a player-only view over the site's own DOM and
//           answers with { ok, num, den, rect, kind }
//        |
//        v  PictureInPictureParams(ratio + sourceRectHint)
//           + Activity.enterPictureInPictureMode()
//
// and on the way out:
//
//   onPictureInPictureModeChanged(false, config)
//        -> window.__animehubPip(false) -> the page is exactly what it was.
//
// A failed preparation never enters PiP and never leaves the page changed: the
// JS restores itself on every failure path, and the Kotlin side re-asserts the
// restore before reporting the failure.
//
// WHY AUTO-ENTER STAYS OFF
// ------------------------
// `PictureInPictureParams.Builder#setAutoEnterEnabled` is documented as "If
// true, Activity#onPictureInPictureRequested() will never be called" — the
// system would enter PiP without giving the WebView a chance to prepare. So
// the user setting ("enter PiP automatically") is honoured through the
// controlled paths instead: `onPictureInPictureRequested()` on API 30+ and
// `onUserLeaveHint()` on API 26-29, both owned by MainActivity.
//
// Threading: every public entry point may be called from any thread and hops
// to the Activity's main looper, because `evaluateJavascript`,
// `setPictureInPictureParams` and `enterPictureInPictureMode` are main-thread
// APIs. The JS callback arrives on the main thread as well, which is what makes
// "prepare, then enter" a single ordered step instead of a race.
//
// This file is a TEMPLATE: `scripts/android_prepare.py` substitutes the
// controller JS into CONTROLLER_JS and copies the result next to the generated
// MainActivity.kt. Do not paste it into `src-tauri/gen/android` by hand.

package dev.animehub.app

import android.app.Activity
import android.app.PictureInPictureParams
import android.content.pm.ApplicationInfo
import android.content.pm.PackageManager
import android.graphics.Rect
import android.os.Build
import android.os.Looper
import android.util.Log
import android.util.Rational
import android.webkit.WebView
import android.widget.Toast
import org.json.JSONObject
import org.json.JSONTokener
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale
import java.lang.ref.WeakReference
import kotlin.math.roundToInt

/**
 * In-app diagnostics: the last PiP lines, read by the launcher's developer
 * panel through the `pip_debug_log` plugin command. Holds only status text
 * (trigger names, outcomes, the page's PiP report); never site URLs.
 */
object AnimeHubPipLog {
  private const val MAX_LINES = 200
  private val lines = ArrayDeque<String>()
  private val stamp = SimpleDateFormat("HH:mm:ss.SSS", Locale.ROOT)

  @Synchronized
  fun add(message: String) {
    if (lines.size >= MAX_LINES) lines.removeFirst()
    lines.addLast(stamp.format(Date()) + " " + message)
  }

  @Synchronized
  fun snapshot(): List<String> = lines.toList()
}

object AnimeHubPipController {

  // ------------------------------------------------------------- references

  /** The shared WebView, attached by MainActivity.onWebViewCreate(). */
  @Volatile
  private var webViewRef: WeakReference<WebView>? = null

  /** The Activity the callbacks came from; used for logging/liveness only. */
  @Volatile
  private var activityRef: WeakReference<Activity>? = null

  /**
   * User setting: may the Activity enter PiP through the controlled paths?
   * Default off (see `Settings::pip_auto_enter` in Rust). This is *not* the
   * platform's auto-enter flag; see the file header.
   */
  @Volatile
  var autoEnterEnabled: Boolean = false
    private set

  /** One preparation chain at a time; only touched from the main thread. */
  private var requestInFlight = false

  // ------------------------------------------------------------------- API

  /** Called from `MainActivity.onWebViewCreate(webView)`. */
  fun attach(activity: Activity, webView: WebView) {
    activityRef = WeakReference(activity)
    webViewRef = WeakReference(webView)
    requestInFlight = false
  }

  /** Called from `MainActivity.onDestroy()`: drop handles to the dead view. */
  fun detach(activity: Activity) {
    if (activityRef?.get() === activity) {
      activityRef = null
      webViewRef = null
      requestInFlight = false
    }
  }

  /** The WebView currently hosting the launcher or a site page, if any. */
  fun webView(): WebView? = webViewRef?.get()

  /** Logcat (tag AnimeHubPip) and the in-app developer panel, both. */
  private fun note(message: String) {
    Log.d(LOG_TAG, message)
    AnimeHubPipLog.add(message)
  }

  /** PiP is API 26+ and needs the hardware feature; both are checked. */
  fun isSupported(activity: Activity): Boolean =
    Build.VERSION.SDK_INT >= Build.VERSION_CODES.O &&
      activity.packageManager.hasSystemFeature(PackageManager.FEATURE_PICTURE_IN_PICTURE)

  /**
   * `set_pip_auto_enter` handler: remember the user's preference and, on
   * Android 12+, make sure the platform's own auto-enter stays disabled so the
   * controlled path keeps running.
   *
   * @return true when the preference was stored on a PiP-capable device.
   */
  fun applyAutoEnter(activity: Activity, enabled: Boolean): Boolean {
    if (!isSupported(activity)) {
      autoEnterEnabled = false
      return false
    }
    autoEnterEnabled = enabled
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.S) return true
    // Rejected ratios silently disable PiP, so the default parameters keep a
    // safe ratio and only switch auto-enter off.
    val params = PictureInPictureParams.Builder()
      .setAspectRatio(safeAspectRatio(DEFAULT_NUM, DEFAULT_DEN))
      .setAutoEnterEnabled(false)
      .build()
    runOnMain(activity) { activity.setPictureInPictureParams(params) }
    return true
  }

  /**
   * Run the controlled PiP chain.
   *
   * Callers: `onPictureInPictureRequested()` (API 30+), `onUserLeaveHint()`
   * (API 26-29) and the manual `enter_pip` command.
   *
   * @param fallbackNum numerator used when the detected player has no usable
   *   ratio (the manual command's requested ratio).
   * @param fallbackDen denominator for the same case.
   * @return true when the request was claimed — PiP is supported and the
   *   preparation started (or is already running / already in PiP). The
   *   `onPictureInPictureRequested()` contract is "the app received this
   *   callback", so claiming is correct even when no player is found.
   */
  fun requestEnter(
    activity: Activity,
    trigger: String,
    onResult: ((Boolean) -> Unit)? = null,
    fallbackNum: Int = DEFAULT_NUM,
    fallbackDen: Int = DEFAULT_DEN,
  ): Boolean {
    if (!isSupported(activity)) return false
    if (activity.isInPictureInPictureMode) {
      onResult?.invoke(true)
      return true
    }
    val webView = webViewRef?.get()
    note("request trigger=$trigger webView=${webView != null}")
    if (webView == null) {
      // Claimed (nothing else may enter PiP either) but answered, so a caller
      // that awaits a result — the `enter_pip` command — never waits forever.
      note("PiP isteği WebView hazır değilken geldi ($trigger)")
      onResult?.invoke(false)
      return true
    }
    if (requestInFlight) {
      note("PiP hazırlığı hâlâ sürüyor, istek yok sayıldı ($trigger)")
      onResult?.invoke(false)
      return true
    }
    requestInFlight = true
    runOnMain(activity) {
      if (rejectIfGone(activity, webView)) {
        requestInFlight = false
        onResult?.invoke(false)
        return@runOnMain
      }
      try {
        // Order matters: the JS answer decides whether PiP is entered at all.
        webView.evaluateJavascript(prepareScript()) { raw ->
          finishRequest(activity, webView, trigger, raw, fallbackNum, fallbackDen, onResult)
        }
      } catch (e: Exception) {
        requestInFlight = false
        note("PiP hazırlık betiği çalıştırılamadı: ${e.javaClass.simpleName}")
        onResult?.invoke(false)
      }
    }
    return true
  }

  /**
   * DIAGNOSTIC (debuggable builds only, no behaviour change): read the page's
   * PiP report and show it as a Toast and in logcat under tag "AnimeHubPip".
   * The script runs on the WebView, so the page gets no new interface.
   */
  fun showDebugReport(activity: Activity, webView: WebView?) {
    val debuggable = (activity.applicationInfo.flags and ApplicationInfo.FLAG_DEBUGGABLE) != 0
    webView?.evaluateJavascript(
      "(window.__animehubPipReport ? window.__animehubPipReport() : 'no-report')",
    ) { raw ->
      val text = runCatching { JSONTokener(raw).nextValue() as? String }.getOrNull() ?: raw
      note("report: $text")
      if (debuggable) {
        Toast.makeText(activity, "AnimeHubPip: $text", Toast.LENGTH_LONG).show()
      }
    }
  }

  /**
   * Keep the WebView rendering while the Activity is in PiP.
   *
   * Entering PiP pauses the Activity, and Wry's `WryActivity.onPause()` calls
   * `WebView.onPause()`. That stops the WebView's rendering and media
   * processing: the PiP window freezes on the last frame and the WebView's
   * media session drops its play/pause actions, while the audio keeps going.
   * Resuming the WebView while in PiP keeps the player live. The media that
   * the pause stopped is started again by the page side: `__animehubPip(true)`
   * restarts the player if it was playing before PiP. The normal `onResume()`
   * on return is a no-op on top of it.
   */
  fun keepRenderingInPip(webView: WebView?) {
    if (webView == null) return
    webView.onResume()
    webView.evaluateJavascript("if (window.__animehubPip) { window.__animehubPip(true); }", null)
  }

  /**
   * Undo the PiP-only view. Called from
   * `onPictureInPictureModeChanged(isInPictureInPictureMode = false, ...)`.
   *
   * The script is evaluated in whatever document is loaded: if the page
   * navigated while PiP was open there is nothing to restore and the call is a
   * no-op (`window.__animehubPip` is then undefined, not re-installed).
   */
  fun restore(webView: WebView?, onDone: ((Boolean) -> Unit)? = null) {
    val view = webView ?: webViewRef?.get() ?: run {
      onDone?.invoke(false)
      return
    }
    runOnMain(view) {
      if (view !== webViewRef?.get()) {
        onDone?.invoke(false)
        return@runOnMain
      }
      try {
        view.evaluateJavascript(restoreScript()) { raw ->
          onDone?.invoke(raw == "true")
        }
      } catch (e: Exception) {
        note("PiP görünümü geri alınamadı: ${e.javaClass.simpleName}")
        onDone?.invoke(false)
      }
    }
  }

  // -------------------------------------------------------------- internals

  private fun finishRequest(
    activity: Activity,
    webView: WebView,
    trigger: String,
    raw: String?,
    fallbackNum: Int,
    fallbackDen: Int,
    onResult: ((Boolean) -> Unit)?,
  ) {
    requestInFlight = false
    if (rejectIfGone(activity, webView)) {
      onResult?.invoke(false)
      return
    }
    if (raw == null || raw == "null") {
      // The document was torn down mid-evaluation (navigation won the race).
      note("PiP hazırlığı yanıtsız kaldı ($trigger)")
      onResult?.invoke(false)
      return
    }
    val answer = try {
      JSONObject(raw)
    } catch (e: Exception) {
      note("PiP hazırlık yanıtı okunamadı: ${e.javaClass.simpleName}")
      null
    }
    if (answer == null || !answer.optBoolean("ok", false)) {
      val reason = answer?.optString("reason") ?: "unreadable"
      // Nothing was entered and the JS already restored what it touched; the
      // extra restore only covers a partial failure inside the page.
      note("PiP hazırlığı uygun oyuncu bulamadı ($trigger, $reason)")
      restore(webView, null)
      onResult?.invoke(false)
      return
    }

    val num = answer.optInt("num", fallbackNum)
    val den = answer.optInt("den", fallbackDen)
    val rect = sourceRect(answer.optJSONObject("rect"), webView)
    val entered = try {
      enter(activity, num, den, rect)
    } catch (e: IllegalStateException) {
      // "Activity must be resumed to enter picture-in-picture": the app was
      // already stopped, so there is nothing to enter. Never crash for this.
      note("PiP'ye girilemedi: Activity hazır değil ($trigger)")
      false
    } catch (e: Exception) {
      note("PiP'ye girilemedi ($trigger): ${e.javaClass.simpleName}")
      false
    }
    if (!entered) {
      // Leaving the page in player-only mode with no PiP window would strand
      // the user on a stripped-down page, so undo it.
      restore(webView, null)
    } else {
      note("PiP'ye girildi ($trigger, ${answer.optString("kind")}, ${num}x$den)")
    }
    onResult?.invoke(entered)
  }

  /** Build and enter PiP with the detected ratio and the player's bounds. */
  private fun enter(activity: Activity, num: Int, den: Int, rect: Rect?): Boolean {
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) return false
    val builder = PictureInPictureParams.Builder()
      .setAspectRatio(safeAspectRatio(num, den))
    // sourceRectHint is only a transition hint: it tells the system which part
    // of the window is worth animating from. It does not crop anything, so the
    // player-only look still comes from the JS view.
    if (rect != null && rect.width() > 0 && rect.height() > 0) {
      builder.setSourceRectHint(rect)
    }
    return activity.enterPictureInPictureMode(builder.build())
  }

  /**
   * `sourceRectHint` wants window coordinates in device pixels. The JS reports
   * the player's viewport rect already scaled by `devicePixelRatio`, and the
   * site WebView fills the Activity window, so the only work left is clamping
   * the numbers to the view and dropping degenerate rects.
   */
  private fun sourceRect(json: JSONObject?, webView: WebView): Rect? {
    if (json == null) return null
    val width = json.optDouble("width", 0.0).roundToInt()
    val height = json.optDouble("height", 0.0).roundToInt()
    if (width <= 0 || height <= 0) return null
    val left = json.optDouble("left", 0.0).roundToInt().coerceAtLeast(0)
    val top = json.optDouble("top", 0.0).roundToInt().coerceAtLeast(0)
    val viewWidth = if (webView.width > 0) webView.width else left + width
    val viewHeight = if (webView.height > 0) webView.height else top + height
    val right = (left + width).coerceAtMost(viewWidth)
    val bottom = (top + height).coerceAtMost(viewHeight)
    if (right <= left || bottom <= top) return null
    return Rect(left, top, right, bottom)
  }

  /** A destroyed Activity or a WebView that is gone: drop the request. */
  private fun rejectIfGone(activity: Activity, webView: WebView): Boolean {
    if (activity.isFinishing || activity.isDestroyed) return true
    return webViewRef?.get() !== webView
  }

  private fun runOnMain(target: Any, action: () -> Unit) {
    if (Looper.myLooper() === Looper.getMainLooper()) {
      action()
      return
    }
    when (target) {
      is Activity -> target.runOnUiThread { action() }
      is WebView -> target.post { action() }
      else -> action()
    }
  }

  // ------------------------------------------------------- embedded JS calls

  /**
   * Install the controller when the loaded document does not have it yet, then
   * prepare. Re-installing matters because site navigation creates a new
   * document: `window.__animehub*` does not survive a page load, and the Rust
   * init script on Android cannot be attached to a document that was not
   * created by the launcher. The controller self-guards by version, so this is
   * cheap when the script is already there.
   */
  private fun prepareScript(): String =
    "(function(){try{" +
      "if(typeof window.$PREPARE_FN!=='function'){" + CONTROLLER_JS + "}" +
      "if(typeof window.$PREPARE_FN!=='function'){return {ok:false,reason:'controller-missing'};}" +
      "return window.$PREPARE_FN();" +
      "}catch(e){return {ok:false,reason:'exception'};}})();"

  /**
   * Restore call. Deliberately does not install the controller: a document
   * that never had one has no PiP view to undo.
   */
  private fun restoreScript(): String =
    "(function(){try{" +
      "if(typeof window.$TOGGLE_FN!=='function'){return false;}" +
      "return window.$TOGGLE_FN(false)===true;" +
      "}catch(e){return false;}})();"

  // --------------------------------------------------------------- constants

  private const val LOG_TAG = "AnimeHubPip"
  private const val PREPARE_FN = "__animehubPreparePip"
  private const val TOGGLE_FN = "__animehubPip"
  private const val DEFAULT_NUM = 16
  private const val DEFAULT_DEN = 9

  /**
   * WebView-side controller, embedded verbatim from
   * `src-tauri/src/web/pip_controller.js` by `scripts/android_prepare.py` —
   * the same file Rust injects into site pages, so both sides can never drift.
   *
   * The script is wrapped in a marker so the prepare step can replace it
   * deterministically, and it must stay free of dollar signs and triple
   * quotes, both of which would break a Kotlin raw string.
   */
  private val CONTROLLER_JS = """
__ANIMEHUB_PIP_CONTROLLER_JS__
""".trim()

  /**
   * Android rejects PiP ratios outside about 1:2.39 .. 2.39:1, and a rejected
   * ratio silently disables PiP. Clamping each side on its own still allows
   * 239:1, which `setAspectRatio` refuses, so the ratio itself is clamped.
   */
  private const val MAX_RATIO = 2.39
  private const val MIN_RATIO = 0.41841 // 1 / 2.39

  private fun safeAspectRatio(num: Int, den: Int): Rational {
    val n = num.coerceAtLeast(1)
    val d = den.coerceAtLeast(1)
    return when {
      n.toDouble() / d.toDouble() > MAX_RATIO -> Rational(2390, 1000)
      n.toDouble() / d.toDouble() < MIN_RATIO -> Rational(1000, 2390)
      else -> Rational(n, d)
    }
  }
}
