// SPDX-License-Identifier: MIT
//
// Android half of AnimeHub's Keystore + Picture-in-Picture + cookie +
// web-storage bridge.
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
// NOT been run on a device yet; the CookieManager and localStorage/IndexedDB
// bridges have not been compiled or run on a device either (the build
// environment has no Android SDK/NDK). Treat it as unverified until a real
// `./scripts/build.sh android` / `tauri android build` has been installed on
// hardware. Only the JS half of PiP has automated tests
// (`tests/pip_controller.test.js`).
//
// STORAGE ISOLATION CONTRACT (localStorage/IndexedDB commands)
// ------------------------------------------------------------
// The Rust side starts exports only while the site page is loaded and imports
// after the target page's URL is observed. WebView operations run on Android's
// UI thread. IndexedDB export/import remains best-effort: the scripts use
// asynchronous IndexedDB APIs and do not provide full database fidelity.
// Keep restored payloads JSON-encoded; never interpolate raw page/user strings.

package dev.animehub.app

import android.app.Activity
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.Base64
import android.graphics.Bitmap
import android.net.Uri
import android.util.Log
import android.webkit.CookieManager
import android.webkit.WebResourceError
import android.webkit.WebResourceRequest
import android.webkit.WebResourceResponse
import android.webkit.WebView
import android.webkit.WebViewClient
import androidx.appcompat.app.AppCompatActivity
import app.tauri.annotation.Command
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import java.security.KeyStore
import java.util.concurrent.Executors
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

@TauriPlugin
class AnimeHubPlugin(private val activity: Activity) : Plugin(activity) {

  /** The shared WebView, handed to the plugin by Tauri at startup. */
  @Volatile
  private var sharedWebView: WebView? = null

  override fun load(webView: WebView) {
    // On Android the launcher and the sites share this single WebView, so
    // keeping the reference is all the storage commands need.
    sharedWebView = webView
    installNavigationGuard(webView)
  }

  /**
   * Wrap the WebView's client so top-level navigations pass the site policy.
   *
   * Tauri calls `load` from its `on_webview_created` hook, which runs after
   * wry has set its own `RustWebViewClient`, so the current client is the one
   * to wrap. Wry keeps its IPC object bound to that client, so it must stay in
   * place as the delegate. See `SiteNavigationGuard` for the limits.
   */
  private fun installNavigationGuard(webView: WebView) {
    val current = webView.webViewClient
    if (current is SiteNavigationGuard) return
    webView.webViewClient = SiteNavigationGuard(current)
  }

  override fun onDestroy(activity: AppCompatActivity) {
    // Plugin instances can outlive their Activity. Reject later WebView work
    // rather than keep a reference to the destroyed native View.
    if (activity === this.activity) sharedWebView = null
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

  /** Persist CookieManager state away from Android's UI thread. */
  private fun flushCookieStoreAsync(manager: CookieManager) {
    try {
      COOKIE_FLUSH_EXECUTOR.execute {
        try {
          manager.flush()
        } catch (_: Exception) {
          Log.w(LOG_TAG, "CookieManager persistence sync failed")
        }
      }
    } catch (_: Exception) {
      Log.w(LOG_TAG, "CookieManager persistence sync could not be queued")
    }
  }

  /** Run WebView work on Android's UI thread and reject stale Activity/View handles. */
  private fun withLiveWebView(invoke: Invoke, action: (WebView) -> Unit) {
    val webView = sharedWebView ?: run {
      invoke.reject("WebView hazır değil")
      return
    }
    withLiveActivity(invoke) {
      if (sharedWebView !== webView) {
        invoke.reject("WebView artık kullanılamıyor")
        return@withLiveActivity
      }
      try {
        action(webView)
      } catch (e: Exception) {
        invoke.reject("WebView işlemi başarısız", e)
      }
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

  // ------------------------------------------------------- Android cookies

  /** Read the current cookie header. Android returns null when there are no cookies. */
  @Command
  fun webview_cookies_get(invoke: Invoke) {
    val url = invoke.getArgs().getString("url") ?: run {
      invoke.reject("url gerekli"); return
    }
    val parsed = android.net.Uri.parse(url)
    if ((parsed.scheme != "http" && parsed.scheme != "https") || parsed.host.isNullOrBlank()) {
      invoke.reject("HTTP(S) URL gerekli"); return
    }
    withLiveActivity(invoke) {
      try {
        // CookieManager.getCookie legitimately returns null for an empty jar.
        val header = CookieManager.getInstance().getCookie(url) ?: ""
        invoke.resolve(JSObject().put("value", header))
      } catch (_: Exception) {
        invoke.reject("Android çerezleri okunamadı")
      }
    }
  }

  /** Replace the shared CookieManager jar; disk persistence is queued off the UI thread. */
  @Command
  fun webview_cookies_replace(invoke: Invoke) {
    val args = invoke.getArgs()
    val url = args.getString("url") ?: run {
      invoke.reject("url gerekli"); return
    }
    val cookieHeaders = try {
      val array = args.getJSONArray("cookies")
      ArrayList<String>(array.length()).apply {
        for (index in 0 until array.length()) add(array.getString(index))
      }
    } catch (_: Exception) {
      invoke.reject("çerez listesi geçersiz"); return
    }
    if (cookieHeaders.isNotEmpty()) {
      val parsed = android.net.Uri.parse(url)
      if ((parsed.scheme != "http" && parsed.scheme != "https") || parsed.host.isNullOrBlank()) {
        invoke.reject("HTTP(S) URL gerekli"); return
      }
    }

    withLiveActivity(invoke) {
      try {
        val manager = CookieManager.getInstance()
        val startedAt = android.os.SystemClock.elapsedRealtime()
        // removeAllCookies is asynchronous; restoring/navigating before its
        // callback would let the outgoing provider's cookies leak forward.
        manager.removeAllCookies {
          if (rejectIfActivityUnavailable(invoke)) return@removeAllCookies
          var rejected = 0
          fun setNext(index: Int) {
            if (index >= cookieHeaders.size) {
              if (rejected > 0) {
                Log.w(LOG_TAG, "CookieManager rejected $rejected restored cookie(s)")
              }
              val elapsedMs = android.os.SystemClock.elapsedRealtime() - startedAt
              Log.d(
                LOG_TAG,
                "Cookie jar ready: ${cookieHeaders.size} cookie(s), ${elapsedMs}ms"
              )
              // flush() blocks until disk I/O is complete. CookieManager's
              // in-memory jar is already ready for the upcoming navigation;
              // persist non-empty restored jars asynchronously. An empty jar
              // needs no disk flush; every later site open replaces it again.
              invoke.resolve(JSObject().put("ok", true))
              if (cookieHeaders.isNotEmpty()) flushCookieStoreAsync(manager)
              return
            }
            try {
              manager.setCookie(url, cookieHeaders[index]) { accepted ->
                if (rejectIfActivityUnavailable(invoke)) return@setCookie
                if (accepted != true) rejected += 1
                setNext(index + 1)
              }
            } catch (_: Exception) {
              // Avoid leaving a partial target jar available to the page
              // that was active before the provider switch.
              try {
                manager.removeAllCookies {
                  if (!rejectIfActivityUnavailable(invoke)) {
                    invoke.reject("Android çerezleri geri yüklenemedi")
                  }
                }
              } catch (_: Exception) {
                invoke.reject("Android çerezleri geri yüklenemedi")
              }
            }
          }
          setNext(0)
        }
      } catch (_: Exception) {
        invoke.reject("Android çerezleri temizlenemedi")
      }
    }
  }

  // ------------------------------------------------- localStorage / IndexedDB

  /**
   * Dump the loaded page's `localStorage` and resolve with the Keystore-sealed
   * base64 blob.
   *
   * The script returns the object itself (not a `JSON.stringify` string), so
   * the `evaluateJavascript` callback receives exactly the JSON text we want
   * to encrypt — no double encoding to undo.
   */
  @Command
  fun localstorage_export(invoke: Invoke) {
    val purpose = invoke.getArgs().getString("purpose") ?: run {
      invoke.reject("purpose gerekli"); return
    }
    withLiveWebView(invoke) { webView ->
      webView.evaluateJavascript(
        "(function(){var o={};for(var i=0;i<localStorage.length;i++){" +
          "var k=localStorage.key(i);o[k]=localStorage.getItem(k);}return o;})()"
      ) { result ->
        if (rejectIfActivityUnavailable(invoke)) return@evaluateJavascript
        if (result == null || result == "null") {
          // Context torn down mid-evaluation (navigation won the race): report
          // an empty payload rather than half a jar.
          invoke.resolve(JSObject().put("value", ""))
          return@evaluateJavascript
        }
        try {
          invoke.resolve(JSObject().put("value", seal(purpose, result)))
        } catch (e: Exception) {
          invoke.reject("localStorage şifrelemesi başarısız: ${e.message}", e)
        }
      }
    }
  }

  /** Restore a [`localstorage_export`] blob into the loaded page. */
  @Command
  fun localstorage_import(invoke: Invoke) {
    val args = invoke.getArgs()
    val purpose = args.getString("purpose") ?: run {
      invoke.reject("purpose gerekli"); return
    }
    val sealed = args.getString("value") ?: run {
      invoke.reject("value gerekli"); return
    }
    try {
      val json = openSealed(purpose, sealed)
      if (json.isEmpty()) {
        invoke.resolve(JSObject().put("ok", true)); return
      }
      // Valid JSON is a valid JS object literal, so the payload is embedded
      // directly — never interpolated as source text.
      val js = "(function(){try{var d=$json;localStorage.clear();" +
        "for(var k in d){localStorage.setItem(k,d[k]);}}catch(e){}})();"
      withLiveWebView(invoke) { webView ->
        webView.evaluateJavascript(js, null)
        invoke.resolve(JSObject().put("ok", true))
      }
    } catch (e: Exception) {
      invoke.reject("localStorage çözülemedi", e)
    }
  }

  /**
   * Dump the loaded page's IndexedDB databases (best effort) and resolve with
   * the Keystore-sealed base64 blob.
   *
   * Fidelity notes, by design: document *values* are captured via
   * `getAll`/`getAllKeys` pairs, so original keys survive; secondary indexes,
   * key paths and auto-increment counters are **not** reconstructed, and
   * structured-clone-only values (Blob, etc.) are dropped by
   * `JSON.stringify`. Good enough for the auth/setting state sites in scope
   * keep in IndexedDB; anything richer is a documented limitation.
   */
  @Command
  fun indexeddb_export(invoke: Invoke) {
    val purpose = invoke.getArgs().getString("purpose") ?: run {
      invoke.reject("purpose gerekli"); return
    }
    // Keep the existing export script and callback contract, but run the
    // WebView call on Android's required UI thread.
    withLiveWebView(invoke) { webView ->
      webView.evaluateJavascript(EXPORT_IDB_JS) { result ->
        if (rejectIfActivityUnavailable(invoke)) return@evaluateJavascript
        if (result == null || result == "null") {
          invoke.resolve(JSObject().put("value", ""))
          return@evaluateJavascript
        }
        try {
          invoke.resolve(JSObject().put("value", seal(purpose, result)))
        } catch (e: Exception) {
          invoke.reject("IndexedDB şifrelemesi başarısız: ${e.message}", e)
        }
      }
    }
  }

  /** Restore an [`indexeddb_export`] blob into the loaded page. */
  @Command
  fun indexeddb_import(invoke: Invoke) {
    val args = invoke.getArgs()
    val purpose = args.getString("purpose") ?: run {
      invoke.reject("purpose gerekli"); return
    }
    val sealed = args.getString("value") ?: run {
      invoke.reject("value gerekli"); return
    }
    try {
      val json = openSealed(purpose, sealed)
      if (json.isEmpty()) {
        invoke.resolve(JSObject().put("ok", true)); return
      }
      // Same no-interpolation rule: the payload becomes a parsed literal.
      val js = "(async function(){try{var d=$json;" +
        "for(var name in d){var spec=d[name];" +
        "var db=await new Promise(function(res,rej){var r=indexedDB.open(name,spec.version||1);" +
        "r.onupgradeneeded=function(){var b=r.result;" +
        "for(var sn in spec.stores){if(!b.objectStoreNames.contains(sn))b.createObjectStore(sn);}};" +
        "r.onsuccess=function(){res(r.result)};r.onerror=function(){rej(r.error)}});" +
        "for(var sn in spec.stores){await new Promise(function(res,rej){" +
        "var tx=db.transaction(sn,'readwrite');var st=tx.objectStore(sn);" +
        "spec.stores[sn].forEach(function(row){st.put(row.value,row.key);});" +
        "tx.oncomplete=function(){res()};tx.onerror=function(){rej(tx.error)}});}" +
        "db.close();}}catch(e){}})();"
      withLiveWebView(invoke) { webView ->
        webView.evaluateJavascript(js, null)
        invoke.resolve(JSObject().put("ok", true))
      }
    } catch (e: Exception) {
      invoke.reject("IndexedDB çözülemedi", e)
    }
  }

  /** Seal a JSON payload with the Keystore key for `purpose` (IV-prefixed b64). */
  private fun seal(purpose: String, plaintext: String): String {
    val cipher = Cipher.getInstance(TRANSFORMATION)
    cipher.init(Cipher.ENCRYPT_MODE, keyFor(purpose))
    val ct = cipher.doFinal(plaintext.toByteArray(Charsets.UTF_8))
    return Base64.encodeToString(cipher.iv + ct, Base64.NO_WRAP)
  }

  /** Open a [`seal`] blob; an empty payload short-circuits to `""`. */
  private fun openSealed(purpose: String, sealed: String): String {
    if (sealed.isEmpty()) return ""
    val combined = Base64.decode(sealed, Base64.NO_WRAP)
    if (combined.size <= GCM_IV_LENGTH) return ""
    val iv = combined.copyOfRange(0, GCM_IV_LENGTH)
    val ct = combined.copyOfRange(GCM_IV_LENGTH, combined.size)
    val cipher = Cipher.getInstance(TRANSFORMATION)
    cipher.init(Cipher.DECRYPT_MODE, keyFor(purpose), GCMParameterSpec(GCM_TAG_BITS, iv))
    return String(cipher.doFinal(ct), Charsets.UTF_8)
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

  /** Developer panel: the last PiP lines, so no logcat is needed on a device. */
  @Command
  fun pip_debug_log(invoke: Invoke) {
    val lines = org.json.JSONArray()
    AnimeHubPipLog.snapshot().forEach { lines.put(it) }
    invoke.resolve(JSObject().put("lines", lines))
  }

  companion object {
    private const val LOG_TAG = "AnimeHubPlugin"
    private val COOKIE_FLUSH_EXECUTOR = Executors.newSingleThreadExecutor { runnable ->
      Thread(runnable, "AnimeHubCookieFlush").apply { isDaemon = true }
    }
    private const val ANDROID_KEYSTORE = "AndroidKeyStore"
    private const val KEY_ALIAS_PREFIX = "animehub_"
    private const val TRANSFORMATION = "AES/GCM/NoPadding"
    private const val GCM_IV_LENGTH = 12
    private const val GCM_TAG_BITS = 128

    /**
     * Best-effort dump of every IndexedDB database on the current origin:
     * `{ "<db>": { "version": n, "stores": { "<store>": [ {key, value} ] } } }`.
     * Returns `"{}"` (never `null`) on any internal failure so the caller can
     * treat a missing dump and an empty one identically.
     */
    private val EXPORT_IDB_JS = """
        (async function(){
          var out={};
          function openDb(name,version){return new Promise(function(res,rej){
            var r=indexedDB.open(name,version||undefined);
            r.onsuccess=function(){res(r.result)};r.onerror=function(){rej(r.error)};});}
          try{
            var dbs=await indexedDB.databases();
            for(var i=0;i<dbs.length;i++){
              var name=dbs[i].name;if(!name)continue;
              var db=await openDb(name,dbs[i].version);
              var stores={};
              for(var j=0;j<db.objectStoreNames.length;j++){
                var sn=db.objectStoreNames[j];
                var rows=await new Promise(function(res,rej){
                  var tx=db.transaction(sn,'readonly');
                  var st=tx.objectStore(sn);
                  var keys=st.getAllKeys();var vals=st.getAll();
                  var failed=function(){rej(keys.error||vals.error||new Error('getAll'))};
                  keys.onsuccess=function(){vals.onsuccess=function(){
                    res(vals.result.map(function(v,k){return{key:keys.result[k],value:v}}))};
                    vals.onerror=failed};
                  keys.onerror=failed;vals.onerror=failed;});
                stores[sn]=rows;
              }
              out[name]={version:db.version,stores:stores};
              db.close();
            }
          }catch(e){return '{}';}
          return out;
        })()
    """.trimIndent().replace("\n", "")

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
    if (!SiteNavigationPolicy.allows(uri)) {
      // Host and scheme only: paths and queries can carry tokens.
      Log.w(LOG_TAG, "navigation blocked: ${uri.scheme}://${uri.host}")
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

  override fun onReceivedError(view: WebView, request: WebResourceRequest, error: WebResourceError) =
    inner.onReceivedError(view, request, error)

  companion object {
    private const val LOG_TAG = "AnimeHubNavGuard"
  }
}

/**
 * The desktop site policy, restated for Kotlin. Keep it in step with
 * `navigation_allowed` and `is_private_host` in `src-tauri/src/sites/url_policy.rs`;
 * `tests/gaps.test.js` checks the shared host suffixes.
 *
 * Deliberately has no exception for the launcher's own origin: a site must not
 * be able to load `tauri.localhost` into the WebView, because that origin is
 * the one that receives IPC. The launcher returns through `loadUrl`, which is
 * not routed through this guard.
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

  fun allows(uri: Uri): Boolean {
    val scheme = uri.scheme?.lowercase() ?: return false
    return when (scheme) {
      "https" -> hostAllowed(uri.host)
      // Playback and inline-asset schemes, as in Rust's navigation_allowed.
      "blob", "data", "about" -> true
      // Plain http is a downgrade, and every other scheme is refused.
      else -> false
    }
  }

  private fun hostAllowed(rawHost: String?): Boolean {
    val host = rawHost?.lowercase()?.trimEnd('.')
    if (host.isNullOrEmpty()) return false
    val bare = host.removePrefix("[").removeSuffix("]")
    if (bare.contains(':') || IPV4.matches(bare)) return isPublicIp(bare)
    if (host in BLOCKED_HOSTS) return false
    return BLOCKED_SUFFIXES.none { host.endsWith(it) }
  }

  private fun isPublicIp(ip: String): Boolean =
    if (ip.contains(':')) isPublicIpv6(ip) else isPublicIpv4(ip)

  private fun isPublicIpv4(ip: String): Boolean {
    val octets = ip.split('.').map { it.toIntOrNull() ?: return false }
    if (octets.size != 4 || octets.any { it !in 0..255 }) return false
    val a = octets[0]
    val b = octets[1]
    return !(
      a == 0 ||                         // 0.0.0.0/8
      a == 10 ||                        // private
      a == 127 ||                       // loopback
      (a == 100 && b in 64..127) ||     // CGNAT
      (a == 169 && b == 254) ||         // link-local
      (a == 172 && b in 16..31) ||      // private
      (a == 192 && b == 0) ||           // IETF protocol assignments, documentation
      (a == 192 && b == 168) ||         // private
      (a == 198 && (b == 18 || b == 19)) || // benchmarking
      a >= 224                          // multicast, reserved, broadcast
    )
  }

  private fun isPublicIpv6(ip: String): Boolean {
    val lower = ip.lowercase()
    if (lower == "::1" || lower == "::") return false
    if (lower.startsWith("::ffff:")) {
      val v4 = lower.removePrefix("::ffff:")
      return IPV4.matches(v4) && isPublicIpv4(v4)
    }
    val first = lower.substringBefore(':').ifEmpty { "0" }.toIntOrNull(16) ?: return false
    return !(
      (first and 0xffc0) == 0xfe80 ||   // link-local fe80::/10
      (first and 0xfe00) == 0xfc00 ||   // unique local fc00::/7
      (first and 0xff00) == 0xff00      // multicast ff00::/8
    )
  }
}
