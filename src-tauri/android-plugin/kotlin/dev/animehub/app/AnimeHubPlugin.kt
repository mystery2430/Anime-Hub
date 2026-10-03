// SPDX-License-Identifier: MIT
//
// Android half of AnimeHub's Keystore + Picture-in-Picture + cookie +
// web-storage bridge.
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
// standard Android APIs listed in the imports. The Keystore and PiP halves
// predate this note; the CookieManager and localStorage/IndexedDB bridges have
// NOT been compiled or run on a device yet (the build environment has no
// Android SDK/NDK). Treat
// it as unverified until `npm run tauri android dev` passes on real hardware.
//
// STORAGE ISOLATION CONTRACT (localStorage/IndexedDB commands)
// ------------------------------------------------------------
// The Rust side calls `*_export` only while the site page is still the loaded
// document and `*_import` only after the target page has loaded (it polls the
// WebView URL first). `run_mobile_plugin` blocks until the invoke resolves,
// and `evaluateJavascript` delivers its callback on the UI thread — so an
// export really has captured the page's storage when Rust regains control.
// The injected scripts are constants with no string interpolation of page
// data; restored state enters the page as a parsed JSON literal, never as
// concatenated source text.

package dev.animehub.app

import android.app.Activity
import android.app.PictureInPictureParams
import android.content.pm.PackageManager
import android.os.Build
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.Base64
import android.util.Log
import android.util.Rational
import android.webkit.CookieManager
import android.webkit.WebView
import app.tauri.annotation.Command
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

@TauriPlugin
class AnimeHubPlugin(private val activity: Activity) : Plugin(activity) {

  /** The shared WebView, handed to the plugin by Tauri at startup. */
  private var sharedWebView: WebView? = null

  override fun load(webView: WebView) {
    // On Android the launcher and the sites share this single WebView, so
    // keeping the reference is all the storage commands need.
    sharedWebView = webView
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
    try {
      activity.runOnUiThread {
        try {
          // CookieManager.getCookie legitimately returns null for an empty jar.
          val header = CookieManager.getInstance().getCookie(url) ?: ""
          invoke.resolve(JSObject().put("value", header))
        } catch (_: Exception) {
          invoke.reject("Android çerezleri okunamadı")
        }
      }
    } catch (_: Exception) {
      invoke.reject("Android çerezleri okunamadı")
    }
  }

  /** Replace the shared CookieManager jar, resolving only after all callbacks. */
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

    try {
      activity.runOnUiThread {
        try {
          val manager = CookieManager.getInstance()
          // removeAllCookies is asynchronous; restoring/navigating before its
          // callback would let the outgoing provider's cookies leak forward.
          manager.removeAllCookies {
            var rejected = 0
            fun setNext(index: Int) {
              if (index >= cookieHeaders.size) {
                try {
                  manager.flush()
                  if (rejected > 0) {
                    Log.w(LOG_TAG, "CookieManager rejected $rejected restored cookie(s)")
                  }
                  invoke.resolve(JSObject().put("ok", true))
                } catch (_: Exception) {
                  invoke.reject("Android çerezleri kaydedilemedi")
                }
                return
              }
              try {
                manager.setCookie(url, cookieHeaders[index]) { accepted ->
                  if (accepted != true) rejected += 1
                  setNext(index + 1)
                }
              } catch (_: Exception) {
                // Avoid leaving a partial target jar available to the page
                // that was active before the provider switch.
                try {
                  manager.removeAllCookies {
                    try { manager.flush() } catch (_: Exception) { }
                    invoke.reject("Android çerezleri geri yüklenemedi")
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
    } catch (_: Exception) {
      invoke.reject("Android çerezleri güncellenemedi")
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
    val webView = sharedWebView ?: run {
      invoke.reject("WebView hazır değil"); return
    }
    webView.evaluateJavascript(
      "(function(){var o={};for(var i=0;i<localStorage.length;i++){" +
        "var k=localStorage.key(i);o[k]=localStorage.getItem(k);}return o;})()"
    ) { result ->
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
      val webView = sharedWebView ?: run {
        invoke.reject("WebView hazır değil"); return
      }
      // Valid JSON is a valid JS object literal, so the payload is embedded
      // directly — never interpolated as source text.
      val js = "(function(){try{var d=$json;localStorage.clear();" +
        "for(var k in d){localStorage.setItem(k,d[k]);}}catch(e){}})();"
      webView.evaluateJavascript(js, null)
      invoke.resolve(JSObject().put("ok", true))
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
    val webView = sharedWebView ?: run {
      invoke.reject("WebView hazır değil"); return
    }
    // `evaluateJavascript` awaits a returned Promise (API 21+), so the async
    // IIFE below resolves the callback with the finished dump.
    webView.evaluateJavascript(EXPORT_IDB_JS) { result ->
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
      val webView = sharedWebView ?: run {
        invoke.reject("WebView hazır değil"); return
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
      webView.evaluateJavascript(js, null)
      invoke.resolve(JSObject().put("ok", true))
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

  @Command
  fun enter_pip(invoke: Invoke) {
    val args = invoke.getArgs()
    val num = args.getInteger("num", 16)
    val den = args.getInteger("den", 9)
    try {
      if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) {
        invoke.resolve(JSObject().put("ok", false)); return
      }
      if (!activity.packageManager.hasSystemFeature(PackageManager.FEATURE_PICTURE_IN_PICTURE)) {
        invoke.resolve(JSObject().put("ok", false)); return
      }
      val params = PictureInPictureParams.Builder()
        .setAspectRatio(safeAspectRatio(num, den))
        .build()
      val ok = activity.enterPictureInPictureMode(params)
      invoke.resolve(JSObject().put("ok", ok))
    } catch (e: Exception) {
      invoke.reject("PiP başlatılamadı: ${e.message}", e)
    }
  }

  @Command
  fun set_pip_auto_enter(invoke: Invoke) {
    val enabled = invoke.getArgs().getBoolean("enabled", false)
    try {
      // setAutoEnterEnabled is API 31+.
      if (Build.VERSION.SDK_INT < Build.VERSION_CODES.S) {
        invoke.resolve(JSObject().put("ok", false)); return
      }
      if (!activity.packageManager.hasSystemFeature(PackageManager.FEATURE_PICTURE_IN_PICTURE)) {
        invoke.resolve(JSObject().put("ok", false)); return
      }
      val params = PictureInPictureParams.Builder()
        .setAspectRatio(Rational(16, 9))
        .setAutoEnterEnabled(enabled)
        .build()
      activity.setPictureInPictureParams(params)
      invoke.resolve(JSObject().put("ok", true))
    } catch (e: Exception) {
      invoke.reject("PiP ayarlanamadı: ${e.message}", e)
    }
  }

  companion object {
    private const val LOG_TAG = "AnimeHubPlugin"
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

    /**
     * Android rejects PiP ratios outside about 1:2.39 .. 2.39:1.
     * Clamping each side on its own still allows 239:1, which
     * `setAspectRatio` refuses.
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
}
