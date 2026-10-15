// SPDX-License-Identifier: MIT
//
// Per-site WebView controller for AnimeHub's Android multi-profile
// architecture (androidx.webkit MULTI_PROFILE).
//
// WHY THIS EXISTS
// ---------------
// The launcher keeps the single Tauri/Wry WebView (default profile). Every
// site opens in a *separate* native WebView whose profile is assigned with
// `WebViewCompat.setProfile` **before** the view touches the hierarchy, any
// other WebView API, or a first navigation — the exact ordering the AndroidX
// docs require. Each registered `site_id` maps to its own persistent profile
// (see `android_profile_name` in Rust), so localStorage, IndexedDB, cookies
// and cache are isolated per site even when two sites share one origin.
//
// There is NO silent fallback: when the WebView provider does not report
// `WebViewFeature.MULTI_PROFILE`, `open` fails with a sentinel error and the
// site never loads — pretending isolation exists would be a security claim
// we cannot back.
//
// DATA CONTRACT
// -------------
// * Cookies live in the profile itself. The legacy encrypted cookie jar of a
//   0.3.4 install is imported **once**, into a profile that does not exist
//   yet, through `Profile.getCookieManager()` (never the static
//   `CookieManager.getInstance()`, which is the launcher's default profile),
//   and only after every set-cookie callback and the blocking `flush()`
//   completed — i.e. before the first navigation of that profile.
// * localStorage / IndexedDB snapshots are NOT imported: the old JSON dump
//   was lossy (indexes, key paths, structured-clone values) and re-injecting
//   it would recreate the cross-site TOCTOU problems this architecture
//   removes. Rust deletes the migration flag once attempted; see
//   `docs/android-storage-isolation.md`.
//
// This file is a TEMPLATE-FREE copy: `scripts/android_prepare.py` copies it
// verbatim next to the generated MainActivity.kt, like AnimeHubPlugin.kt.

package dev.animehub.app

import android.app.Activity
import android.graphics.Bitmap
import android.util.Log
import android.view.View
import android.view.ViewGroup
import androidx.activity.ComponentActivity
import android.webkit.CookieManager
import android.webkit.GeolocationPermissions
import android.webkit.WebChromeClient
import android.webkit.WebView
import android.webkit.WebViewClient
import androidx.activity.OnBackPressedCallback
import androidx.webkit.Profile
import androidx.webkit.ProfileStore
import androidx.webkit.WebViewCompat
import androidx.webkit.WebViewFeature
import java.lang.ref.WeakReference
import java.util.concurrent.Executors

/**
 * Owns the site WebView: creation with its own profile, show/hide over the
 * launcher, lifecycle forwarding, back navigation, fullscreen video and
 * profile data operations (clear / delete).
 *
 * All WebView and ProfileStore work runs on the UI thread (both APIs are
 * `@UiThread`); the blocking `CookieManager.flush()` always runs on
 * [FLUSH_EXECUTOR], never on the UI thread.
 */
object AnimeHubSiteController {

  /** Rejected-open sentinel; Rust maps it to the user-facing guidance. */
  const val ERR_UNSUPPORTED_MULTI_PROFILE = "unsupported-multi-profile"

  /** Another site WebView is already attached (Rust serialises opens). */
  const val ERR_ACTIVE_SITE = "active-site-open"

  /**
   * Profile names are produced by Rust (`android_profile_name`): a SHA-256
   * slice of the registered site id plus a persistent generation counter.
   * Validating here means a hostile/buggy caller can never steer a delete or
   * clear at the `Default` profile or at arbitrary provider data.
   */
  private val PROFILE_NAME = Regex("""site-[0-9a-f]{32}-g\d{1,10}""")

  private val FLUSH_EXECUTOR = Executors.newSingleThreadExecutor { runnable ->
    Thread(runnable, "AnimeHubSiteFlush").apply { isDaemon = true }
  }

  /** The live site WebView, if one is attached to the Activity. */
  @Volatile
  private var activeWebView: WeakReference<WebView>? = null

  /** Profile name of the active site WebView (mirrors the Rust session). */
  @Volatile
  private var activeProfileName: String? = null

  /** The launcher's Tauri/Wry WebView, for PiP re-binding and back fallback. */
  @Volatile
  private var launcherWebView: WeakReference<WebView>? = null

  /** Registered once; enabled only while a site WebView is attached. */
  private var backCallback: OnBackPressedCallback? = null

  /** Fullscreen video view currently shown by [SiteChromeClient]. */
  private var customView: View? = null
  private var customViewCallback: WebChromeClient.CustomViewCallback? = null

  // ------------------------------------------------------------------- state

  /** Remember the launcher WebView (`AnimeHubPlugin.load`). */
  fun bindLauncher(webView: WebView) {
    launcherWebView = WeakReference(webView)
  }

  /** The profile name of the open site, if any (used by delete guards). */
  fun activeProfile(): String? = activeProfileName

  fun isSupported(): Boolean = WebViewFeature.isFeatureSupported(WebViewFeature.MULTI_PROFILE)

  private fun profileNameIsValid(name: String): Boolean = PROFILE_NAME.matches(name)

  private fun liveActivity(activity: Activity): Boolean = !activity.isFinishing && !activity.isDestroyed

  // -------------------------------------------------------------------- open

  /**
   * Open `url` in a fresh WebView bound to [profileName].
   *
   * Ordering contract (AndroidX `WebViewCompat.setProfile`):
   * `WebView(context)` → `setProfile` → *then* settings, clients, document
   * start script, hierarchy attach and `loadUrl`. Nothing else touches the
   * view before `setProfile` returns.
   *
   * When [cookies] is non-empty and the profile does not exist yet (first
   * open after the 0.3.4 migration), the legacy jar is written into the
   * profile's own cookie manager and flushed to disk before the WebView is
   * even constructed.
   */
  fun open(
    activity: Activity,
    url: String,
    profileName: String,
    initScript: String,
    cookies: List<String>,
    onResult: (migrated: Boolean, error: String?) -> Unit,
  ) {
    if (!isSupported()) {
      onResult(false, ERR_UNSUPPORTED_MULTI_PROFILE)
      return
    }
    if (!profileNameIsValid(profileName)) {
      onResult(false, "geçersiz profil adı")
      return
    }
    activity.runOnUiThread {
      if (!liveActivity(activity)) {
        onResult(false, "Android Activity artık kullanılamıyor")
        return@runOnUiThread
      }
      if (activeWebView?.get() != null) {
        onResult(false, ERR_ACTIVE_SITE)
        return@runOnUiThread
      }
      try {
        if (cookies.isNotEmpty()) {
          migrateCookies(activity, url, profileName, cookies) { migrated, error ->
            if (error != null) onResult(false, error) else buildWebView(activity, url, profileName, initScript, migrated, onResult)
          }
        } else {
          buildWebView(activity, url, profileName, initScript, migrated = false, onResult)
        }
      } catch (e: Exception) {
        Log.e(LOG_TAG, "site open failed (${e.javaClass.simpleName})")
        AnimeHubDebugLog.add("site", "open failed (${e.javaClass.simpleName})")
        onResult(false, "site açılamadı: ${e.message}")
      }
    }
  }

  /**
   * One-time import of the legacy encrypted cookie jar into a *virgin*
   * profile. Existing profiles keep their own live cookies: their state is
   * authoritative and must never be overwritten by a stale snapshot.
   */
  private fun migrateCookies(
    activity: Activity,
    url: String,
    profileName: String,
    cookies: List<String>,
    onResult: (migrated: Boolean, error: String?) -> Unit,
  ) {
    val store = ProfileStore.getInstance()
    val existing = store.getProfile(profileName)
    if (existing != null) {
      // Profile already exists: nothing to migrate, do not touch its jar.
      onResult(false, null)
      return
    }
    val profile = store.getOrCreateProfile(profileName)
    val manager: CookieManager = profile.cookieManager
    var rejected = 0
    fun setNext(index: Int) {
      if (index >= cookies.size) {
        // Every callback has fired; make the jar durable before the first
        // navigation, then hand back to the UI thread for the WebView.
        try {
          FLUSH_EXECUTOR.execute {
            try {
              manager.flush()
            } catch (e: Exception) {
              activity.runOnUiThread {
                onResult(false, "çerez aktarımı kalıcı hale getirilemedi: ${e.message}")
              }
              return@execute
            }
            activity.runOnUiThread {
              if (rejected > 0) {
                Log.w(LOG_TAG, "profile cookie import rejected $rejected cookie(s)")
              }
              AnimeHubDebugLog.add("cookie", "migrated into profile ($rejected rejected)")
              onResult(true, null)
            }
          }
        } catch (e: Exception) {
          onResult(false, "çerez aktarımı kuyruğa alınamadı: ${e.message}")
        }
        return
      }
      try {
        manager.setCookie(url, cookies[index]) { accepted ->
          activity.runOnUiThread {
            if (accepted != true) rejected += 1
            setNext(index + 1)
          }
        }
      } catch (e: Exception) {
        onResult(false, "çerez aktarımı başarısız: ${e.message}")
      }
    }
    setNext(0)
  }

  /** Construct, configure, attach and navigate the site WebView. UI thread. */
  private fun buildWebView(
    activity: Activity,
    url: String,
    profileName: String,
    initScript: String,
    migrated: Boolean,
    onResult: (migrated: Boolean, error: String?) -> Unit,
  ) {
    var webView: WebView? = null
    var attached = false
    try {
      // (1) Constructor, (2) profile — before ANY other WebView API.
      webView = WebView(activity)
      WebViewCompat.setProfile(webView, profileName)

      // (3) Settings: the same surface wry's RustWebView enables, so site
      // behaviour (JS, DOM/web storage, autoplay, popup-capable scripts)
      // matches the shared WebView this replaces.
      webView.settings.javaScriptEnabled = true
      webView.settings.domStorageEnabled = true
      webView.settings.databaseEnabled = true
      webView.settings.setGeolocationEnabled(true)
      webView.settings.mediaPlaybackRequiresUserGesture = false
      webView.settings.javaScriptCanOpenWindowsAutomatically = true

      // (4) Navigation protection: the same SiteNavigationGuard policy the
      // launcher WebView uses, over a plain client (never wry's client, which
      // casts to RustWebView). Blocklist/DNS checks stay in Rust — unchanged
      // documented Android limitation.
      val evalOnPageStart = !WebViewFeature.isFeatureSupported(WebViewFeature.DOCUMENT_START_SCRIPT)
      webView.webViewClient = SiteNavigationGuard(SiteWebViewClient(initScript, evalOnPageStart))
      webView.webChromeClient =
        SiteChromeClient({ view, callback -> enterCustomView(view, callback) }, { exitCustomViewIfNeeded() })

      // (5) Document-start script (cosmetic rules, popup guard, PiP
      // controller). Self-guarding, so the page-start fallback can never
      // double-apply it.
      if (!evalOnPageStart) {
        WebViewCompat.addDocumentStartJavaScript(webView, initScript, setOf("*"))
      }

      // (6) Attach above the launcher; never remove the launcher — closing
      // this view reveals it again.
      val content = activity.findViewById<ViewGroup>(android.R.id.content)
      content.addView(
        webView,
        ViewGroup.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT),
      )
      attached = true
      webView.isFocusable = true
      webView.isFocusableInTouchMode = true
      webView.requestFocus()

      // (7) PiP follows the visible page: bind the controller to this view.
      AnimeHubPipController.attach(activity, webView)

      // (8) Back button: history first, launcher second, never the task.
      ensureBackCallback(activity).isEnabled = true

      activeWebView = WeakReference(webView)
      activeProfileName = profileName
      AnimeHubDebugLog.add("site", "opened (migrated=$migrated)")

      webView.loadUrl(url)
      onResult(migrated, null)
    } catch (e: Exception) {
      Log.e(LOG_TAG, "site WebView setup failed (${e.javaClass.simpleName})")
      AnimeHubDebugLog.add("site", "setup failed (${e.javaClass.simpleName})")
      // Roll the partial view back so a failed open never leaves a hidden
      // half-configured WebView above the launcher.
      try {
        if (attached && webView != null) {
          (webView.parent as? ViewGroup)?.removeView(webView)
        }
        webView?.destroy()
      } catch (_: Exception) {
        // Best effort; the Activity is likely going away anyway.
      }
      if (backCallback?.isEnabled == true) backCallback?.isEnabled = false
      launcherWebView?.get()?.let { launcher -> AnimeHubPipController.attach(activity, launcher) }
      onResult(false, "site açılamadı: ${e.message}")
    }
  }

  // ------------------------------------------------------------------- close

  /**
   * Detach and destroy the active site WebView, restoring PiP and focus to
   * the launcher. With [flush] the profile's cookies are written to disk
   * first (normal close); wipe paths pass `false` — their data is about to
   * be cleared or deleted anyway. Idempotent: closing with no active site is
   * a success, which is what keeps a stale Rust session harmless.
   */
  fun close(activity: Activity, flush: Boolean, onResult: (error: String?) -> Unit) {
    activity.runOnUiThread {
      val webView = activeWebView?.get()
      val profileName = activeProfileName
      if (webView == null) {
        finishClose(activity, null)
        onResult(null)
        return@runOnUiThread
      }
      if (!flush || profileName == null) {
        finishClose(activity, webView)
        onResult(null)
        return@runOnUiThread
      }
      val profile: Profile = try {
        WebViewCompat.getProfile(webView)
      } catch (e: Exception) {
        finishClose(activity, webView)
        onResult(null)
        return@runOnUiThread
      }
      try {
        FLUSH_EXECUTOR.execute {
          try {
            profile.cookieManager.flush()
          } catch (e: Exception) {
            Log.w(LOG_TAG, "profile cookie flush failed (${e.javaClass.simpleName})")
          }
          activity.runOnUiThread {
            finishClose(activity, webView)
            onResult(null)
          }
        }
      } catch (e: Exception) {
        finishClose(activity, webView)
        onResult("çerez kalıcılığı kuyruğa alınamadı: ${e.message}")
      }
    }
  }

  /** UI thread. Remove, destroy, re-bind PiP/focus to the launcher. */
  private fun finishClose(activity: Activity, webView: WebView?) {
    if (webView != null) {
      try {
        (webView.parent as? ViewGroup)?.removeView(webView)
      } catch (_: Exception) {
        // The view may already be detached with the Activity.
      }
      try {
        webView.destroy()
      } catch (_: Exception) {
        // Destroying twice must never break the close path.
      }
    }
    if (activeWebView?.get() === webView || webView == null) {
      activeWebView = null
      activeProfileName = null
    }
    exitCustomViewIfNeeded()
    backCallback?.isEnabled = false
    // PiP belongs to whatever the user sees now: the launcher.
    launcherWebView?.get()?.let { AnimeHubPipController.attach(activity, it) }
    launcherWebView?.get()?.requestFocus()
    AnimeHubDebugLog.add("site", "closed")
  }

  // ------------------------------------------------------- lifecycle / back

  /** WryActivity pauses only its own WebView; the site view follows too. */
  fun onPause(activity: Activity) {
    if (!liveActivity(activity)) return
    activeWebView?.get()?.onPause()
  }

  fun onResume(activity: Activity) {
    if (!liveActivity(activity)) return
    activeWebView?.get()?.onResume()
  }

  /**
   * Activity teardown: flush the site profile (same durability guarantee the
   * launcher's onDestroy gives the default profile), then destroy the view.
   * Never touches the default profile.
   */
  fun destroy(activity: Activity) {
    val webView = activeWebView?.get()
    val profileName = activeProfileName
    activeWebView = null
    activeProfileName = null
    exitCustomViewIfNeeded()
    backCallback?.isEnabled = false
    if (webView == null) return
    if (profileName != null) {
      try {
        val profile = WebViewCompat.getProfile(webView)
        profile.cookieManager.flush()
      } catch (_: Exception) {
        Log.w(LOG_TAG, "profile flush on destroy failed")
      }
    }
    try {
      (webView.parent as? ViewGroup)?.removeView(webView)
    } catch (_: Exception) {
      // Detached with the Activity already.
    }
    try {
      webView.destroy()
    } catch (_: Exception) {
      // Never crash teardown.
    }
    AnimeHubDebugLog.add("site", "destroyed with activity")
  }

  /**
   * Back button while a site is showing: exit fullscreen video, else walk the
   * site's own history, else hand back to the launcher. The Activity is never
   * finished from here, so a site can never eat the app's task.
   */
  private fun ensureBackCallback(activity: Activity): OnBackPressedCallback {
    backCallback?.let { return it }
    val callback = object : OnBackPressedCallback(false) {
      override fun handleOnBackPressed() {
        if (customView != null) {
          exitCustomViewIfNeeded()
          return
        }
        val webView = activeWebView?.get()
        if (webView != null && webView.canGoBack()) {
          webView.goBack()
          return
        }
        closeViaLauncher(activity)
      }
    }
    // `onBackPressedDispatcher` lives on ComponentActivity (MainActivity is
    // one through AppCompatActivity). A host that is not would fall back to
    // the platform default, so log loudly instead of pretending to handle it.
    val host = activity as? ComponentActivity
    if (host == null) {
      Log.e(LOG_TAG, "back callback not installed: host is not a ComponentActivity")
    } else {
      host.onBackPressedDispatcher.addCallback(callback)
      backCallback = callback
    }
    return callback
  }

  /**
   * Preferred close path: ask the launcher page to run the normal
   * `close_site` flow (Rust state, `animehub://site-closed`, cookie
   * flush bookkeeping) by evaluating a launcher-side hook. If the launcher
   * cannot answer, close directly — the Rust session may then be stale for a
   * moment, which every Rust path re-checks against this controller.
   */
  private fun closeViaLauncher(activity: Activity) {
    val launcher = launcherWebView?.get()
    val script =
      "!!(window.__animehubBackToLauncher && (window.__animehubBackToLauncher(), true))"
    if (launcher == null) {
      close(activity, flush = true) { }
      return
    }
    try {
      launcher.evaluateJavascript(script) { result ->
        if (result != "true") {
          AnimeHubDebugLog.add("site", "launcher back hook unavailable; closed natively")
          close(activity, flush = true) { }
        }
      }
    } catch (_: Exception) {
      AnimeHubDebugLog.add("site", "launcher back hook failed; closed natively")
      close(activity, flush = true) { }
    }
  }

  // ----------------------------------------------------------- profile data

  /**
   * Clear one site profile in place: cookies (profile manager, then flush),
   * web storage and geolocation permissions. The profile itself survives, so
   * the next open of the same site starts logged out without a delete/recreate
   * race. HTTP cache and service-worker registrations are NOT part of this
   * path — documented in docs/android-storage-isolation.md.
   *
   * `deleted=false`-style states resolve normally: an absent profile is
   * already clean. Unsupported providers resolve without touching anything
   * (no profile data can be addressed through this API there); Rust logs it.
   */
  fun clearProfileData(activity: Activity, profileName: String, onResult: (error: String?) -> Unit) {
    if (!profileNameIsValid(profileName)) {
      onResult("geçersiz profil adı")
      return
    }
    if (activeProfileName == profileName) {
      onResult("aktif site verisi önce kapatılmalı")
      return
    }
    if (!isSupported()) {
      AnimeHubDebugLog.add("site", "clear skipped: profile feature unavailable")
      onResult(null)
      return
    }
    activity.runOnUiThread {
      if (!liveActivity(activity)) {
        onResult("Android Activity artık kullanılamıyor")
        return@runOnUiThread
      }
      try {
        val profile = ProfileStore.getInstance().getProfile(profileName)
        if (profile == null) {
          onResult(null)
          return@runOnUiThread
        }
        val manager: CookieManager = profile.cookieManager
        val storage = profile.webStorage
        val geo: GeolocationPermissions = profile.geolocationPermissions
        manager.removeAllCookies {
          try {
            geo.removeAll { }
          } catch (_: Exception) {
            // Geolocation permissions are best-effort; cookies and storage
            // below are the contract.
          }
          try {
            storage.deleteAllData()
          } catch (e: Exception) {
            activity.runOnUiThread {
              onResult("site deposu temizlenemedi: ${e.message}")
            }
            return@removeAllCookies
          }
          try {
            FLUSH_EXECUTOR.execute {
              try {
                manager.flush()
              } catch (_: Exception) {
                Log.w(LOG_TAG, "profile cookie flush after clear failed")
              }
              activity.runOnUiThread { onResult(null) }
            }
          } catch (e: Exception) {
            activity.runOnUiThread { onResult("çerez temizliği kuyruğa alınamadı: ${e.message}") }
          }
        }
      } catch (e: Exception) {
        Log.e(LOG_TAG, "profile clear failed (${e.javaClass.simpleName})")
        onResult("site verisi temizlenemedi: ${e.message}")
      }
    }
  }

  /**
   * Delete a site profile's data.
   *
   * Resolves `{deleted, pending}`:
   *  * `deleted=true`  — the provider accepted the deletion (disk work may
   *    still finish asynchronously; Rust bumps the generation so the same
   *    name is never immediately recreated).
   *  * `deleted=false, pending=false` — the profile does not exist (already
   *    clean).
   *  * `deleted=false, pending=true` — the profile is loaded in this process
   *    (Chromium refuses to delete an in-use context) or the provider does
   *    not support the API right now. Rust queues the name and retries at
   *    the next process start, before any site can open.
   *
   * Rejects (hard failure — the caller must NOT drop its record) for invalid
   * names, an active WebView on that profile, or unexpected exceptions.
   */
  fun deleteProfileData(profileName: String, onResult: (deleted: Boolean, pending: Boolean, error: String?) -> Unit) {
    if (!profileNameIsValid(profileName)) {
      onResult(false, false, "geçersiz profil adı")
      return
    }
    if (activeProfileName == profileName) {
      onResult(false, false, "aktif site profili önce kapatılmalı")
      return
    }
    if (!isSupported()) {
      // Profiles could only exist from an earlier provider build; retry later.
      onResult(false, true, null)
      return
    }
    // NOTE: no getProfile/getOrCreateProfile here on purpose — merely loading
    // a profile makes the provider refuse deletion for the rest of the
    // process (AwBrowserContextStore::Delete returns in-use).
    try {
      val deleted = ProfileStore.getInstance().deleteProfile(profileName)
      AnimeHubDebugLog.add("site", if (deleted) "profile deleted" else "profile absent")
      onResult(deleted, false, null)
    } catch (_: IllegalStateException) {
      // Living/loaded context: defer to the next process start.
      AnimeHubDebugLog.add("site", "profile delete deferred (in use)")
      onResult(false, true, null)
    } catch (e: Exception) {
      Log.e(LOG_TAG, "profile delete failed (${e.javaClass.simpleName})")
      onResult(false, false, "profil verisi silinemedi: ${e.message}")
    }
  }

  /**
   * UI thread. Every profile name the provider store knows — the "clear all"
   * orphan sweep in Rust filters this to our own `site-*` namespace, so the
   * `Default` profile is never even considered. `null` when the provider
   * cannot enumerate (MULTI_PROFILE unsupported): Rust then skips the sweep
   * and logs, rather than guessing.
   */
  fun allProfileNames(): List<String>? {
    if (!isSupported()) return null
    return ProfileStore.getInstance().getAllProfileNames()
  }

  /** Durability hook for the periodic Rust session sync. */
  fun flush(activity: Activity, onResult: (error: String?) -> Unit) {
    val webView = activeWebView?.get()
    if (webView == null) {
      onResult(null)
      return
    }
    val profile: Profile = try {
      WebViewCompat.getProfile(webView)
    } catch (_: Exception) {
      onResult(null)
      return
    }
    try {
      FLUSH_EXECUTOR.execute {
        try {
          profile.cookieManager.flush()
        } catch (e: Exception) {
          activity.runOnUiThread { onResult("çerez kalıcılığı başarısız: ${e.message}") }
          return@execute
        }
        activity.runOnUiThread { onResult(null) }
      }
    } catch (e: Exception) {
      onResult("çerez kalıcılığı kuyruğa alınamadı: ${e.message}")
    }
  }

  // -------------------------------------------------------- fullscreen video

  /**
   * `WebChromeClient` fullscreen video. The site view is hidden and the
   * player view is added above it; back exits fullscreen first (handled in
   * [ensureBackCallback]). wry's own client declines custom views, so this is
   * an intentional capability of the new site WebView, documented as such.
   */
  private class SiteChromeClient(
    private val onShow: (View, WebChromeClient.CustomViewCallback) -> Unit,
    private val onHide: () -> Unit,
  ) : WebChromeClient() {
    override fun onShowCustomView(view: View, callback: WebChromeClient.CustomViewCallback) {
      onShow(view, callback)
    }

    override fun onHideCustomView() {
      onHide()
    }
  }

  /** UI thread. Add the fullscreen player view above the (hidden) site view. */
  private fun enterCustomView(view: View, callback: WebChromeClient.CustomViewCallback) {
    val activity = activeWebView?.get()?.context as? Activity
    if (customView != null || activity == null) {
      callback.onCustomViewHidden()
      return
    }
    customView = view
    customViewCallback = callback
    val content = activity.findViewById<ViewGroup>(android.R.id.content)
    content.addView(
      view,
      ViewGroup.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT),
    )
    activeWebView?.get()?.visibility = View.INVISIBLE
  }

  /** Remove the fullscreen video view and reveal the site WebView again. */
  private fun exitCustomViewIfNeeded() {
    val view = customView ?: return
    val callback = customViewCallback
    customView = null
    customViewCallback = null
    try {
      (view.parent as? ViewGroup)?.removeView(view)
    } catch (_: Exception) {
      // Already gone with the Activity.
    }
    activeWebView?.get()?.visibility = View.VISIBLE
    try {
      callback?.onCustomViewHidden()
    } catch (_: Exception) {
      // Chromium may have released the callback with the page.
    }
  }

  /**
   * Base client for the site WebView: forwards everything (the
   * [SiteNavigationGuard] wrapper owns policy) and only adds the init-script
   * fallback for providers without `DOCUMENT_START_SCRIPT`. Deliberately NOT
   * wry's `RustWebViewClient`: that class casts to `RustWebView` and talks to
   * the launcher's IPC/asset machinery, none of which a site may reach.
   */
  private class SiteWebViewClient(
    private val initScript: String,
    private val evalOnPageStart: Boolean,
  ) : WebViewClient() {
    override fun onPageStarted(view: WebView, url: String, favicon: Bitmap?) {
      if (evalOnPageStart) {
        try {
          view.evaluateJavascript(initScript, null)
        } catch (_: Exception) {
          Log.w(LOG_TAG, "page-start init script failed")
        }
      }
      super.onPageStarted(view, url, favicon)
    }
  }

  private const val LOG_TAG = "AnimeHubSite"
}
