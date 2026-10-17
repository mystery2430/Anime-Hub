// SPDX-License-Identifier: MIT
//
// Android half of AnimeHub's Keystore + Picture-in-Picture + per-site
// WebView profile bridge.
//
// The PiP commands below delegate to `AnimeHubPipController`, which owns the
// PiP lifecycle and the WebView preparation handshake; the file next to this
// one is generated from a template by `scripts/android_prepare.py` (see the
// PLACEHOLDER note in that script).
//
// INSTALLATION
// ------------
// `npm run tauri android init` generates the Gradle project under
// `src-tauri/gen/android/`. Copy this file next to the generated
// `MainActivity.kt`, i.e. into:
//
//   src-tauri/gen/android/app/src/main/java/dev/animehub/app/
//
// Do not copy by hand. `scripts/android_prepare.py` copies this file and
// refuses to continue unless three strings are identical:
//   * the `package` line below,
//   * `register_android_plugin` in `android-plugin/src/lib.rs`,
//   * the package of the generated `MainActivity.kt`.
// Tauri keeps the dots in `dev.animehub.app`; it does not rewrite them to
// underscores.
//
// VERIFICATION STATUS
// -------------------
// Written against the Tauri 2.11.6 plugin API
// (`@TauriPlugin`, `Plugin`, `@Command`, `Invoke`, `JSObject`) and the
// standard Android APIs listed in the imports. The Keystore half predates this
// note. The controlled PiP chain (this file + `AnimeHubPipController.kt`) was
// written from the AOSP sources of `Activity`/`PictureInPictureParams` and has
// NOT been run on a device yet; the site-profile commands (`site_open` and
// friends, in `AnimeHubSiteController.kt`) were written against the AndroidX
// WebKit 1.16.0 multi-profile reference and have not been compiled or run on
// a device either (the build environment has no Android SDK/NDK). Treat it as
// unverified until a real `./scripts/build.sh android` / `tauri android
// build` has been installed on hardware. Only the JS half of PiP has
// automated tests (`tests/pip_controller.test.js`).
//
// STORAGE ISOLATION CONTRACT (site commands)
// ------------------------------------------
// Sites open in their own WebView bound to their own androidx.webkit Profile
// (`AnimeHubSiteController`), so localStorage, IndexedDB, cookies and cache
// are isolated per site by the provider itself. There is no JSON
// export/import path any more: the old dump was lossy and never a security
// boundary. Cookie migration from the pre-profile era is a one-time,
// profile-scoped import inside the controller; every other cookie operation
// goes through the site profile's own `Profile.getCookieManager()`, never the
// static `CookieManager.getInstance()` (that one is the launcher's default
// profile). When the WebView provider lacks `WebViewFeature.MULTI_PROFILE`,
// `site_open` fails with a sentinel instead of silently sharing the default
// profile. WebView operations run on Android's UI thread; blocking cookie
// flushes run on a background executor.

package dev.animehub.app

import android.app.Activity
import android.os.Build
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.Base64
import android.graphics.Bitmap
import android.net.Uri
import android.util.Log
import android.widget.Toast
import java.net.Inet6Address
import java.net.InetAddress
import android.webkit.WebResourceError
import android.webkit.WebResourceRequest
import android.webkit.WebResourceResponse
import android.webkit.WebView
import android.webkit.WebViewClient
import androidx.appcompat.app.AppCompatActivity
import androidx.webkit.JavaScriptReplyProxy
import androidx.webkit.WebMessageCompat
import androidx.webkit.WebViewCompat
import androidx.webkit.WebViewFeature
import app.tauri.annotation.Command
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import java.security.KeyStore
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale
import java.util.concurrent.Executors
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

@TauriPlugin
class AnimeHubPlugin(private val activity: Activity) : Plugin(activity) {

  override fun load(webView: WebView) {
    // The launcher keeps the default profile. Sites open above it in their
    // own profiled WebViews (AnimeHubSiteController), so this WebView stays
    // launcher-only: policy guard + IPC restriction as before.
    installNavigationGuard(webView)
    restrictIpcToLauncher(webView)
    AnimeHubSiteController.bindLauncher(webView)
    AnimeHubDebugLog.add("app", "WebView ready, api ${Build.VERSION.SDK_INT}")
  }

  /**
   * wry gives every WebView a `ipc` JavaScript interface, so any page loaded in
   * it, a remote site included, can call `window.ipc.postMessage`. Tauri on
   * Android has no other IPC path (its custom protocol is off there), so the
   * launcher still needs the object. This replaces it with a WebMessageListener
   * that only the launcher origins receive, and only in the main frame.
   * If the listener cannot be installed, wry's object is still removed and the
   * launcher's IPC is off (logged). Sites never get an `ipc` object.
   */
  private fun restrictIpcToLauncher(webView: WebView) {
    // Remove wry's object on every path, first. A site page must never get an
    // `ipc` object, even when the launcher listener cannot be installed. The
    // cost of failing closed is that launcher IPC is off on that device; it is
    // logged as an error below, not hidden.
    webView.removeJavascriptInterface(LauncherIpc.OBJECT_NAME)
    val id = (webView as? RustWebView)?.id
    if (id == null) {
      Log.e(LOG_TAG, "launcher IPC is off: WebView is not a RustWebView")
      AnimeHubDebugLog.add("ipc", "launcher IPC off: WebView is not a RustWebView")
      showIpcOffNotice()
      return
    }
    if (!WebViewFeature.isFeatureSupported(WebViewFeature.WEB_MESSAGE_LISTENER)) {
      Log.e(LOG_TAG, "launcher IPC is off: WebMessageListener unsupported")
      AnimeHubDebugLog.add("ipc", "launcher IPC off: WebMessageListener unsupported")
      showIpcOffNotice()
      return
    }
    try {
      WebViewCompat.addWebMessageListener(
        webView,
        LauncherIpc.OBJECT_NAME,
        LauncherIpc.ORIGINS,
        LauncherIpc(id),
      )
      AnimeHubDebugLog.add("ipc", "launcher listener installed")
    } catch (e: Exception) {
      AnimeHubDebugLog.add("ipc", "listener failed (${e.javaClass.simpleName}); launcher IPC is off")
      // Fail closed: the wry object is already gone, so no site page gets an `ipc` object.
      Log.e(LOG_TAG, "IPC listener could not be installed; launcher IPC is off", e)
      showIpcOffNotice()
    }
  }

  /**
   * Tell the user, natively, that launcher IPC is off. The launcher cannot
   * show this itself: its JS never gets a working `ipc` in this state. Only the
   * fixed Turkish text is shown; no URL, no page data. Posted to the UI thread.
   */
  private fun showIpcOffNotice() {
    activity.runOnUiThread {
      try {
        Toast.makeText(
          activity,
          "Uygulama içi bağlantı başlatılamadı. Ayarlar ve bazı özellikler çalışmayabilir; uygulamayı yeniden başlatın.",
          Toast.LENGTH_LONG,
        ).show()
      } catch (e: Exception) {
        // Showing a notice must never break the plugin load.
        Log.e(LOG_TAG, "could not show the IPC notice", e)
      }
    }
  }

  /**
   * Wrap the WebView's client so top-level navigations pass the site policy.
   *
   * Tauri calls `load` from its `on_webview_created` hook, which runs after
   * wry has set its own `RustWebViewClient`, so the current client is the one
   * to wrap. wry's `Ipc` object holds a reference to that original client and
   * reads its `currentUrl`, which the delegate's `onPageStarted` keeps up to
   * date. See `SiteNavigationGuard` for the limits.
   */
  private fun installNavigationGuard(webView: WebView) {
    val current = webView.webViewClient
    if (current is SiteNavigationGuard) return
    webView.webViewClient = SiteNavigationGuard(current)
  }

  override fun onDestroy(activity: AppCompatActivity) {
    // Plugin instances can outlive their Activity. The site WebView is torn
    // down by AnimeHubSiteController.destroy (MainActivity block) before this
    // runs; no WebView reference is kept on the plugin any more.
  }

  /** Reject an async reply if its Android host was destroyed while work was pending. */
  private fun rejectIfActivityUnavailable(invoke: Invoke): Boolean {
    if (activity.isFinishing || activity.isDestroyed) {
      invoke.reject("Android Activity artık kullanılamıyor")
      return true
    }
    return false
  }

  /** Run Activity-bound Android APIs on the UI thread and reject a destroyed host. */
  private fun withLiveActivity(invoke: Invoke, action: () -> Unit) {
    try {
      activity.runOnUiThread {
        if (rejectIfActivityUnavailable(invoke)) return@runOnUiThread
        try {
          action()
        } catch (e: Exception) {
          invoke.reject("Android işlemi başarısız", e)
        }
      }
    } catch (e: Exception) {
      invoke.reject("Android Activity artık kullanılamıyor", e)
    }
  }

  // ------------------------------------------------------------------ Keystore

  @Command
  fun keystore_seal(invoke: Invoke) {
    val args = invoke.getArgs()
    val purpose = args.getString("purpose") ?: run {
      invoke.reject("purpose gerekli"); return
    }
    val value = args.getString("value") ?: run {
      invoke.reject("value gerekli"); return
    }
    try {
      val cipher = Cipher.getInstance(TRANSFORMATION)
      cipher.init(Cipher.ENCRYPT_MODE, keyFor(purpose))
      val ct = cipher.doFinal(value.toByteArray(Charsets.UTF_8))
      // Prefix the IV so `open` can recover it; the IV is not secret.
      val combined = cipher.iv + ct
      invoke.resolve(JSObject().put("value", Base64.encodeToString(combined, Base64.NO_WRAP)))
    } catch (e: Exception) {
      invoke.reject("Keystore şifrelemesi başarısız: ${e.message}", e)
    }
  }

  @Command
  fun keystore_open(invoke: Invoke) {
    val args = invoke.getArgs()
    val purpose = args.getString("purpose") ?: run {
      invoke.reject("purpose gerekli"); return
    }
    val sealed = args.getString("value") ?: run {
      invoke.reject("value gerekli"); return
    }
    try {
      val combined = Base64.decode(sealed, Base64.NO_WRAP)
      if (combined.size <= GCM_IV_LENGTH) {
        invoke.reject("şifreli veri çok kısa"); return
      }
      val iv = combined.copyOfRange(0, GCM_IV_LENGTH)
      val ct = combined.copyOfRange(GCM_IV_LENGTH, combined.size)

      val cipher = Cipher.getInstance(TRANSFORMATION)
      cipher.init(Cipher.DECRYPT_MODE, keyFor(purpose), GCMParameterSpec(GCM_TAG_BITS, iv))
      val plain = cipher.doFinal(ct)
      invoke.resolve(JSObject().put("value", String(plain, Charsets.UTF_8)))
    } catch (e: Exception) {
      // Never leak the plaintext or the key; a decrypt failure is reported
      // as a generic error.
      invoke.reject("Keystore çözülemedi", e)
    }
  }

  // -------------------------------------------------------- Site profiles
  //
  // Per-site WebViews with androidx.webkit multi-profile (see the header and
  // AnimeHubSiteController). Every command resolves with data or rejects with
  // a message Rust classifies. The open is TWO-PHASED: `site_migrate` (the
  // one-time legacy jar import, no view) and `site_open` (the view). Both
  // reject with the exact sentinel `unsupported-multi-profile` when the
  // WebView provider cannot isolate sites — Rust turns that into the
  // user-facing guidance, and the site is never opened on the shared default
  // profile. `site_migrate` additionally rejects with `migration-partial: …`
  // when the strict import criterion was not met, which Rust retires and
  // retries on a fresh generation.

  /**
   * PHASE 1 of the two-phase open protocol: import the one-time legacy jar
   * into `profile` WITHOUT creating the site WebView. Rust persists
   * `profilemig-<site>` only after this resolves, and only then calls
   * `site_open` — so no view the user sees can exist while the migration
   * state is not durable, and a failed flag write aborts the open before any
   * view is built (no orphan WebView, no split Rust/Kotlin ownership).
   * Rejects with `migration-partial: …` when the import did not strictly
   * complete (Rust retires that profile name and retries on a fresh
   * generation), and with `unsupported-multi-profile` when the provider
   * cannot isolate sites at all.
   */
  @Command
  fun site_migrate(invoke: Invoke) {
    val args = invoke.getArgs()
    val url = args.getString("url") ?: run {
      invoke.reject("url gerekli"); return
    }
    val profileName = args.getString("profile") ?: run {
      invoke.reject("profile gerekli"); return
    }
    val cookies = mutableListOf<String>()
    try {
      args.optJSONArray("cookies")?.let { array ->
        for (i in 0 until array.length()) {
          cookies.add(array.getString(i))
        }
      }
    } catch (e: Exception) {
      invoke.reject("çerez listesi çözümlenemedi", e)
      return
    }
    withLiveActivity(invoke) {
      AnimeHubSiteController.migrate(activity, url, profileName, cookies) { error ->
        if (error != null) {
          invoke.reject(error)
        } else {
          invoke.resolve(JSObject().put("ok", true))
        }
      }
    }
  }

  /**
   * PHASE 2: open the site WebView bound to `profile`. Takes no cookies —
   * the one-time jar import runs in [site_migrate] before Rust persists the
   * migration flag, so this call can never migrate behind the flag's back.
   */
  @Command
  fun site_open(invoke: Invoke) {
    val args = invoke.getArgs()
    val url = args.getString("url") ?: run {
      invoke.reject("url gerekli"); return
    }
    val profileName = args.getString("profile") ?: run {
      invoke.reject("profile gerekli"); return
    }
    val initScript = args.getString("initScript") ?: ""
    withLiveActivity(invoke) {
      AnimeHubSiteController.open(activity, url, profileName, initScript) { error ->
        if (error != null) {
          invoke.reject(error)
        } else {
          invoke.resolve(JSObject().put("ok", true))
        }
      }
    }
  }

  /**
   * Close the active site WebView. `flush=false` is the wipe path: the data
   * is about to be cleared or deleted, so it must not be re-persisted first.
   * Idempotent — a stale Rust session resolves `ok` instead of failing.
   */
  @Command
  fun site_close(invoke: Invoke) {
    val flush = invoke.getArgs().optBoolean("flush", true)
    withLiveActivity(invoke) {
      AnimeHubSiteController.close(activity, flush) { error ->
        if (error != null) invoke.reject(error) else invoke.resolve(JSObject().put("ok", true))
      }
    }
  }

  /**
   * Delete one site profile's data. `{deleted:false, pending:true}` means the
   * provider refused right now (in use / unsupported) and Rust must queue the
   * name for the next process start; a reject is a hard failure the caller
   * must not interpret as success.
   */
  @Command
  fun site_profile_delete(invoke: Invoke) {
    val profileName = invoke.getArgs().getString("profile") ?: run {
      invoke.reject("profile gerekli"); return
    }
    withLiveActivity(invoke) {
      AnimeHubSiteController.deleteProfileData(profileName) { deleted, pending, error ->
        if (error != null) {
          invoke.reject(error)
        } else {
          val payload = JSObject()
          payload.put("deleted", deleted)
          payload.put("pending", pending)
          invoke.resolve(payload)
        }
      }
    }
  }

  /**
   * Retire one profile name's quarantine ledger.
   *
   * Rust calls this ONLY after the site's generation bump is durably stored
   * (`run_mobile_delete_protocol`), which is what makes the name underivable
   * forever; retiring earlier would leave a name both reachable and unblocked
   * when the bump or its write fails. Replies `{dropped}` — how many
   * outstanding operation records went with it — or rejects with the refusal
   * decided by the pure `ProfileRetirement` rule.
   */
  @Command
  fun site_profile_retire(invoke: Invoke) {
    val profileName = invoke.getArgs().getString("profile") ?: run {
      invoke.reject("profile gerekli"); return
    }
    withLiveActivity(invoke) {
      AnimeHubSiteController.retireProfileQuarantine(profileName) { dropped, error ->
        if (error != null) {
          invoke.reject(error)
        } else {
          invoke.resolve(JSObject().put("dropped", dropped))
        }
      }
    }
  }

  /** Clear one site profile in place (cookies, web storage, permissions). */
  @Command
  fun site_clear_data(invoke: Invoke) {
    val profileName = invoke.getArgs().getString("profile") ?: run {
      invoke.reject("profile gerekli"); return
    }
    withLiveActivity(invoke) {
      AnimeHubSiteController.clearProfileData(activity, profileName) { error ->
        if (error != null) invoke.reject(error) else invoke.resolve(JSObject().put("ok", true))
      }
    }
  }

  /** Periodic durability hook: flush the active site profile's cookies. */
  @Command
  fun site_flush(invoke: Invoke) {
    withLiveActivity(invoke) {
      AnimeHubSiteController.flush(activity) { error ->
        if (error != null) invoke.reject(error) else invoke.resolve(JSObject().put("ok", true))
      }
    }
  }

  /**
   * Every profile name the provider store knows (for the "clear all" orphan
   * sweep). An unsupported provider resolves an empty list with
   * `unsupported=true`: Rust skips the sweep and logs instead of guessing.
   */
  @Command
  fun site_profile_names(invoke: Invoke) {
    withLiveActivity(invoke) {
      val names = AnimeHubSiteController.allProfileNames()
      val payload = JSObject()
      val array = org.json.JSONArray()
      if (names == null) {
        payload.put("unsupported", true)
      } else {
        names.forEach { array.put(it) }
      }
      payload.put("names", array)
      invoke.resolve(payload)
    }
  }

  // ------------------------------------------------------------------- PiP

  /**
   * Manual PiP entry (`enter_pip`).
   *
   * Runs the exact same controlled chain as the Activity callbacks — prepare
   * the WebView through `window.__animehubPreparePip()`, wait for its answer,
   * then `enterPictureInPictureMode()` — so this command can never put the
   * whole site into the PiP window, and the preparation logic exists once
   * (in `AnimeHubPipController`) instead of twice.
   *
   * `num`/`den` are only a fallback: the ratio of the player the JS actually
   * selected wins when it can be measured.
   *
   * Resolves with `{ ok: true }` only when the Activity really entered PiP.
   */
  @Command
  fun enter_pip(invoke: Invoke) {
    val args = invoke.getArgs()
    val num = args.getInteger("num", 16)
    val den = args.getInteger("den", 9)
    try {
      var answered = false
      fun answer(ok: Boolean) {
        if (answered) return
        answered = true
        try {
          invoke.resolve(JSObject().put("ok", ok))
        } catch (_: Exception) {
          // The host went away between the request and its answer; the
          // preparation path already restored the page.
        }
      }
      val claimed = AnimeHubPipController.requestEnter(
        activity,
        "command",
        onResult = { entered -> answer(entered) },
        fallbackNum = num,
        fallbackDen = den,
      )
      // Unsupported device: no callback will ever fire.
      if (!claimed) answer(false)
    } catch (e: Exception) {
      invoke.reject("PiP başlatılamadı: ${e.message}", e)
    }
  }

  /**
   * Store the user's "enter PiP automatically" preference.
   *
   * The platform auto-enter flag is deliberately never enabled: Android
   * documents that it suppresses `onPictureInPictureRequested()`, so the
   * system would enter PiP before the WebView could be prepared. The
   * preference is handed to `AnimeHubPipController`, which owns the controlled
   * paths (API 30+ `onPictureInPictureRequested`, API 26-29 `onUserLeaveHint`).
   */
  @Command
  fun set_pip_auto_enter(invoke: Invoke) {
    val enabled = invoke.getArgs().getBoolean("enabled", false)
    try {
      val applied = AnimeHubPipController.applyAutoEnter(activity, enabled)
      invoke.resolve(JSObject().put("ok", applied))
    } catch (e: Exception) {
      invoke.reject("PiP ayarlanamadı: ${e.message}", e)
    }
  }

  /** Developer panel: the last Android lines, so no logcat is needed on a device. */
  @Command
  fun debug_log(invoke: Invoke) {
    val lines = org.json.JSONArray()
    AnimeHubDebugLog.snapshot().forEach { lines.put(it) }
    invoke.resolve(JSObject().put("lines", lines))
  }

  companion object {
    private const val LOG_TAG = "AnimeHubPlugin"
    private const val ANDROID_KEYSTORE = "AndroidKeyStore"
    private const val KEY_ALIAS_PREFIX = "animehub_"
    private const val TRANSFORMATION = "AES/GCM/NoPadding"
    private const val GCM_IV_LENGTH = 12
    private const val GCM_TAG_BITS = 128


    /** Return the existing key for `purpose`, generating one on first use. */
    private fun keyFor(purpose: String): SecretKey {
      val alias = "$KEY_ALIAS_PREFIX${sanitize(purpose)}"
      val ks = KeyStore.getInstance(ANDROID_KEYSTORE).apply { load(null) }
      (ks.getKey(alias, null) as? SecretKey)?.let { return it }

      val generator = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, ANDROID_KEYSTORE)
      generator.init(
        KeyGenParameterSpec.Builder(
          alias,
          KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT
        )
          .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
          .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
          .setKeySize(256)
          // The key stays in the keystore; Android may keep it in secure
          // hardware depending on the device.
          .setRandomizedEncryptionRequired(true)
          .build()
      )
      return generator.generateKey()
    }

    /** Keep a key alias inside the character set the keystore accepts. */
    private fun sanitize(purpose: String): String {
      val cleaned = purpose.filter { it.isLetterOrDigit() || it == '_' || it == '-' }.take(64)
      return if (cleaned.isEmpty()) "default" else cleaned
    }

  }
}

/**
 * Receives `window.ipc.postMessage` from the launcher only. The origin rules
 * stop the object being injected into site pages at all; the main-frame check
 * is a second, cheap guard. `Rust.ipc` gets the same arguments as wry's object:
 * the WebView id, the page URL and the message text. Message bodies are never logged.
 */
class LauncherIpc(private val webViewId: String) : WebViewCompat.WebMessageListener {
  override fun onPostMessage(
    view: WebView,
    message: WebMessageCompat,
    sourceOrigin: Uri,
    isMainFrame: Boolean,
    replyProxy: JavaScriptReplyProxy,
  ) {
    if (!isMainFrame) return
    if (!isAllowedOrigin(sourceOrigin)) {
      Log.w(LOG_TAG, "IPC blocked: unexpected origin ${sourceOrigin.scheme}")
      AnimeHubDebugLog.add("ipc", "blocked origin scheme=${sourceOrigin.scheme}")
      return
    }
    val body = message.data ?: return
    val url = view.url ?: "about:blank"
    // onPostMessage runs on the UI thread. Rust runs blocking commands inline, and
    // a plugin command waits for Kotlin code that needs the UI thread, so calling
    // Rust here can deadlock (ANR). Run it on a worker, as wry's JavaScript
    // interface did before this listener. A single thread keeps message order.
    try {
      IPC_EXECUTOR.execute {
        try {
          Rust.ipc(webViewId, url, body)
        } catch (e: Exception) {
          AnimeHubDebugLog.add("ipc", "dispatch failed (${e.javaClass.simpleName})")
        }
      }
    } catch (e: Exception) {
      AnimeHubDebugLog.add("ipc", "dispatch could not be queued (${e.javaClass.simpleName})")
    }
  }

  private fun isAllowedOrigin(origin: Uri): Boolean {
    val scheme = origin.scheme?.lowercase() ?: return false
    if (scheme != "http" && scheme != "https") return false
    val host = origin.host?.lowercase() ?: return false
    if (host != "tauri.localhost") return false
    if (origin.port != -1) return false
    // Origin must be bare: no path, query, fragment or userInfo beyond "/".
    val path = origin.path
    if (!path.isNullOrEmpty() && path != "/") return false
    if (!origin.query.isNullOrEmpty() || !origin.fragment.isNullOrEmpty()) return false
    if (!origin.userInfo.isNullOrEmpty()) return false
    return true
  }

  companion object {
    private const val LOG_TAG = "AnimeHubIpc"
    private val IPC_EXECUTOR = Executors.newSingleThreadExecutor { runnable ->
      Thread(runnable, "AnimeHubLauncherIpc").apply { isDaemon = true }
    }
    const val OBJECT_NAME = "ipc"
    val ORIGINS: Set<String> = setOf("http://tauri.localhost", "https://tauri.localhost")
  }
}
