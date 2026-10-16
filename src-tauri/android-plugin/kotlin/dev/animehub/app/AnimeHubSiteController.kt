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
//   `CookieManager.getInstance()`, which is the launcher's default profile).
//   Success is strict: every `setCookie` callback accepted AND `flush()`
//   completed before the WebView is constructed. Any rejection, exception,
//   timeout or an already-existing profile fails the attempt with the
//   `migration-partial` sentinel; Rust then retires that profile name
//   (queue + generation bump) and retries on a virgin one. The legacy
//   snapshot is only consumed after a real success.
// * localStorage / IndexedDB snapshots are NOT imported: the old JSON dump
//   was lossy (indexes, key paths, structured-clone values) and re-injecting
//   it would recreate the cross-site TOCTOU problems this architecture
//   removes. Rust deletes the migration flag once attempted; see
//   `docs/android-storage-isolation.md`.
//
// FAILURE HANDLING
// ----------------
// The sequencing lives in `SiteProfileOps` (pure Kotlin, executed by the CI
// unit tests). Invariants enforced here:
// * normal close (`flush=true`) reports success only after the profile
//   flush succeeded; a flush/resolve failure keeps the view AND the session
//   ownership so Kotlin and Rust cannot disagree, and back/close is retryable;
// * a failed open rolls back the view and releases the active slot (only if
//   it still holds that view), so the next open never sees a stuck
//   `ERR_ACTIVE_SITE`;
// * Activity teardown schedules the flush on the background executor and
//   never blocks the UI thread or claims durability it did not await;
// * the launcher back hook only *starts* the normal `close_site` flow;
//   Kotlin polls its status, surfaces failures with a native Toast (visible
//   above the site view) and falls back to a native close when the launcher
//   cannot answer.
//
// This file is a TEMPLATE-FREE copy: `scripts/android_prepare.py` copies it
// verbatim next to the generated MainActivity.kt, like AnimeHubPlugin.kt.

package dev.animehub.app

import android.app.Activity
import android.graphics.Bitmap
import android.os.Handler
import android.os.Looper
import android.util.Log
import android.view.View
import android.view.ViewGroup
import android.widget.Toast
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
import java.util.concurrent.atomic.AtomicLong
import org.json.JSONTokener

/**
 * Owns the site WebView: creation with its own profile, show/hide over the
 * launcher, lifecycle forwarding, back navigation, fullscreen video and
 * profile data operations (clear / delete).
 *
 * All WebView and ProfileStore work runs on the UI thread (both APIs are
 * `@UiThread`); the blocking `CookieManager.flush()` always runs on
 * [FLUSH_EXECUTOR], never on the UI thread. Flow sequencing (close / clear /
 * migration) lives in [SiteCloseFlow], [SiteClearFlow] and
 * [SiteMigrationFlow]; this file only wires the Android ports.
 */
object AnimeHubSiteController {

  /** Rejected-open sentinel; Rust maps it to the user-facing guidance. */
  const val ERR_UNSUPPORTED_MULTI_PROFILE = "unsupported-multi-profile"

  /**
   * The pending one-time migration could not complete against a virgin
   * profile; Rust retires this profile name and retries later. Exact
   * literal shared with `SiteProfileErrors.MIGRATION_RETRY` (the plugin
   * crate asserts it against this source).
   */
  const val ERR_MIGRATION_RETRY = "migration-partial"

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

  /**
   * When the flush task currently running on [FLUSH_EXECUTOR] started, or 0
   * while the worker is idle. Single-threaded on purpose: `CookieManager
   * .flush()` is the blocking provider call, and serialising it keeps the
   * profile cookie stores from being written concurrently.
   */
  private val flushBusySince = AtomicLong(0L)

  /**
   * How long a running flush may take before a newly queued one is logged as
   * evidence of a stuck worker.
   *
   * A flow watchdog answers the CALLER — it never cancels the blocking
   * `flush()` underneath, because the provider call offers no cancellation.
   * So a genuinely stuck flush leaves its task occupying the worker; later
   * flushes queue behind it and every later await is answered by its own
   * watchdog. This counter makes that pileup observable in the in-app log
   * instead of invisible (no unbounded thread spawning to paper over it).
   */
  private const val FLUSH_STUCK_WARN_MS = 10_000L

  /**
   * Run one blocking `flush()` on [FLUSH_EXECUTOR] and deliver its result on
   * the UI thread. Warns (logcat + in-app log) when a previous flush has
   * still been running for [FLUSH_STUCK_WARN_MS] or more — the caller has
   * its watchdog; this is the evidence that the worker itself is stuck.
   */
  private fun submitFlush(
    activity: Activity,
    tag: String,
    work: () -> Unit,
    onDone: (Throwable?) -> Unit,
  ) {
    val since = flushBusySince.get()
    if (since > 0L) {
      val waited = System.currentTimeMillis() - since
      if (waited >= FLUSH_STUCK_WARN_MS) {
        Log.w(LOG_TAG, "flush worker stuck ${waited}ms while queueing '$tag'")
        AnimeHubDebugLog.add("site", "flush worker stuck ${waited}ms ($tag)")
      }
    }
    try {
      FLUSH_EXECUTOR.execute {
        flushBusySince.compareAndSet(0L, System.currentTimeMillis())
        val error =
          try {
            work()
            null
          } catch (e: Exception) {
            e
          }
        flushBusySince.set(0L)
        activity.runOnUiThread { onDone(error) }
      }
    } catch (e: Exception) {
      activity.runOnUiThread { onDone(e) }
    }
  }

  /** UI-thread scheduler for flow watchdogs and the back-status poll. */
  private val mainHandler = Handler(Looper.getMainLooper())

  /** The live site WebView + its profile, as one indivisible slot. */
  private val active = SiteSessionState<WebView>()

  /** The launcher's Tauri/Wry WebView, for PiP re-binding and back fallback. */
  @Volatile
  private var launcherWebView: WeakReference<WebView>? = null

  /** Registered once; enabled only while a site WebView is attached. */
  private var backCallback: OnBackPressedCallback? = null

  /** Fullscreen video view currently shown by [SiteChromeClient]. */
  private var customView: View? = null
  private var customViewCallback: WebChromeClient.CustomViewCallback? = null

  // Back-close handshake with the launcher JS (window.__animehubBack*).
  private const val BACK_HOOK_SCRIPT =
    "!!(window.__animehubBackToLauncher && (window.__animehubBackToLauncher(), true))"
  private const val BACK_STATUS_SCRIPT =
    "window.__animehubBackStatus ? window.__animehubBackStatus() : null"
  private const val BACK_POLL_MS = 200L
  private const val BACK_MAX_POLLS = 25

  // ------------------------------------------------------------------- state

  /** Remember the launcher WebView (`AnimeHubPlugin.load`). */
  fun bindLauncher(webView: WebView) {
    launcherWebView = WeakReference(webView)
  }

  /** The profile name of the open site, if any (used by delete guards). */
  fun activeProfile(): String? = active.activeProfile

  fun isSupported(): Boolean = WebViewFeature.isFeatureSupported(WebViewFeature.MULTI_PROFILE)

  private fun profileNameIsValid(name: String): Boolean = PROFILE_NAME.matches(name)

  /**
   * Quarantine gate shared by BOTH phases of the two-phase open. Returns the
   * refusal to report (already logged) or `null` when the profile may be used.
   *
   * [SiteOpenGate] blocks a profile with ANY destructive provider call still
   * outstanding — `removeAllCookies` timeouts, which cannot be cancelled: the
   * deletion may still be in flight and would land on the new session's
   * cookies. The open is refused until every such operation has settled AND a
   * wipe was verified after the last of them, because ignoring a late callback
   * result does not stop the late mutation — and because a later attempt's
   * success says nothing about an earlier attempt's deletion. [ProfileQuarantine]
   * records the per-operation ownership ledger and why it is process-scoped;
   * the decision itself is pure Kotlin, executed by the CI unit tests. This
   * refusal is identical in both phases: [migrate] and [open] call this.
   */
  private fun quarantineRefusal(profileName: String, phase: String): String? =
    SiteOpenGate.check(profileName)?.also { refusal ->
      Log.w(LOG_TAG, "site $phase refused: $refusal")
      AnimeHubDebugLog.add("site", "$phase refused (quarantined profile)")
    }

  private fun liveActivity(activity: Activity): Boolean = !activity.isFinishing && !activity.isDestroyed

  // -------------------------------------------------------------------- open

  /**
   * PHASE 1 of the two-phase open protocol: import the one-time legacy jar
   * into [profileName] WITHOUT creating the site WebView.
   *
   * Why this is a separate command (`site_migrate`) from [open]:
   * Rust persists `profilemig-<site>` only after a strict migration success,
   * and only then asks for the view. The ordering is
   * migrate → durable flag → open, so a profile the user has actually seen
   * can never exist while the flag is missing — and a missing flag can
   * therefore never mean "an in-use profile must be rebuilt from the stale
   * jar". A failed flag write aborts the open BEFORE any view exists: no
   * orphan WebView, no Rust/Kotlin ownership split (`run_mobile_open_protocol`
   * in windows.rs).
   *
   * Failure modes: [ERR_UNSUPPORTED_MULTI_PROFILE] (feature gap — refuse,
   * never fall back to the default profile), invalid name,
   * [SiteProfileErrors.QUARANTINED] (an earlier in-place wipe of this profile
   * timed out on a provider call that cannot be cancelled, so its completion
   * is unverified — see [ProfileQuarantine]), dead Activity,
   * [ERR_ACTIVE_SITE], and [ERR_MIGRATION_RETRY] for anything short of a
   * full, flushed import (see [SiteMigrationFlow]) so Rust can retire the
   * profile name and retry on a fresh generation.
   */
  fun migrate(
    activity: Activity,
    url: String,
    profileName: String,
    cookies: List<String>,
    onResult: (error: String?) -> Unit,
  ) {
    if (!isSupported()) {
      onResult(ERR_UNSUPPORTED_MULTI_PROFILE)
      return
    }
    if (!profileNameIsValid(profileName)) {
      onResult("geçersiz profil adı")
      return
    }
    // Quarantine gate, BEFORE the provider is touched: writing the legacy jar
    // into a profile whose previous wipe may still be in flight would hand
    // those cookies to a late `removeAllCookies`. Rust writes its migration
    // flag only after a success, so refusing here leaves the jar and the flag
    // exactly as they were.
    val quarantined = quarantineRefusal(profileName, "migration")
    if (quarantined != null) {
      onResult(quarantined)
      return
    }
    activity.runOnUiThread {
      if (!liveActivity(activity)) {
        onResult("Android Activity artık kullanılamıyor")
        return@runOnUiThread
      }
      if (active.hasActive()) {
        onResult(ERR_ACTIVE_SITE)
        return@runOnUiThread
      }
      if (cookies.isEmpty()) {
        // No jar (or every cookie in it expired): there is nothing to await
        // and no profile to create here — [open] binds the view to the name
        // and the provider materialises it. The flag still becomes durable
        // before any view exists, exactly like the imported case.
        onResult(null)
        return@runOnUiThread
      }
      try {
        migrateCookies(activity, url, profileName, cookies, onResult)
      } catch (e: Exception) {
        Log.e(LOG_TAG, "cookie migration failed (${e.javaClass.simpleName})")
        AnimeHubDebugLog.add("cookie", "migration failed (${e.javaClass.simpleName})")
        onResult("site çerezleri taşınamadı: ${e.message}")
      }
    }
  }

  /**
   * PHASE 2: open `url` in a fresh WebView bound to [profileName]. Called
   * only after phase 1 settled AND Rust persisted the migration flag, so the
   * view the user sees always belongs to a durably recorded session state.
   *
   * Ordering contract (AndroidX `WebViewCompat.setProfile`):
   * `WebView(context)` → `setProfile` → *then* settings, clients, document
   * start script, hierarchy attach and `loadUrl`. Nothing else touches the
   * view before `setProfile` returns — and the legacy jar (when one exists)
   * was written into the profile's own cookie manager and flushed to disk
   * during [migrate], before this WebView was even constructed, i.e. before
   * the first navigation.
   */
  fun open(
    activity: Activity,
    url: String,
    profileName: String,
    initScript: String,
    onResult: (error: String?) -> Unit,
  ) {
    if (!isSupported()) {
      onResult(ERR_UNSUPPORTED_MULTI_PROFILE)
      return
    }
    if (!profileNameIsValid(profileName)) {
      onResult("geçersiz profil adı")
      return
    }
    // Same quarantine gate as phase 1: a WebView bound to a profile whose
    // wipe is still unverified could have its fresh cookies deleted by the
    // provider call that timed out. Nothing is claimed and nothing is built
    // on this path, so Rust's `current_site` stays empty and the refusal is
    // simply retryable once the wipe is verified.
    val quarantined = quarantineRefusal(profileName, "open")
    if (quarantined != null) {
      onResult(quarantined)
      return
    }
    activity.runOnUiThread {
      if (!liveActivity(activity)) {
        onResult("Android Activity artık kullanılamıyor")
        return@runOnUiThread
      }
      if (active.hasActive()) {
        onResult(ERR_ACTIVE_SITE)
        return@runOnUiThread
      }
      try {
        buildWebView(activity, url, profileName, initScript, onResult)
      } catch (e: Exception) {
        Log.e(LOG_TAG, "site open failed (${e.javaClass.simpleName})")
        AnimeHubDebugLog.add("site", "open failed (${e.javaClass.simpleName})")
        onResult("site açılamadı: ${e.message}")
      }
    }
  }

  /**
   * One-time import of the legacy encrypted cookie jar into a *virgin*
   * profile. Existing profiles keep their own live cookies: their state is
   * authoritative and must never be overwritten by a stale snapshot.
   * Anything short of a full, flushed success reports
   * [ERR_MIGRATION_RETRY] — see [SiteMigrationFlow] for the criterion.
   */
  private fun migrateCookies(
    activity: Activity,
    url: String,
    profileName: String,
    cookies: List<String>,
    onResult: (error: String?) -> Unit,
  ) {
    val port = migrationPort(activity, url, profileName)
    SiteMigrationFlow.run(cookies, port) { outcome ->
      when (outcome) {
        is MigrationOutcome.Complete -> {
          AnimeHubDebugLog.add("cookie", "migrated into profile (${outcome.accepted} accepted)")
          onResult(null)
        }
        MigrationOutcome.ProfileAlreadyExists -> {
          AnimeHubDebugLog.add("cookie", "migration refused: profile already exists")
          onResult("$ERR_MIGRATION_RETRY: profil zaten mevcut")
        }
        is MigrationOutcome.Failed -> {
          Log.w(LOG_TAG, "cookie migration failed: ${outcome.reason}")
          AnimeHubDebugLog.add("cookie", "migration failed (${outcome.reason})")
          onResult("$ERR_MIGRATION_RETRY: ${outcome.reason}")
        }
      }
    }
  }

  /** Android wiring for [SiteMigrationFlow]; callbacks land on the UI thread. */
  private fun migrationPort(activity: Activity, url: String, profileName: String): SiteMigrationPort =
    object : SiteMigrationPort {
      private var manager: CookieManager? = null

      override fun profileExists(): Boolean = ProfileStore.getInstance().getProfile(profileName) != null

      override fun createProfile() {
        val store = ProfileStore.getInstance()
        manager = store.getOrCreateProfile(profileName).cookieManager
      }

      override fun setCookie(cookie: String, onAccepted: (Boolean) -> Unit) {
        val m = manager ?: throw IllegalStateException("profil cookie yöneticisi yok")
        m.setCookie(url, cookie) { accepted ->
          activity.runOnUiThread { onAccepted(accepted == true) }
        }
      }

      override fun flushCookies(onDone: (Throwable?) -> Unit) {
        val m = manager ?: run {
          activity.runOnUiThread { onDone(IllegalStateException("profil cookie yöneticisi yok")) }
          return
        }
        submitFlush(activity, "migration", { m.flush() }, onDone)
      }

      override fun scheduleTimeout(millis: Long, onTimeout: () -> Unit) {
        try {
          mainHandler.postDelayed({ onTimeout() }, millis)
        } catch (_: Exception) {
          // A dead looper only exists with a dead process; ignore.
        }
      }
    }

  /** Construct, configure, attach and navigate the site WebView. UI thread. */
  private fun buildWebView(
    activity: Activity,
    url: String,
    profileName: String,
    initScript: String,
    onResult: (error: String?) -> Unit,
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

      // (9) Claim the slot BEFORE navigating: from here on, any failure must
      // release exactly this view (see the catch block).
      if (!active.acquire(webView, profileName)) {
        throw IllegalStateException("aktif site görünümü zaten var")
      }
      // The migration (if any) completed and its flag is durable before this
      // view exists — phase 2 only builds and navigates.
      AnimeHubDebugLog.add("site", "opened")

      webView.loadUrl(url)
      onResult(null)
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
      // Release the slot only if it still holds THIS view (a newer session
      // must never be cleared by a stale failure), then restore global state
      // — but only while no other site view is active.
      if (webView != null) active.releaseIf(webView)
      if (!active.hasActive()) {
        if (backCallback?.isEnabled == true) backCallback?.isEnabled = false
        exitCustomViewIfNeeded()
        launcherWebView?.get()?.let { launcher -> AnimeHubPipController.attach(activity, launcher) }
      }
      onResult("site açılamadı: ${e.message}")
    }
  }

  // ------------------------------------------------------------------- close

  /**
   * Detach and destroy the active site WebView, restoring PiP and focus to
   * the launcher. With [flush] the profile's cookies are written to disk
   * first (normal close) and **failure keeps the view and the session
   * ownership** — the caller (Rust `close_site`) must see the error so
   * `current_site` and this slot never diverge, and the close can be
   * retried. Wipe paths pass `false`: their data is about to be cleared or
   * deleted anyway, so this branch never touches the flush path.
   * Idempotent: closing with no active site is a success, which is what
   * keeps a stale Rust session harmless.
   */
  fun close(activity: Activity, flush: Boolean, onResult: (error: String?) -> Unit) {
    activity.runOnUiThread {
      val port =
        object : SiteClosePort {
          override fun hasActiveView(): Boolean = active.hasActive()

          override fun resolveProfileName(): Result<String> = runCatching {
            val view = active.activeToken ?: error("aktif site görünümü yok")
            WebViewCompat.getProfile(view).name
          }

          override fun flushCookies(profileName: String, onDone: (Throwable?) -> Unit) {
            val view = active.activeToken
            val profile =
              try {
                if (view != null) WebViewCompat.getProfile(view) else null
              } catch (_: Exception) {
                null
              }
            if (profile == null) {
              onDone(IllegalStateException("profil çözümlenemedi"))
              return
            }
            submitFlush(activity, "close", { profile.cookieManager.flush() }, onDone)
          }

          override fun scheduleTimeout(millis: Long, onTimeout: () -> Unit) {
            try {
              mainHandler.postDelayed({ onTimeout() }, millis)
            } catch (_: Exception) {
              // Dead looper ⇒ dead process; nothing to time out.
            }
          }

          override fun destroyActiveView() {
            finishClose(activity)
          }
        }
      SiteCloseFlow.run(port, flush) { error ->
        if (error != null) {
          Log.w(LOG_TAG, "site close refused: $error")
          AnimeHubDebugLog.add("site", "close refused (${error.take(60)})")
        }
        onResult(error)
      }
    }
  }

  /** UI thread. Remove, destroy, re-bind PiP/focus to the launcher. */
  private fun finishClose(activity: Activity) {
    val webView = active.activeToken
    active.clear()
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
    active.activeToken?.onPause()
  }

  fun onResume(activity: Activity) {
    if (!liveActivity(activity)) return
    active.activeToken?.onResume()
  }

  /**
   * Activity teardown: schedule the site profile's flush on the background
   * executor (the same durability attempt the launcher's onDestroy gives the
   * default profile), then destroy the view. The flush is **never awaited**
   * and never runs on the UI thread — `flush()` may block on disk I/O — so
   * this path makes no durability claim: the write either lands during the
   * process's remaining lifetime or the periodic sync/normal close already
   * persisted it earlier. Never touches the default profile.
   */
  fun destroy(activity: Activity) {
    val webView = active.activeToken
    val profileName = active.clear()
    exitCustomViewIfNeeded()
    backCallback?.isEnabled = false
    if (webView == null) return
    if (profileName != null) {
      try {
        val profile = WebViewCompat.getProfile(webView)
        FLUSH_EXECUTOR.execute {
          try {
            profile.cookieManager.flush()
            AnimeHubDebugLog.add("site", "destroy-time flush completed")
          } catch (e: Exception) {
            Log.w(LOG_TAG, "profile flush on destroy failed (${e.javaClass.simpleName})")
            AnimeHubDebugLog.add("site", "destroy-time flush failed (${e.javaClass.simpleName})")
          }
        }
      } catch (e: Exception) {
        Log.w(LOG_TAG, "profile flush on destroy skipped (${e.javaClass.simpleName})")
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
    AnimeHubDebugLog.add("site", "destroyed with activity (flush scheduled, not awaited)")
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
        val webView = active.activeToken
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
   * `close_site` flow (Rust state, `animehub://site-closed`, native view
   * close through this controller) by evaluating the launcher-side hook.
   * The hook only reports whether a close was *started* — it never claims
   * success. Kotlin then polls `window.__animehubBackStatus()`:
   *  * `closed` → Rust already drove the whole transition (view gone);
   *  * `failed:<msg>` → surface it with a native Toast (the launcher's own
   *    notice would sit *behind* the site view) and keep everything so the
   *    next back press retries;
   *  * `pending` → keep waiting within a budget (a flush may be slow);
   *  * no answer → close natively through the same flush-first path.
   * Every step re-checks that the captured view is still the active one, so
   * a stale callback can never close a newer site.
   */
  private fun closeViaLauncher(activity: Activity) {
    val captured = active.activeToken ?: return
    val launcher = launcherWebView?.get()
    if (launcher == null || launcher === captured) {
      nativeFallbackClose(activity, captured)
      return
    }
    try {
      launcher.evaluateJavascript(BACK_HOOK_SCRIPT) { result ->
        if (result == "true") {
          pollBackStatus(activity, launcher, captured, 0)
        } else {
          AnimeHubDebugLog.add("site", "launcher back hook unavailable; closing natively")
          nativeFallbackClose(activity, captured)
        }
      }
    } catch (_: Exception) {
      AnimeHubDebugLog.add("site", "launcher back hook failed; closing natively")
      nativeFallbackClose(activity, captured)
    }
  }

  /** UI thread. One poll of the launcher close status; see [closeViaLauncher]. */
  private fun pollBackStatus(activity: Activity, launcher: WebView, captured: WebView, attempt: Int) {
    if (active.activeToken !== captured) return
    try {
      launcher.evaluateJavascript(BACK_STATUS_SCRIPT) { raw ->
        if (active.activeToken !== captured) return@evaluateJavascript
        val status =
          try {
            JSONTokener(raw).nextValue() as? String
          } catch (_: Exception) {
            null
          }
        when (SiteBackStatus.decide(status, attempt, BACK_MAX_POLLS)) {
          BackCloseDecision.POLL ->
            mainHandler.postDelayed({ pollBackStatus(activity, launcher, captured, attempt + 1) }, BACK_POLL_MS)
          BackCloseDecision.CLOSED ->
            AnimeHubDebugLog.add("site", "closed via launcher close_site flow")
          BackCloseDecision.FAILED -> {
            val message = SiteBackStatus.failureMessage(status)
            AnimeHubDebugLog.add("site", "launcher close failed")
            toast(activity, "Site kapatılamadı: $message — tekrar deneyebilirsiniz")
          }
          BackCloseDecision.FALLBACK -> nativeFallbackClose(activity, captured)
          BackCloseDecision.GIVE_UP ->
            AnimeHubDebugLog.add("site", "close still pending after poll budget")
        }
      }
    } catch (_: Exception) {
      nativeFallbackClose(activity, captured)
    }
  }

  /**
   * Direct flush-first close for when the launcher cannot run (or answer)
   * the normal flow. Failures are surfaced with a Toast — never swallowed:
   * the view stays open, the Rust session stays claimed and the user can
   * retry from the back button.
   */
  private fun nativeFallbackClose(activity: Activity, captured: WebView) {
    if (active.activeToken !== captured) return
    close(activity, flush = true) { error ->
      if (error != null) {
        AnimeHubDebugLog.add("site", "native close failed")
        toast(activity, "Site kapatılamadı: $error — tekrar deneyebilirsiniz")
      }
    }
  }

  /** A short system message — the only surface guaranteed above the site view. */
  private fun toast(activity: Activity, message: String) {
    try {
      activity.runOnUiThread {
        Toast.makeText(activity.applicationContext, message, Toast.LENGTH_LONG).show()
      }
    } catch (_: Exception) {
      Log.w(LOG_TAG, "close-failure toast failed")
    }
  }

  // ----------------------------------------------------------- profile data

  /**
   * Clear one site profile in place: profile cookies (`removeAllCookies`, its
   * delivery interpreted as a [CookieWipe] — the provider's boolean says
   * whether anything was deleted, so an already-empty jar is a SUCCESS and
   * only an unreachable store is a failure), every stored geolocation
   * permission (awaited, behind a [StepGuard]), `WebStorage.deleteAllData()`
   * and a final flush. The sequence, watchdogs and error aggregation live in
   * [SiteClearFlow]; success is only reported when every required step
   * completed, so Rust keeps its encrypted session blobs on any failure and
   * the user can retry.
   *
   * TIMEOUTS BOUND THE SIDE EFFECTS TOO, not just the answer: the awaited
   * geolocation wipe claims its step guard before the first `geo.clear`
   * ([GeolocationWipe]), so a callback that arrives after the watchdog fired
   * changes nothing on a profile that may already back a new session; and a
   * `removeAllCookies` timeout — a destructive provider call that cannot be
   * cancelled — quarantines the profile in [ProfileQuarantine], so
   * [migrate]/[open] refuse it until the wipe is verified.
   *
   * The profile itself survives, so the next open of the same site starts
   * logged out without a delete/recreate race. HTTP cache and
   * service-worker registrations are NOT part of this path — no
   * profile-scoped API clears them; documented in
   * docs/android-storage-isolation.md.
   *
   * Unsupported providers report [SiteProfileErrors.UNSUPPORTED] — never a
   * success over data this build cannot address.
   */
  fun clearProfileData(activity: Activity, profileName: String, onResult: (error: String?) -> Unit) {
    if (!profileNameIsValid(profileName)) {
      onResult("geçersiz profil adı")
      return
    }
    if (active.activeProfile == profileName) {
      onResult("aktif site verisi önce kapatılmalı")
      return
    }
    activity.runOnUiThread {
      if (!liveActivity(activity)) {
        onResult("Android Activity artık kullanılamıyor")
        return@runOnUiThread
      }
      val port =
        object : SiteClearPort {
          private var cached: Profile? = null

          override fun profileExists(): Boolean {
            val profile = ProfileStore.getInstance().getProfile(profileName) ?: return false
            cached = profile
            return true
          }

          override fun removeAllCookies(onDone: (CookieWipe) -> Unit) {
            val profile = cached
            if (profile == null) {
              // A real error, NOT "nothing to delete": [profileExists] ran
              // before this step, so losing the profile here means it vanished
              // underneath the wipe. Handing the flow `Answered(false)` would
              // let an unreachable store be read as a clean one.
              onDone(CookieWipe.Failed("profil bu adımda erişilebilir değil"))
              return
            }
            try {
              profile.cookieManager.removeAllCookies { deleted ->
                // `deleted` is Chromium's "were ANY cookies deleted" — an
                // outcome report, not an error flag: an already-empty jar
                // answers false for a wipe that fully succeeded. Pass the raw
                // answer through; [SiteClearFlow.interpretCookieWipe] decides,
                // with [cookiesCleared] as the tie-breaker.
                activity.runOnUiThread { onDone(CookieWipe.Answered(deleted == true)) }
              }
            } catch (e: Exception) {
              activity.runOnUiThread {
                onDone(CookieWipe.Failed("${e.javaClass.simpleName}: ${e.message}"))
              }
            }
          }

          override fun cookiesCleared(): Boolean? {
            val profile = cached ?: return null
            // The only store-wide emptiness probe the platform offers:
            // `getCookie(url)` needs an origin and this path deliberately does
            // not carry the site URL. Read-only and synchronous, and consulted
            // ONLY when the provider said it deleted nothing — so it reads a
            // stable store instead of racing the wipe it verifies. Anything
            // unexpected (no profile, a throw) is `null` = indeterminate,
            // which the flow reports as a retryable failure, never as success.
            // Conservative in the other direction too: session cookies the
            // provider counts but did not delete make this `false`, i.e. a
            // visible failure the user can retry, not a silent claim.
            return runCatching { !profile.cookieManager.hasCookies() }.getOrNull()
          }

          override fun deleteAllStorage() {
            val profile = cached ?: error("profil yok")
            profile.webStorage.deleteAllData()
          }

          override fun clearGeolocationPermissions(guard: StepGuard, onDone: (Boolean) -> Unit) {
            val profile = cached
            if (profile == null) {
              if (guard.claim()) onDone(false)
              return
            }
            val geo: GeolocationPermissions = profile.geolocationPermissions
            // `GeolocationPermissions` exposes no removeAll-with-callback, so
            // this step awaits `getOrigins` and performs the `clear` calls
            // itself. That sequencing — including claiming [guard] BEFORE the
            // first mutation, so a callback that lost to the watchdog clears
            // nothing — is [GeolocationWipe]: pure production code the CI unit
            // tests drive against a recording fake. This adapter only supplies
            // the two provider calls and keeps the answer on the UI thread,
            // which is also where the watchdog is posted, so claim+mutate is
            // one serialised block.
            GeolocationWipe.start(
              guard,
              object : GeolocationApi {
                override fun getOrigins(onResult: (Set<String>?) -> Unit) {
                  geo.getOrigins { origins -> activity.runOnUiThread { onResult(origins) } }
                }

                override fun clear(origin: String) {
                  geo.clear(origin)
                }
              },
            ) { ok ->
              if (!ok) {
                Log.w(LOG_TAG, "geolocation wipe incomplete (op ${guard.operationId})")
                AnimeHubDebugLog.add("site", "geo wipe incomplete (op ${guard.operationId})")
              }
              onDone(ok)
            }
          }

          override fun flushCookies(onDone: (Throwable?) -> Unit) {
            val profile = cached
            if (profile == null) {
              onDone(IllegalStateException("profil yok"))
              return
            }
            submitFlush(activity, "clear", { profile.cookieManager.flush() }, onDone)
          }

          override fun scheduleTimeout(millis: Long, onTimeout: () -> Unit) {
            try {
              mainHandler.postDelayed({ onTimeout() }, millis)
            } catch (_: Exception) {
              // Dead looper ⇒ dead process; nothing to time out.
            }
          }
        }
      SiteClearFlow.run(isSupported(), profileName, port) { error ->
        when (error) {
          null -> AnimeHubDebugLog.add("site", "profile data cleared")
          SiteProfileErrors.UNSUPPORTED -> {
            AnimeHubDebugLog.add("site", "clear refused: profile feature unavailable")
            Log.w(LOG_TAG, "profile clear refused: MULTI_PROFILE unsupported")
          }
          else -> {
            Log.w(LOG_TAG, "profile clear failed: $error")
            AnimeHubDebugLog.add("site", "clear failed (${error.take(60)})")
            if (ProfileQuarantine.isBlocked(profileName)) {
              // A destructive provider call is still outstanding (or the last
              // verification predates it): the site is refused until that is
              // resolved (see SiteOpenGate), so say so here instead of leaving
              // the user with a bare failure.
              val pending = ProfileQuarantine.pendingOperations(profileName).size
              Log.w(LOG_TAG, "profile blocked until its wipe is verified: $profileName ($pending pending)")
              AnimeHubDebugLog.add("site", "profile blocked (unverified wipe, $pending pending)")
            }
          }
        }
        onResult(error)
      }
    }
  }

  /**
   * Delete a site profile's data.
   *
   * Resolves `{deleted, pending, error}`:
   *  * `deleted=true`  — the provider accepted the deletion (disk work may
   *    still finish asynchronously; Rust bumps the generation so the same
   *    name is never immediately recreated).
   *  * `deleted=false, pending=false, error=null` — the profile does not
   *    exist (already clean).
   *  * `deleted=false, pending=true` — the profile is loaded in this process
   *    (Chromium refuses to delete an in-use context). Rust queues the name
   *    and retries at the next process start, before any site can open, and
   *    reports it as *deferred*, never as deleted.
   *  * `deleted=false, pending=false, error=unsupported-multi-profile` — the
   *    provider cannot address profiles at all. Distinct from deferred: Rust
   *    reports the feature gap and queues nothing.
   *
   * Rejects (hard failure — the caller must NOT drop its record) for invalid
   * names, an active WebView on that profile, or unexpected exceptions.
   *
   * A deletion the provider accepted (or an absent profile) RETIRES the name
   * in [ProfileQuarantine] — dropping its outstanding-operation ledger too,
   * since Rust bumps the generation and the name is never handed out again —
   * while a *deferred* deletion keeps every record.
   */
  fun deleteProfileData(profileName: String, onResult: (deleted: Boolean, pending: Boolean, error: String?) -> Unit) {
    if (!profileNameIsValid(profileName)) {
      onResult(false, false, "geçersiz profil adı")
      return
    }
    if (active.activeProfile == profileName) {
      onResult(false, false, "aktif site profili önce kapatılmalı")
      return
    }
    if (!isSupported()) {
      // No profile can be addressed through this API — report the feature
      // gap honestly instead of pretending a deletion was deferred.
      onResult(false, false, ERR_UNSUPPORTED_MULTI_PROFILE)
      return
    }
    // NOTE: no getProfile/getOrCreateProfile here on purpose — merely loading
    // a profile makes the provider refuse deletion for the rest of the
    // process (AwBrowserContextStore::Delete returns in-use).
    try {
      val deleted = ProfileStore.getInstance().deleteProfile(profileName)
      // A deletion the provider did NOT refuse retires this name for good:
      // Rust bumps the site's generation and never hands the name out again,
      // so no new session can ever sit on it — which is the whole reason a
      // quarantine block exists. Drop the name's ledger here, including any
      // operation still outstanding: those callbacks can only land on a
      // profile no session will ever use, and the NEW generation starts
      // unblocked. The deferred branch below keeps every record, because the
      // profile still exists and could be reopened under the same name.
      val dropped = ProfileQuarantine.retire(profileName)
      if (dropped > 0) {
        AnimeHubDebugLog.add("site", "profile retired ($dropped pending wipe op(s) dropped)")
      }
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
    val view = active.activeToken
    if (view == null) {
      onResult(null)
      return
    }
    val profile: Profile =
      try {
        WebViewCompat.getProfile(view)
      } catch (e: Exception) {
        onResult("profil çözümlenemedi: ${e.message}")
        return
      }
    submitFlush(activity, "sync", { profile.cookieManager.flush() }) { error ->
      onResult(
        if (error == null) null else "çerez kalıcılığı başarısız: ${error.message}",
      )
    }
  }

  // -------------------------------------------------------- fullscreen video

  /**
   * `WebChromeClient` fullscreen video. The site view is hidden and the
   * player view is added above it; back exits fullscreen first (handled in
   * [ensureBackCallback]). wry's own client declines custom views, so this
   * is an intentional capability of the new site WebView, documented as such.
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
    val activity = active.activeToken?.context as? Activity
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
    active.activeToken?.visibility = View.INVISIBLE
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
    active.activeToken?.visibility = View.VISIBLE
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
