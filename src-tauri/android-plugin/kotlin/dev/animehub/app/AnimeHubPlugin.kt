// SPDX-License-Identifier: MIT
//
// Android half of AnimeHub's Keystore + Picture-in-Picture + web-storage
// bridge.
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
// predate this note; the localStorage/IndexedDB half has NOT been compiled or
// run on a device yet (the build environment has no Android SDK/NDK). Treat
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
import android.util.Rational
import android.webkit.WebView
import app.tauri.annotation.Command
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import java.security.KeyStore
import java.util.UUID
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec
import org.json.JSONObject
import org.json.JSONTokener

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
   * Export IndexedDB with schema and common structured-clone values preserved.
   * The page work is asynchronous, so it signals completion through a random
   * one-shot window key; `evaluateJavascript` itself does not await Promises.
   */
  @Command
  fun indexeddb_export(invoke: Invoke) {
    val purpose = invoke.getArgs().getString("purpose") ?: run {
      invoke.reject("purpose gerekli"); return
    }
    val webView = sharedWebView ?: run {
      invoke.reject("WebView hazır değil"); return
    }
    val stateKey = "__animehub_idb_${UUID.randomUUID().toString().replace("-", "")}"
    val resultKey = "${stateKey}_result"
    val stateLiteral = JSONObject.quote(stateKey)
    val resultLiteral = JSONObject.quote(resultKey)
    val js = "(function(){var s=$stateLiteral,r=$resultLiteral;window[s]='pending';" +
      "($EXPORT_IDB_JS)().then(function(snapshot){if(window[s]==='cancelled'){delete window[s];delete window[r];return;}try{" +
      "var json=JSON.stringify(snapshot);var size=typeof TextEncoder==='function'?new TextEncoder().encode(json).length:json.length;" +
      "if(!json||json.length>$MAX_IDB_SNAPSHOT_CHARS||size>$MAX_IDB_SNAPSHOT_BYTES)throw new Error();" +
      "window[r]=json;window[s]='done';}catch(e){window[s]='error';}}," +
      "function(){if(window[s]==='cancelled'){delete window[s];delete window[r];return;}window[s]='error';});return true;})()"
    try {
      webView.evaluateJavascript(js) { started ->
        if (started != "true") {
          webView.evaluateJavascript(
            "(function(){window[$stateLiteral]='cancelled';delete window[$resultLiteral];return true;})()",
            null,
          )
          invoke.reject("IndexedDB dışa aktarımı başlatılamadı")
          return@evaluateJavascript
        }
        awaitJavascriptResult(webView, stateKey, resultKey) { encoded ->
          if (encoded == null) {
            invoke.reject("IndexedDB dışa aktarılamadı")
            return@awaitJavascriptResult
          }
          try {
            val json = JSONTokener(encoded).nextValue() as? String
              ?: throw IllegalArgumentException("JSON string bekleniyordu")
            if (json.toByteArray(Charsets.UTF_8).size > MAX_IDB_SNAPSHOT_BYTES) {
              throw IllegalArgumentException("IndexedDB anlık görüntüsü çok büyük")
            }
            invoke.resolve(JSObject().put("value", seal(purpose, json)))
          } catch (e: Exception) {
            invoke.reject("IndexedDB şifrelemesi başarısız: ${e.message}", e)
          }
        }
      }
    } catch (e: Exception) {
      invoke.reject("IndexedDB dışa aktarımı başlatılamadı", e)
    }
  }

  /** Restore an [`indexeddb_export`] blob, preserving its stores and indexes. */
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
      if (sealed.length > MAX_IDB_SEALED_CHARS) {
        invoke.reject("IndexedDB anlık görüntüsü çok büyük"); return
      }
      val json = openSealed(purpose, sealed)
      if (json.isEmpty()) {
        invoke.resolve(JSObject().put("ok", true)); return
      }
      if (json.toByteArray(Charsets.UTF_8).size > MAX_IDB_SNAPSHOT_BYTES) {
        invoke.reject("IndexedDB anlık görüntüsü çok büyük"); return
      }
      val webView = sharedWebView ?: run {
        invoke.reject("WebView hazır değil"); return
      }
      val payload = Base64.encodeToString(json.toByteArray(Charsets.UTF_8), Base64.NO_WRAP)
      val stateKey = "__animehub_idb_${UUID.randomUUID().toString().replace("-", "")}"
      val resultKey = "${stateKey}_result"
      val payloadKey = "${stateKey}_payload"
      val stateLiteral = JSONObject.quote(stateKey)
      val resultLiteral = JSONObject.quote(resultKey)
      val payloadLiteral = JSONObject.quote(payloadKey)
      val bootstrap = "(function(){window[$stateLiteral]='pending';window[$resultLiteral]=null;" +
        "window[$payloadLiteral]=[];return true;})()"
      webView.evaluateJavascript(bootstrap) { started ->
        if (started != "true") {
          webView.evaluateJavascript(
            "(function(){window[$stateLiteral]='cancelled';delete window[$resultLiteral];" +
              "delete window[$payloadLiteral];return true;})()",
            null,
          )
          invoke.reject("IndexedDB geri yüklemesi başlatılamadı")
          return@evaluateJavascript
        }
        awaitJavascriptResult(webView, stateKey, resultKey, payloadKey) { result ->
          if (result == "true") {
            invoke.resolve(JSObject().put("ok", true))
          } else {
            invoke.reject("IndexedDB geri yüklenemedi")
          }
        }
        appendJavascriptPayload(webView, payload, payloadKey) { uploaded ->
          if (!uploaded) {
            webView.evaluateJavascript(
              "(function(){if(window[$stateLiteral]!=='cancelled')window[$stateLiteral]='error';" +
                "delete window[$payloadLiteral];return true;})()",
              null,
            )
          } else {
            val startImport = "(function(){var s=$stateLiteral,r=$resultLiteral,p=$payloadLiteral;" +
              "if(window[s]!=='pending'||!Array.isArray(window[p]))return false;" +
              "var encoded=window[p].join('');delete window[p];" +
              "($IMPORT_IDB_JS)(encoded,s).then(function(ok){" +
              "if(window[s]==='cancelled'){delete window[s];delete window[r];return;}" +
              "window[r]=!!ok;window[s]='done';},function(){" +
              "if(window[s]==='cancelled'){delete window[s];delete window[r];return;}window[s]='error';});" +
              "return true;})()"
            try {
              webView.evaluateJavascript(startImport) { result ->
                if (result != "true") {
                  webView.evaluateJavascript(
                    "(function(){if(window[$stateLiteral]!=='cancelled')window[$stateLiteral]='error';" +
                      "delete window[$payloadLiteral];return true;})()",
                    null,
                  )
                }
              }
            } catch (e: Exception) {
              webView.evaluateJavascript(
                "(function(){if(window[$stateLiteral]!=='cancelled')window[$stateLiteral]='error';" +
                  "delete window[$payloadLiteral];return true;})()",
                null,
              )
            }
          }
        }
      }
    } catch (e: Exception) {
      invoke.reject("IndexedDB çözülemedi", e)
    }
  }

  /** Append base64 chunks without sending a Binder-sized JavaScript string. */
  private fun appendJavascriptPayload(
    webView: WebView,
    payload: String,
    payloadKey: String,
    onComplete: (Boolean) -> Unit,
  ) {
    val payloadLiteral = JSONObject.quote(payloadKey)

    fun sendNext(offset: Int) {
      if (offset >= payload.length) {
        onComplete(true)
        return
      }
      val end = minOf(offset + IDB_TRANSFER_CHUNK_CHARS, payload.length)
      val chunkLiteral = JSONObject.quote(payload.substring(offset, end))
      val js = "(function(){var p=window[$payloadLiteral];if(!Array.isArray(p))return false;" +
        "p.push($chunkLiteral);return true;})()"
      try {
        webView.evaluateJavascript(js) { result ->
          if (result == "true") sendNext(end) else onComplete(false)
        }
      } catch (_: Exception) {
        onComplete(false)
      }
    }

    sendNext(0)
  }

  /**
   * Poll a one-shot asynchronous page operation until it reports a result.
   * Large string results are fetched in bounded chunks; completion is delivered
   * on the WebView thread and timeouts cancel any pending page-side work.
   */
  private fun awaitJavascriptResult(
    webView: WebView,
    stateKey: String,
    resultKey: String,
    payloadKey: String? = null,
    onResult: (String?) -> Unit,
  ) {
    val stateLiteral = JSONObject.quote(stateKey)
    val resultLiteral = JSONObject.quote(resultKey)
    val payloadLiteral = payloadKey?.let { JSONObject.quote(it) }
    val deletePayload = payloadLiteral?.let { "delete window[$it];" } ?: ""
    val pollScript = "(function(){var s=window[$stateLiteral];" +
      "if(s==='pending')return 'pending';" +
      "if(s==='error'||s==='cancelled'){delete window[$stateLiteral];delete window[$resultLiteral];" +
      "$deletePayload return 'error';}" +
      "if(s==='done'){var r=window[$resultLiteral];delete window[$stateLiteral];" +
      "if(typeof r==='string'){$deletePayload return 'string:'+r.length;}" +
      "delete window[$resultLiteral];$deletePayload return 'value:'+String(r);}return 'missing';})()"
    val cleanupScript = "(function(){window[$stateLiteral]='cancelled';delete window[$resultLiteral];" +
      "$deletePayload return true;})()"
    val cleanupResultScript = "(function(){delete window[$resultLiteral];$deletePayload return true;})()"
    var finished = false
    var timeout: Runnable? = null

    fun finish(result: String?) {
      if (finished) return
      finished = true
      timeout?.let { webView.removeCallbacks(it) }
      onResult(result)
    }

    fun cancelPageOperation() {
      try {
        webView.evaluateJavascript(cleanupScript, null)
      } catch (_: Exception) {
        // The WebView may already be detached or navigating away.
      }
    }

    fun readChunk(offset: Int, length: Int, builder: StringBuilder) {
      if (finished) return
      if (offset >= length) {
        try {
          webView.evaluateJavascript(cleanupResultScript, null)
        } catch (_: Exception) {
          // The snapshot is already in memory; cleanup is best effort.
        }
        finish(builder.toString())
        return
      }
      val end = minOf(offset + IDB_TRANSFER_CHUNK_CHARS, length)
      val chunkScript = "(function(){var v=window[$resultLiteral];" +
        "if(typeof v!=='string'||v.length<$end)return null;return v.slice($offset,$end);})()"
      try {
        webView.evaluateJavascript(chunkScript) { raw ->
          if (finished) return@evaluateJavascript
          try {
            val chunk = raw?.let { JSONTokener(it).nextValue() as? String }
              ?: throw IllegalArgumentException("JavaScript chunk was not a string")
            if (chunk.length != end - offset) throw IllegalArgumentException("JavaScript chunk size mismatch")
            builder.append(chunk)
            readChunk(end, length, builder)
          } catch (_: Exception) {
            cancelPageOperation()
            finish(null)
          }
        }
      } catch (_: Exception) {
        cancelPageOperation()
        finish(null)
      }
    }

    timeout = Runnable {
      if (!finished) {
        cancelPageOperation()
        finish(null)
      }
    }
    if (!webView.postDelayed(timeout!!, IDB_OPERATION_TIMEOUT_MS)) {
      cancelPageOperation()
      finish(null)
      return
    }

    fun poll() {
      if (finished) return
      try {
        webView.evaluateJavascript(pollScript) { raw ->
          if (finished) return@evaluateJavascript
          try {
            val response = raw?.let { JSONTokener(it).nextValue() as? String }
              ?: throw IllegalArgumentException("JavaScript operation returned no status")
            when {
              response == "pending" -> {
                if (!webView.postDelayed({ poll() }, IDB_POLL_INTERVAL_MS)) {
                  cancelPageOperation()
                  finish(null)
                }
              }
              response == "error" || response == "missing" -> finish(null)
              response.startsWith("string:") -> {
                val length = response.removePrefix("string:").toIntOrNull()
                if (length == null || length < 0 || length > MAX_IDB_SNAPSHOT_CHARS) {
                  cancelPageOperation()
                  finish(null)
                } else {
                  readChunk(0, length, StringBuilder(length))
                }
              }
              response.startsWith("value:") -> finish(response.removePrefix("value:"))
              else -> {
                cancelPageOperation()
                finish(null)
              }
            }
          } catch (_: Exception) {
            cancelPageOperation()
            finish(null)
          }
        }
      } catch (_: Exception) {
        cancelPageOperation()
        finish(null)
      }
    }
    poll()
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
    if (combined.size < GCM_IV_LENGTH + GCM_TAG_BITS / 8) {
      throw IllegalArgumentException("Şifreli değer çok kısa")
    }
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
    private const val ANDROID_KEYSTORE = "AndroidKeyStore"
    private const val KEY_ALIAS_PREFIX = "animehub_"
    private const val TRANSFORMATION = "AES/GCM/NoPadding"
    private const val GCM_IV_LENGTH = 12
    private const val GCM_TAG_BITS = 128

    private const val IDB_OPERATION_TIMEOUT_MS = 30_000L
    private const val IDB_POLL_INTERVAL_MS = 50L
    private const val IDB_TRANSFER_CHUNK_CHARS = 48 * 1024
    private const val MAX_IDB_SNAPSHOT_BYTES = 16 * 1024 * 1024
    private const val MAX_IDB_SNAPSHOT_CHARS = MAX_IDB_SNAPSHOT_BYTES
    private const val MAX_IDB_SEALED_CHARS = 24 * 1024 * 1024

    /**
     * Export database schema and rows. Object values are encoded into
     * JSON-safe tagged records so Blob/File, dates, binary buffers and common
     * structured-clone containers survive the JavaScript bridge.
     */
    private val EXPORT_IDB_JS = """
        async function() {
          var maxBinaryBytes = 8 * 1024 * 1024;
          var maxRowsPerStore = 50000;
          var budget = {binaryBytes: 0};
          function reserveBinary(size) {
            if (size > maxBinaryBytes - budget.binaryBytes) throw new Error('binary limit');
            budget.binaryBytes += size;
          }
          function requestValue(request) {
            return new Promise(function(resolve, reject) {
              request.onsuccess = function() { resolve(request.result); };
              request.onerror = function() { reject(request.error || new Error('IDB request')); };
            });
          }
          function blobToBase64(blob) {
            return new Promise(function(resolve, reject) {
              var reader = new FileReader();
              reader.onload = function() {
                var value = String(reader.result || '');
                var comma = value.indexOf(',');
                if (comma < 0) reject(new Error('Blob encoding'));
                else resolve(value.slice(comma + 1));
              };
              reader.onerror = function() { reject(reader.error || new Error('Blob read')); };
              reader.onabort = function() { reject(new Error('Blob read aborted')); };
              reader.readAsDataURL(blob);
            });
          }
          function bytesToBase64(bytes) {
            var chunks = [];
            for (var offset = 0; offset < bytes.length; offset += 32768) {
              var end = Math.min(offset + 32768, bytes.length);
              var chunk = '';
              for (var i = offset; i < end; i++) chunk += String.fromCharCode(bytes[i]);
              chunks.push(chunk);
            }
            return btoa(chunks.join(''));
          }
          async function encodeValue(value, seen) {
            if (value === null || typeof value === 'string' || typeof value === 'boolean') return value;
            if (typeof value === 'number') {
              if (Number.isNaN(value)) return {t:'number',v:'NaN'};
              if (value === Infinity) return {t:'number',v:'Infinity'};
              if (value === -Infinity) return {t:'number',v:'-Infinity'};
              if (Object.is(value, -0)) return {t:'number',v:'-0'};
              return value;
            }
            if (typeof value === 'undefined') return {t:'undefined'};
            if (typeof value === 'bigint') return {t:'bigint',v:value.toString()};
            if (typeof value === 'function' || typeof value === 'symbol') throw new Error('unsupported value');
            if (seen.has(value)) throw new Error('cyclic value');
            seen.add(value);
            try {
              if (value instanceof Date) {
                var dateValue = value.getTime();
                return {t:'date',v:Number.isNaN(dateValue) ? 'NaN' : dateValue};
              }
              if (typeof Blob !== 'undefined' && value instanceof Blob) {
                reserveBinary(value.size);
                var isFile = typeof File !== 'undefined' && value instanceof File;
                var blobRecord = {t:isFile ? 'file' : 'blob',mime:value.type,data:await blobToBase64(value)};
                if (isFile) {
                  blobRecord.name = value.name;
                  blobRecord.lastModified = value.lastModified;
                }
                return blobRecord;
              }
              if (value instanceof ArrayBuffer) {
                reserveBinary(value.byteLength);
                return {t:'arraybuffer',data:bytesToBase64(new Uint8Array(value))};
              }
              if (ArrayBuffer.isView(value)) {
                var viewBytes = new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
                reserveBinary(viewBytes.byteLength);
                var viewKind = value instanceof DataView ? 'DataView' : value.constructor.name;
                return {t:'typedarray',kind:viewKind,data:bytesToBase64(viewBytes)};
              }
              if (Array.isArray(value)) {
                var items = [];
                for (var a = 0; a < value.length; a++) {
                  items.push(a in value ? await encodeValue(value[a], seen) : {t:'hole'});
                }
                return {t:'array',v:items};
              }
              if (value instanceof Map) {
                var entries = [];
                for (var pair of value.entries()) {
                  entries.push([await encodeValue(pair[0], seen), await encodeValue(pair[1], seen)]);
                }
                return {t:'map',v:entries};
              }
              if (value instanceof Set) {
                var members = [];
                for (var member of value.values()) members.push(await encodeValue(member, seen));
                return {t:'set',v:members};
              }
              if (value instanceof RegExp) return {t:'regexp',source:value.source,flags:value.flags,lastIndex:value.lastIndex};
              var prototype = Object.getPrototypeOf(value);
              if (prototype !== Object.prototype && prototype !== null) throw new Error('unsupported object');
              var properties = [];
              var propertyNames = Object.keys(value);
              for (var p = 0; p < propertyNames.length; p++) {
                var propertyName = propertyNames[p];
                properties.push([propertyName, await encodeValue(value[propertyName], seen)]);
              }
              return {t:'object',nullPrototype:prototype === null,v:properties};
            } finally {
              seen.delete(value);
            }
          }
          function openDatabase(name, version) {
            return new Promise(function(resolve, reject) {
              var request = typeof version === 'number' ? indexedDB.open(name, version) : indexedDB.open(name);
              request.onsuccess = function() { resolve(request.result); };
              request.onerror = function() { reject(request.error || new Error('IDB open')); };
              request.onblocked = function() { reject(new Error('IDB open blocked')); };
            });
          }
          var databaseInfo = await indexedDB.databases();
          if (!databaseInfo || typeof databaseInfo.length !== 'number') throw new Error('database listing unavailable');
          var databases = Object.create(null);
          for (var d = 0; d < databaseInfo.length; d++) {
            var info = databaseInfo[d];
            var databaseName = info.name;
            if (!databaseName) continue;
            var database = await openDatabase(databaseName, info.version);
            try {
              var stores = Object.create(null);
              for (var s = 0; s < database.objectStoreNames.length; s++) {
                var storeName = database.objectStoreNames.item(s);
                var transaction = database.transaction(storeName, 'readonly');
                var store = transaction.objectStore(storeName);
                var storeKeyPath = store.keyPath;
                var storeAutoIncrement = store.autoIncrement;
                var indexes = [];
                for (var ix = 0; ix < store.indexNames.length; ix++) {
                  var index = store.index(store.indexNames.item(ix));
                  indexes.push({name:index.name,keyPath:index.keyPath,unique:index.unique,multiEntry:index.multiEntry});
                }
                var keyRequest = store.getAllKeys(undefined, maxRowsPerStore + 1);
                var valueRequest = store.getAll(undefined, maxRowsPerStore + 1);
                var results = await Promise.all([requestValue(keyRequest), requestValue(valueRequest)]);
                var keys = results[0];
                var values = results[1];
                if (keys.length > maxRowsPerStore) throw new Error('IDB row limit');
                if (keys.length !== values.length) throw new Error('IDB row mismatch');
                var rows = [];
                for (var r = 0; r < keys.length; r++) {
                  rows.push({
                    key: await encodeValue(keys[r], new WeakSet()),
                    value: await encodeValue(values[r], new WeakSet())
                  });
                }
                stores[storeName] = {
                  keyPath:storeKeyPath,
                  autoIncrement:storeAutoIncrement,
                  indexes:indexes,
                  rows:rows
                };
              }
              databases[databaseName] = {version:database.version,stores:stores};
            } finally {
              database.close();
            }
          }
          return {format:'animehub-idb',version:2,databases:databases};
        }
    """.trimIndent()

    /** Accepts both legacy v1 row dumps and the schema-preserving v2 envelope. */
    private val IMPORT_IDB_JS = """
        async function(encoded, stateKey) {
          function ensureActive() {
            if (window[stateKey] === 'cancelled') throw new Error('cancelled');
          }
          function decodeBase64Utf8(value) {
            var binary = atob(value);
            var bytes = new Uint8Array(binary.length);
            for (var i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
            if (typeof TextDecoder !== 'undefined') return new TextDecoder('utf-8', {fatal:true}).decode(bytes);
            throw new Error('UTF-8 decoder unavailable');
          }
          function bytesFromBase64(value) {
            var binary = atob(value);
            var bytes = new Uint8Array(binary.length);
            for (var i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
            return bytes;
          }
          function decodeValue(value) {
            if (value === null || typeof value !== 'object' || !value.t) return value;
            switch (value.t) {
              case 'undefined': return undefined;
              case 'hole': return undefined;
              case 'number':
                if (value.v === 'NaN') return NaN;
                if (value.v === 'Infinity') return Infinity;
                if (value.v === '-Infinity') return -Infinity;
                if (value.v === '-0') return -0;
                throw new Error('bad number');
              case 'bigint': return BigInt(value.v);
              case 'date': return new Date(value.v === 'NaN' ? NaN : value.v);
              case 'blob': return new Blob([bytesFromBase64(value.data)], {type:value.mime || ''});
              case 'file':
                if (typeof File === 'undefined') throw new Error('File unavailable');
                return new File([bytesFromBase64(value.data)], value.name || '', {
                  type:value.mime || '',lastModified:value.lastModified || 0
                });
              case 'arraybuffer': return bytesFromBase64(value.data).buffer;
              case 'typedarray': {
                var bytes = bytesFromBase64(value.data);
                if (value.kind === 'DataView') return new DataView(bytes.buffer);
                var allowed = ['Int8Array','Uint8Array','Uint8ClampedArray','Int16Array','Uint16Array',
                  'Int32Array','Uint32Array','Float32Array','Float64Array','BigInt64Array','BigUint64Array'];
                if (allowed.indexOf(value.kind) < 0 || typeof window[value.kind] !== 'function') throw new Error('typed array unavailable');
                return new window[value.kind](bytes.buffer);
              }
              case 'array': {
                var array = new Array(value.v.length);
                for (var a = 0; a < value.v.length; a++) {
                  if (!value.v[a] || value.v[a].t !== 'hole') array[a] = decodeValue(value.v[a]);
                }
                return array;
              }
              case 'map': {
                var map = new Map();
                value.v.forEach(function(pair) { map.set(decodeValue(pair[0]), decodeValue(pair[1])); });
                return map;
              }
              case 'set': {
                var set = new Set();
                value.v.forEach(function(member) { set.add(decodeValue(member)); });
                return set;
              }
              case 'regexp': {
                var expression = new RegExp(value.source, value.flags);
                expression.lastIndex = value.lastIndex || 0;
                return expression;
              }
              case 'object': {
                var object = value.nullPrototype ? Object.create(null) : {};
                value.v.forEach(function(entry) {
                  Object.defineProperty(object, entry[0], {
                    value:decodeValue(entry[1]),enumerable:true,writable:true,configurable:true
                  });
                });
                return object;
              }
              default: throw new Error('unknown value encoding');
            }
          }
          function sameKeyPath(left, right) {
            return JSON.stringify(left) === JSON.stringify(right);
          }
          function databaseHasStores(database, stores) {
            var storeNames = Object.keys(stores);
            for (var i = 0; i < storeNames.length; i++) {
              if (!database.objectStoreNames.contains(storeNames[i])) return false;
            }
            return true;
          }
          function databaseMatchesSchema(database, stores) {
            var storeNames = Object.keys(stores);
            var matches = database.objectStoreNames.length === storeNames.length;
            for (var s = 0; matches && s < storeNames.length; s++) {
              var storeName = storeNames[s];
              var expected = stores[storeName];
              if (!expected || Array.isArray(expected) || !database.objectStoreNames.contains(storeName)) {
                matches = false;
                break;
              }
              var store = database.transaction(storeName, 'readonly').objectStore(storeName);
              if (!sameKeyPath(store.keyPath, expected.keyPath) ||
                  store.autoIncrement !== !!expected.autoIncrement) {
                matches = false;
                break;
              }
              var expectedIndexes = Array.isArray(expected.indexes) ? expected.indexes : [];
              if (store.indexNames.length !== expectedIndexes.length) {
                matches = false;
                break;
              }
              for (var i = 0; matches && i < expectedIndexes.length; i++) {
                var expectedIndex = expectedIndexes[i];
                if (!store.indexNames.contains(expectedIndex.name)) {
                  matches = false;
                  break;
                }
                var actualIndex = store.index(expectedIndex.name);
                if (!sameKeyPath(actualIndex.keyPath, expectedIndex.keyPath) ||
                    actualIndex.unique !== !!expectedIndex.unique ||
                    actualIndex.multiEntry !== !!expectedIndex.multiEntry) matches = false;
              }
            }
            return matches;
          }
          function openDatabase(name, spec, version, replaceSchema, stateKey) {
            return new Promise(function(resolve, reject) {
              var request = typeof version === 'number' ? indexedDB.open(name, version) : indexedDB.open(name);
              request.onupgradeneeded = function() {
                try {
                  if (window[stateKey] === 'cancelled') {
                    request.transaction.abort();
                    return;
                  }
                  var database = request.result;
                  var transaction = request.transaction;
                  if (replaceSchema) {
                    while (database.objectStoreNames.length) {
                      database.deleteObjectStore(database.objectStoreNames.item(0));
                    }
                  }
                  Object.keys(spec.stores || {}).forEach(function(storeName) {
                    var storeInfo = spec.stores[storeName];
                    var legacy = Array.isArray(storeInfo);
                    var schema = legacy ? null : storeInfo;
                    var store;
                    if (!database.objectStoreNames.contains(storeName)) {
                      var options = {};
                      if (schema && schema.keyPath !== null && schema.keyPath !== undefined) options.keyPath = schema.keyPath;
                      if (schema && schema.autoIncrement) options.autoIncrement = true;
                      store = database.createObjectStore(storeName, options);
                    } else {
                      store = transaction.objectStore(storeName);
                    }
                    if (schema && Array.isArray(schema.indexes)) {
                      schema.indexes.forEach(function(index) {
                        if (!store.indexNames.contains(index.name)) {
                          store.createIndex(index.name, index.keyPath, {
                            unique:!!index.unique,multiEntry:!!index.multiEntry
                          });
                        }
                      });
                    }
                  });
                } catch (error) {
                  try { request.transaction.abort(); } catch (_) {}
                }
              };
              request.onsuccess = function() {
                if (window[stateKey] === 'cancelled') {
                  request.result.close();
                  reject(new Error('cancelled'));
                } else {
                  resolve(request.result);
                }
              };
              request.onerror = function() { reject(request.error || new Error('IDB open')); };
              // The open request cannot be canceled when blocked. Leave it pending;
              // if the bridge times out, onupgradeneeded will abort after unblocking.
              request.onblocked = function() {};
            });
          }
          ensureActive();
          var parsed = JSON.parse(decodeBase64Utf8(encoded));
          var isV2 = !!(parsed && parsed.format === 'animehub-idb' && parsed.version === 2 && parsed.databases);
          var databases = isV2 ? parsed.databases : parsed;
          if (!databases || typeof databases !== 'object') throw new Error('bad snapshot');
          var databaseNames = Object.keys(databases);
          for (var d = 0; d < databaseNames.length; d++) {
            ensureActive();
            var name = databaseNames[d];
            var spec = databases[name];
            if (!spec || !spec.stores || typeof spec.stores !== 'object') throw new Error('bad database');
            var savedVersion = Number(spec.version);
            if (!Number.isSafeInteger(savedVersion) || savedVersion < 1) savedVersion = 1;
            var database = await openDatabase(name, spec, undefined, isV2, stateKey);
            try {
              ensureActive();
              var compatibleSchema = isV2
                ? databaseMatchesSchema(database, spec.stores)
                : databaseHasStores(database, spec.stores);
              if (database.version < savedVersion || !compatibleSchema) {
                var version = Math.max(
                  savedVersion,
                  !compatibleSchema ? database.version + 1 : database.version
                );
                database.close();
                database = await openDatabase(name, spec, version, isV2, stateKey);
                ensureActive();
              }
              var storeNames = Object.keys(spec.stores || {});
              for (var s = 0; s < storeNames.length; s++) {
                ensureActive();
                var storeName = storeNames[s];
                var storeInfo = spec.stores[storeName];
                var legacy = Array.isArray(storeInfo);
                var schema = legacy ? null : storeInfo;
                var rows = legacy ? storeInfo : storeInfo.rows;
                if (!Array.isArray(rows)) throw new Error('bad rows');
                ensureActive();
                await new Promise(function(resolve, reject) {
                  var transaction = database.transaction(storeName, 'readwrite');
                  var store = transaction.objectStore(storeName);
                  transaction.oncomplete = function() { resolve(); };
                  transaction.onerror = function() { reject(transaction.error || new Error('IDB write')); };
                  transaction.onabort = function() { reject(transaction.error || new Error('IDB aborted')); };
                  try {
                    ensureActive();
                    store.clear();
                    rows.forEach(function(row) {
                      ensureActive();
                      var value = isV2 ? decodeValue(row.value) : row.value;
                      var keyPath = schema ? schema.keyPath : store.keyPath;
                      if (keyPath !== null && keyPath !== undefined) store.put(value);
                      else store.put(value, isV2 ? decodeValue(row.key) : row.key);
                    });
                  } catch (error) {
                    try { transaction.abort(); } catch (_) {}
                    reject(error);
                  }
                });
              }
            } finally {
              database.close();
            }
          }
          return true;
        }
    """.trimIndent()


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
