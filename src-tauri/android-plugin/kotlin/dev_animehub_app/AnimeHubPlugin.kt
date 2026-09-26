// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Android half of AnimeHub's Keystore + Picture-in-Picture bridge.
//
// INSTALLATION
// ------------
// `npm run tauri android init` generates the Gradle project under
// `src-tauri/gen/android/`. Copy this file next to the generated
// `MainActivity.kt`, i.e. into:
//
//   src-tauri/gen/android/app/src/main/java/dev_animehub_app/
//
// The package name below must match the one Tauri generates for the app
// identifier (`dev.animehub.app` -> `dev_animehub_app`), and it must match
// the string passed to `register_android_plugin` in
// `src-tauri/android-plugin/src/lib.rs`.
//
// VERIFICATION STATUS
// -------------------
// Written against the Tauri 2.11.6 plugin API
// (`@TauriPlugin`, `Plugin`, `@Command`, `Invoke`, `JSObject`) and the
// standard Android APIs listed in the imports. It has NOT been compiled or
// run here: that needs the Android SDK + NDK and a device, which are not
// available in the environment this was written in. Treat it as unverified
// until `npm run tauri android dev` passes on real hardware.

package dev_animehub_app

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
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/**
 * Bridges the two Android-only features AnimeHub needs:
 *
 *  - **AndroidKeyStore**: an AES/GCM key that never leaves the secure
 *    hardware. Rust sends plaintext, gets base64 ciphertext back, so the raw
 *    key is never present in the Rust process.
 *  - **Picture-in-Picture**: `PictureInPictureParams` /
 *    `enterPictureInPictureMode()` live on the host Activity.
 *
 * Command names must stay in sync with `CMD_*` in
 * `src-tauri/android-plugin/src/lib.rs`.
 */
@TauriPlugin
class AnimeHubPlugin(private val activity: Activity) : Plugin(activity) {

  override fun load(webView: WebView) {
    // No webview state needed; the bridges are Activity/Keystore only.
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
        .setAspectRatio(Rational(clampRatio(num), clampRatio(den)))
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

    /** Keep a key alias inside the character set the keystore accepts. */
    private fun sanitize(purpose: String): String {
      val cleaned = purpose.filter { it.isLetterOrDigit() || it == '_' || it == '-' }.take(64)
      return if (cleaned.isEmpty()) "default" else cleaned
    }

    /** Android rejects aspect ratios outside roughly 1:2.39 .. 2.39:1. */
    private fun clampRatio(v: Int): Int = v.coerceIn(1, 239)
  }
}
