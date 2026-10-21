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
// * Activity teardown hands the site profile's flush to the background
//   executor (the disk I/O never runs on the UI thread) and awaits it for a
//   BOUNDED budget, so the process kill the generated Activity performs right
//   after cannot land mid-write; a timeout or a failure is logged as
//   "durability not guaranteed" instead of being claimed;
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
import android.webkit.PermissionRequest
import android.webkit.WebChromeClient
import android.webkit.WebSettings
import android.webkit.WebView
import android.webkit.WebViewClient
import androidx.activity.OnBackPressedCallback
import androidx.webkit.Profile
import androidx.webkit.ProfileStore
import androidx.webkit.WebViewCompat
import androidx.webkit.WebViewFeature
import java.lang.ref.WeakReference
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicReference
import org.json.JSONObject
import org.json.JSONTokener

/**
 * Owns the site WebView: creation with its own profile, show/hide over the
 * launcher, lifecycle forwarding, back navigation, fullscreen video and
 * profile data operations (clear / delete).
 *
 * All WebView and ProfileStore work runs on the UI thread (both APIs are
 * `@UiThread`); the blocking `CookieManager.flush()` always runs on
 * [SiteFlushWorker]'s single serialised worker, never on the UI thread. Flow
 * sequencing (close / clear /
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
  // The profile-name rule lives ONCE, in SiteProfileOps.kt (`SITE_PROFILE_NAME`),
  // and is shared with [ProfileRetirement.refusal] so the two cannot drift.

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
  // The generated operation id is quoted as a JS string so correlation cannot
  // turn into script injection if its format ever changes.
  private fun backHookScript(operationId: String): String =
    "!!(window.__animehubBackToLauncher && (window.__animehubBackToLauncher(${JSONObject.quote(operationId)}), true))"
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

  private fun profileNameIsValid(name: String): Boolean = SITE_PROFILE_NAME.matches(name)

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
  private fun quarantineRefusal(
    profileName: String,
    operationId: String,
  ): String? = SiteOpenGate.check(profileName)?.also {
    val pending = ProfileQuarantine.pendingOperations(profileName).size
    Log.w(LOG_TAG, "site open refused by quarantine gate")
    AnimeHubDebugLog.record(
      "profile", "quarantine", "quarantined", operationId,
      errorClass = "quarantined", count = pending.toLong(),
    )
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
    operationId: String,
    onResult: (error: String?) -> Unit,
  ) {
    if (!isSupported()) {
      AnimeHubDebugLog.record("profile", "capability", "unsupported_provider", operationId, errorClass = "unsupported")
      onResult(ERR_UNSUPPORTED_MULTI_PROFILE)
      return
    }
    AnimeHubDebugLog.record("profile", "capability", "supported", operationId)
    if (!profileNameIsValid(profileName)) {
      AnimeHubDebugLog.record("profile", "profile_check", "invalid", operationId, errorClass = "invalid_profile")
      onResult("geçersiz profil adı")
      return
    }
    AnimeHubDebugLog.record("profile", "profile_check", "ok", operationId)
    // Quarantine gate, BEFORE the provider is touched: writing the legacy jar
    // into a profile whose previous wipe may still be in flight would hand
    // those cookies to a late `removeAllCookies`. Rust writes its migration
    // flag only after a success, so refusing here leaves the jar and the flag
    // exactly as they were.
    val quarantined = quarantineRefusal(profileName, operationId)
    if (quarantined != null) {
      onResult(quarantined)
      return
    }
    activity.runOnUiThread {
      if (!liveActivity(activity)) {
        AnimeHubDebugLog.record("migration", "complete", "failed", operationId, errorClass = "activity_gone")
        onResult("Android Activity artık kullanılamıyor")
        return@runOnUiThread
      }
      if (active.hasActive()) {
        AnimeHubDebugLog.record("migration", "complete", "failed", operationId, errorClass = "active_site")
        onResult(ERR_ACTIVE_SITE)
        return@runOnUiThread
      }
      if (cookies.isEmpty()) {
        // No jar (or every cookie in it expired): there is nothing to await
        // and no profile to create here — [open] binds the view to the name
        // and the provider materialises it. The flag still becomes durable
        // before any view exists, exactly like the imported case.
        AnimeHubDebugLog.record("migration", "start", "started", operationId, count = 0)
        AnimeHubDebugLog.record("migration", "complete", "empty", operationId, count = 0)
        onResult(null)
        return@runOnUiThread
      }
      try {
        migrateCookies(activity, url, profileName, cookies, operationId, onResult)
      } catch (_: Exception) {
        Log.e(LOG_TAG, "cookie migration failed")
        AnimeHubDebugLog.record("migration", "complete", "failed", operationId, errorClass = "provider")
        onResult("$ERR_MIGRATION_RETRY: migration failed")
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
    operationId: String,
    onResult: (error: String?) -> Unit,
  ) {
    AnimeHubDebugLog.record("open", "start", "started", operationId)
    if (!isSupported()) {
      AnimeHubDebugLog.record("profile", "capability", "unsupported_provider", operationId, errorClass = "unsupported")
      onResult(ERR_UNSUPPORTED_MULTI_PROFILE)
      return
    }
    AnimeHubDebugLog.record("profile", "capability", "supported", operationId)
    if (!profileNameIsValid(profileName)) {
      AnimeHubDebugLog.record("profile", "profile_check", "invalid", operationId, errorClass = "invalid_profile")
      onResult("geçersiz profil adı")
      return
    }
    // Same quarantine gate as phase 1: a WebView bound to a profile whose
    // wipe is still unverified could have its fresh cookies deleted by the
    // provider call that timed out. Nothing is claimed and nothing is built
    // on this path, so Rust's `current_site` stays empty and the refusal is
    // simply retryable once the wipe is verified.
    val quarantined = quarantineRefusal(profileName, operationId)
    if (quarantined != null) {
      onResult(quarantined)
      return
    }
    activity.runOnUiThread {
      if (!liveActivity(activity)) {
        AnimeHubDebugLog.record("open", "complete", "failed", operationId, errorClass = "activity_gone")
        onResult("Android Activity artık kullanılamıyor")
        return@runOnUiThread
      }
      if (active.hasActive()) {
        AnimeHubDebugLog.record("open", "complete", "failed", operationId, errorClass = "active_site")
        onResult(ERR_ACTIVE_SITE)
        return@runOnUiThread
      }
      try {
        buildWebView(activity, url, profileName, initScript, operationId, onResult)
      } catch (_: Exception) {
        Log.e(LOG_TAG, "site open failed")
        AnimeHubDebugLog.record("open", "complete", "failed", operationId, errorClass = "provider")
        onResult("site açılamadı")
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
    operationId: String,
    onResult: (error: String?) -> Unit,
  ) {
    val port = migrationPort(activity, url, profileName, operationId)
    SiteMigrationFlow.run(cookies, port, operationId) { outcome ->
      when (outcome) {
        is MigrationOutcome.Complete -> onResult(null)
        MigrationOutcome.ProfileAlreadyExists -> onResult("$ERR_MIGRATION_RETRY: profile already exists")
        is MigrationOutcome.Failed -> {
          Log.w(LOG_TAG, "cookie migration failed")
          onResult("$ERR_MIGRATION_RETRY: migration incomplete")
        }
      }
    }
  }

  /** Android wiring for [SiteMigrationFlow]; callbacks land on the UI thread. */
  private fun migrationPort(activity: Activity, url: String, profileName: String, operationId: String): SiteMigrationPort =
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
        SiteFlushWorker.submit(activity, "migration", { m.flush() }, operationId, onDone)
      }

      override fun scheduleTimeout(millis: Long, onTimeout: () -> Unit): Watchdog =
        SiteFlushWorker.scheduleWatchdog(millis, onTimeout)
    }

  /** Construct, configure, attach and navigate the site WebView. UI thread. */
  private fun buildWebView(
    activity: Activity,
    url: String,
    profileName: String,
    initScript: String,
    operationId: String,
    onResult: (error: String?) -> Unit,
  ) {
    var webView: WebView? = null
    var attached = false
    val buildStarted = System.nanoTime()
    try {
      // (1) Constructor, (2) profile — before ANY other WebView API.
      AnimeHubDebugLog.record("open", "view_create", "started", operationId)
      webView = WebView(activity)
      AnimeHubDebugLog.record("open", "view_create", "ok", operationId)
      AnimeHubDebugLog.record("open", "profile_bind", "started", operationId)
      WebViewCompat.setProfile(webView, profileName)
      AnimeHubDebugLog.record("open", "profile_bind", "ok", operationId)

      // (3) Settings: the decided plan from `SiteWebViewSettings`, applied
      // through the only adapter that touches the platform object. JavaScript,
      // DOM/web storage, autoplay and same-view window.open stay on so site
      // behaviour matches the shared WebView this replaces; file and content
      // access, both file-URL escape hatches and mixed content are OFF (the
      // platform defaults are not), and geolocation is off because there is no
      // consent flow to grant it. Every value and its reason is in the plan,
      // which is unit-tested.
      applySiteSettings(WebSettingsSink(webView.settings))

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
      //
      // ORIGIN SCOPE (AH-P2) - why this stays setOf("*"), and what a page
      // actually gets from it:
      //
      //  * The rule set cannot honestly be narrowed to the site's own origin.
      //    The players these sites embed and the ad frames that call
      //    window.open are third-party origins, and this API has no
      //    "top frame only" rule - an origin allowlist would drop the popup
      //    guard and the cosmetic rules exactly where they are needed, and
      //    could break playback, login redirects and PiP. There is no device
      //    or emulator matrix behind this code, so narrowing it on a guess
      //    would trade a documented, capability-free surface for an unverified
      //    functional risk. This item is therefore NOT counted as fixed; it is
      //    scoped and documented.
      //  * What every frame gets is capability-free by construction: the
      //    cosmetic style element (selectors are data, not code), a
      //    window.open override that returns null and keeps no reference to
      //    the original, and an install flag. No frame receives a site
      //    identity, a credential or any per-site value (the payload is the
      //    three display settings), and no frame receives an IPC bridge - this
      //    view has no JavascriptInterface and Tauri disables IPC for external
      //    URLs.
      //  * Only the TOP frame installs the PiP controller: pip_controller.js
      //    returns early in a subframe, so a third-party frame is not handed
      //    the __animehubPip* entry points. PiP never depended on this copy -
      //    AnimeHubPipController embeds the same file and installs it in the
      //    main frame on demand before entering PiP.
      //  * Residual impact, stated plainly: a cross-origin frame can still
      //    detect that it is embedded in this app (the cosmetic style element,
      //    the overridden window.open) and can hide ad containers inside its
      //    own document. Neither is a capability, and neither reaches another
      //    origin's data. Verified by unit tests on the injected text
      //    (Rust session.rs, tests/init_script.test.js, tests/pip_controller
      //    .test.js) - NOT on a physical device.
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
      AnimeHubDebugLog.record("open", "view_attach", "ok", operationId)
      webView.isFocusable = true
      webView.isFocusableInTouchMode = true
      webView.requestFocus()

      // (7) PiP follows the visible page: bind the controller to this view.
      AnimeHubPipController.attach(activity, webView, operationId)

      // (8) Back button: history first, launcher second, never the task.
      ensureBackCallback(activity).isEnabled = true

      // (9) Claim the slot BEFORE navigating: from here on, any failure must
      // release exactly this view (see the catch block).
      if (!active.acquire(webView, profileName)) {
        throw IllegalStateException("aktif site görünümü zaten var")
      }
      // The migration (if any) completed and its flag is durable before this
      // view exists — phase 2 only builds and navigates.
      AnimeHubDebugLog.record("open", "slot_claim", "ok", operationId)

      AnimeHubDebugLog.record("open", "navigation_start", "started", operationId)
      webView.loadUrl(url)
      AnimeHubDebugLog.record(
        "open", "complete", "ok", operationId,
        durationMs = (System.nanoTime() - buildStarted) / 1_000_000,
      )
      onResult(null)
    } catch (_: Exception) {
      Log.e(LOG_TAG, "site WebView setup failed")
      AnimeHubDebugLog.record(
        "open", "complete", "failed", operationId,
        durationMs = (System.nanoTime() - buildStarted) / 1_000_000,
        errorClass = "provider",
      )
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
        transferPipToLauncher(activity, operationId)
      }
      onResult("site açılamadı")
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
  fun close(activity: Activity, flush: Boolean, operationId: String, onResult: (error: String?) -> Unit) {
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
            SiteFlushWorker.submit(activity, "close", { profile.cookieManager.flush() }, operationId, onDone)
          }

          override fun scheduleTimeout(millis: Long, onTimeout: () -> Unit): Watchdog =
            SiteFlushWorker.scheduleWatchdog(millis, onTimeout)

          override fun destroyActiveView() {
            finishClose(activity, operationId)
          }
        }
      SiteCloseFlow.run(port, flush, operationId) { error ->
        if (error != null) {
          Log.w(LOG_TAG, "site close did not complete")
        }
        onResult(error)
      }
    }
  }

  /** UI thread. Remove, destroy, re-bind PiP/focus to the launcher. */
  private fun finishClose(activity: Activity, operationId: String) {
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
    // PiP belongs to whatever the user sees now: the launcher. A missing
    // launcher or a failed native attach is diagnostic, not a close failure.
    transferPipToLauncher(activity, operationId)
    AnimeHubDebugLog.record("close", "destroy", "ok", operationId)
  }

  private fun transferPipToLauncher(activity: Activity, operationId: String) {
    val launcher = launcherWebView?.get()
    PipTransferFlow.transferToLauncher(
      operationId = operationId,
      launcherAvailable = launcher != null,
      attach = { launcher?.let { AnimeHubPipController.attach(activity, it, operationId) } ?: false },
      focus = { launcher?.requestFocus() == true },
    )
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
   * UI thread, from the generated `MainActivity.onDestroy`.
   *
   * The site profile's cookie flush is submitted to [SiteFlushWorker] — disk I/O
   * never runs here — and then awaited for
   * [ShutdownFlow.DESTROY_FLUSH_BUDGET_MS], because the generated Activity ends
   * the process right after this when it is finishing: an unawaited flush could
   * be killed mid-write and lose the site's cookies. The outcome is reported
   * honestly ([ShutdownFlush]): a timeout or a failure is logged as "durability
   * NOT guaranteed", never as a completed flush, and the default-profile
   * `CookieManager.getInstance().flush()` the generated Activity performs is NOT
   * treated as evidence about this profile's store. Teardown of the view runs
   * regardless of the flush outcome; a second call finds the session slot
   * already cleared and flushes nothing.
   */
  fun destroy(activity: Activity, operationId: String = AnimeHubDebugLog.newOperationId()) {
    val webView = active.activeToken
    val profileName = active.clear()
    exitCustomViewIfNeeded()
    backCallback?.isEnabled = false
    if (webView == null) {
      AnimeHubDebugLog.record("shutdown", "destroy", "started", operationId)
      AnimeHubDebugLog.record("shutdown", "destroy_flush", "not_needed", operationId)
      AnimeHubDebugLog.record("shutdown", "complete", "ok", operationId)
      return
    }
    // Resolved BEFORE the view is destroyed, and the ONLY thing the worker task
    // captures: a `Profile` comes from the store and outlives its WebView,
    // while capturing the Activity (or the view) here would leak it past
    // teardown. The completion state below is local to THIS call, so a double
    // destroy — or a flush belonging to an older session — cannot be counted
    // here.
    val finished = CountDownLatch(1)
    val failure = AtomicReference<String?>(null)
    val profile =
      if (profileName != null) {
        try {
          WebViewCompat.getProfile(webView)
        } catch (_: Exception) {
          failure.set("provider")
          Log.w(LOG_TAG, "profile flush on destroy skipped")
          null
        }
      } else {
        null
      }
    val outcome =
      ShutdownFlow.run(
        object : ShutdownPort {
          override fun submitFlush(): Boolean {
            val target = profile ?: return false
            // Disk I/O on the single flush worker, never on this (UI) thread —
            // and through the worker, so a stuck provider flush is observable
            // here too instead of only on the awaited paths.
            // Destroy-time flush is best effort: a refusal (worker budget
            // exhausted) is already recorded by the worker itself.
            SiteFlushWorker.execute("destroy", operationId) {
              try {
                target.cookieManager.flush()
              } catch (error: Exception) {
                failure.set("provider")
                Log.w(LOG_TAG, "profile flush on destroy failed")
                throw error
              } finally {
                finished.countDown()
              }
            }
            return true
          }

          override fun teardownView() {
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
          }

          override fun awaitFlush(budgetMillis: Long): Boolean =
            try {
              finished.await(budgetMillis, TimeUnit.MILLISECONDS)
            } catch (_: InterruptedException) {
              // Interrupted teardown: report honestly, never as a landed flush.
              Thread.currentThread().interrupt()
              false
            }

          override fun flushFailure(): String? = failure.get()
        },
        operationId = operationId,
      )
    if (outcome == ShutdownFlush.TimedOut) {
      // The generated Activity kills the process right after this when it is
      // finishing, so a timeout means the write may never land: say so.
      Log.w(
        LOG_TAG,
        "destroy-time flush unfinished in ${ShutdownFlow.DESTROY_FLUSH_BUDGET_MS}ms; " +
          "cookie durability NOT guaranteed",
      )
    }
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
        val operationId = AnimeHubDebugLog.newOperationId()
        AnimeHubDebugLog.record("back", "back_pressed", "started", operationId)
        AnimeHubDebugLog.record("back", "back_step", "started", operationId, stepId = 0)
        if (customView != null) {
          exitCustomViewIfNeeded()
          AnimeHubDebugLog.record("back", "fullscreen", "handled", operationId, stepId = 0)
          AnimeHubDebugLog.record("back", "back_step", "handled", operationId, stepId = 0)
          return
        }
        AnimeHubDebugLog.record("back", "fullscreen", "skipped", operationId, stepId = 0)
        AnimeHubDebugLog.record("back", "back_step", "passed", operationId, stepId = 0)
        AnimeHubDebugLog.record("back", "back_step", "started", operationId, stepId = 1)
        val webView = active.activeToken
        if (webView != null && webView.canGoBack()) {
          webView.goBack()
          AnimeHubDebugLog.record("back", "web_history", "handled", operationId, stepId = 1)
          AnimeHubDebugLog.record("back", "back_step", "handled", operationId, stepId = 1)
          return
        }
        AnimeHubDebugLog.record("back", "web_history", "skipped", operationId, stepId = 1)
        AnimeHubDebugLog.record("back", "back_step", "passed", operationId, stepId = 1)
        AnimeHubDebugLog.record("back", "back_step", "started", operationId, stepId = 2)
        AnimeHubDebugLog.record("back", "launcher_close", "started", operationId)
        closeViaLauncher(activity, operationId)
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
  private fun closeViaLauncher(activity: Activity, operationId: String) {
    val captured = active.activeToken ?: return
    val launcher = launcherWebView?.get()
    if (launcher == null || launcher === captured) {
      AnimeHubDebugLog.record("back", "launcher_close", "fallback", operationId, errorClass = "unavailable")
      nativeFallbackClose(activity, captured, operationId)
      return
    }
    try {
      launcher.evaluateJavascript(backHookScript(operationId)) { result ->
        if (result == "true") {
          AnimeHubDebugLog.record("back", "launcher_close", "started", operationId)
          pollBackStatus(activity, launcher, captured, 0, operationId)
        } else {
          AnimeHubDebugLog.record("back", "launcher_close", "fallback", operationId, errorClass = "unavailable")
          nativeFallbackClose(activity, captured, operationId)
        }
      }
    } catch (_: Exception) {
      AnimeHubDebugLog.record("back", "launcher_close", "fallback", operationId, errorClass = "provider")
      nativeFallbackClose(activity, captured, operationId)
    }
  }

  /** UI thread. One poll of the launcher close status; see [closeViaLauncher]. */
  private fun pollBackStatus(
    activity: Activity,
    launcher: WebView,
    captured: WebView,
    attempt: Int,
    operationId: String,
  ) {
    if (active.activeToken !== captured) return
    AnimeHubDebugLog.record("back", "poll", "started", operationId, count = attempt.toLong())
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
          BackCloseDecision.POLL -> {
            AnimeHubDebugLog.record("back", "poll", "pending", operationId, count = attempt.toLong())
            mainHandler.postDelayed(
              { pollBackStatus(activity, launcher, captured, attempt + 1, operationId) },
              BACK_POLL_MS,
            )
          }
          BackCloseDecision.CLOSED -> {
            AnimeHubDebugLog.record("back", "poll", "closed", operationId, count = attempt.toLong())
            AnimeHubDebugLog.record("back", "launcher_close", "closed", operationId)
            AnimeHubDebugLog.record("back", "back_step", "closed", operationId, stepId = 2)
          }
          BackCloseDecision.FAILED -> {
            AnimeHubDebugLog.record("back", "poll", "failed", operationId, count = attempt.toLong(), errorClass = "provider")
            AnimeHubDebugLog.record("back", "launcher_close", "failed", operationId, errorClass = "provider")
            AnimeHubDebugLog.record("back", "back_step", "failed", operationId, stepId = 2)
            toast(activity, "Site kapatılamadı; tekrar deneyebilirsiniz")
          }
          BackCloseDecision.FALLBACK -> {
            AnimeHubDebugLog.record("back", "poll", "fallback", operationId, count = attempt.toLong())
            AnimeHubDebugLog.record("back", "back_step", "fallback", operationId, stepId = 2)
            nativeFallbackClose(activity, captured, operationId)
          }
          BackCloseDecision.GIVE_UP -> {
            AnimeHubDebugLog.record(
              "back", "poll", "timeout", operationId,
              durationMs = (attempt * BACK_POLL_MS), watchdog = "fired", errorClass = "timeout",
              count = attempt.toLong(),
            )
            AnimeHubDebugLog.record(
              "back", "launcher_close", "pending", operationId,
              durationMs = (attempt * BACK_POLL_MS), watchdog = "fired", errorClass = "timeout",
            )
            AnimeHubDebugLog.record("back", "back_step", "pending", operationId, stepId = 2)
          }
        }
      }
    } catch (_: Exception) {
      AnimeHubDebugLog.record("back", "launcher_close", "fallback", operationId, errorClass = "provider")
      nativeFallbackClose(activity, captured, operationId)
    }
  }

  /**
   * Direct flush-first close for when the launcher cannot run (or answer)
   * the normal flow. Failures are surfaced with a Toast — never swallowed:
   * the view stays open, the Rust session stays claimed and the user can
   * retry from the back button.
   */
  private fun nativeFallbackClose(activity: Activity, captured: WebView, operationId: String) {
    if (active.activeToken !== captured) return
    AnimeHubDebugLog.record("back", "native_close", "started", operationId)
    close(activity, flush = true, operationId = operationId) { error ->
      if (error != null) {
        AnimeHubDebugLog.record("back", "native_close", "failed", operationId, errorClass = "provider")
        toast(activity, "Site kapatılamadı; tekrar deneyebilirsiniz")
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
  fun clearProfileData(
    activity: Activity,
    profileName: String,
    operationId: String,
    onResult: (error: String?) -> Unit,
  ) {
    if (!profileNameIsValid(profileName)) {
      AnimeHubDebugLog.record("clear", "profile_check", "invalid", operationId, errorClass = "invalid_profile")
      onResult("geçersiz profil adı")
      return
    }
    if (active.activeProfile == profileName) {
      AnimeHubDebugLog.record("clear", "profile_check", "failed", operationId, errorClass = "active_site")
      onResult("aktif site verisi önce kapatılmalı")
      return
    }
    activity.runOnUiThread {
      if (!liveActivity(activity)) {
        AnimeHubDebugLog.record("clear", "complete", "failed", operationId, errorClass = "activity_gone")
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
              // let an unreachable store be read as a clean one. The mapping is
              // [CookieWipes] — pure, CI-tested production code.
              onDone(CookieWipes.noProfile())
              return
            }
            try {
              profile.cookieManager.removeAllCookies { deleted ->
                // `deleted` is Chromium's "were ANY cookies deleted" — an
                // outcome report, not an error flag: an already-empty jar
                // answers false for a wipe that fully succeeded, and the
                // callback may even deliver null. [CookieWipes.answered] maps
                // both null-safely; [SiteClearFlow.interpretCookieWipe] then
                // decides, with [cookiesCleared] as the tie-breaker.
                activity.runOnUiThread { onDone(CookieWipes.answered(deleted)) }
              }
            } catch (e: Exception) {
              activity.runOnUiThread { onDone(CookieWipes.failed(e)) }
            }
          }

          override fun cookiesCleared(): Boolean? {
            val profile = cached ?: return null
            // The only store-wide emptiness probe the platform offers:
            // `getCookie(url)` needs an origin and this path deliberately does
            // not carry the site URL. Read-only and synchronous, and consulted
            // ONLY when the provider said it deleted nothing — so it reads a
            // stable store instead of racing the wipe it verifies. `hasCookies()`
            // asks the opposite question ("are there cookies?"), so the
            // negation and the throw handling live in [CookieWipes.emptinessOf],
            // which the CI unit tests pin: an unexpected result stays `null` =
            // indeterminate, which the flow reports as a retryable failure,
            // never as success. Conservative in the other direction too: session
            // cookies the provider counts but did not delete make this `false`,
            // i.e. a visible failure the user can retry, not a silent claim.
            return CookieWipes.emptinessOf { profile.cookieManager.hasCookies() }
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
                  geo.getOrigins { origins ->
                    activity.runOnUiThread {
                      AnimeHubDebugLog.record(
                        "clear", "geolocation_origins", if (origins == null) "failed" else "ok",
                        operationId,
                        count = origins?.size?.toLong(),
                        errorClass = if (origins == null) "provider" else null,
                      )
                      onResult(origins)
                    }
                  }
                }

                override fun clear(origin: String) {
                  geo.clear(origin)
                }
              },
            ) { ok ->
              if (!ok) Log.w(LOG_TAG, "geolocation wipe incomplete")
              onDone(ok)
            }
          }

          override fun flushCookies(onDone: (Throwable?) -> Unit) {
            val profile = cached
            if (profile == null) {
              onDone(IllegalStateException("profil yok"))
              return
            }
            SiteFlushWorker.submit(activity, "clear", { profile.cookieManager.flush() }, operationId, onDone)
          }

          override fun scheduleTimeout(millis: Long, onTimeout: () -> Unit): Watchdog =
            SiteFlushWorker.scheduleWatchdog(millis, onTimeout)
        }
      val supported = isSupported()
      AnimeHubDebugLog.record(
        "profile", "capability", if (supported) "supported" else "unsupported_provider",
        operationId, errorClass = if (supported) null else "unsupported",
      )
      SiteClearFlow.run(supported, profileName, port, operationId) { error ->
        when (error) {
          null -> Unit
          SiteProfileErrors.UNSUPPORTED -> {
            Log.w(LOG_TAG, "profile clear refused: MULTI_PROFILE unsupported")
          }
          else -> {
            Log.w(LOG_TAG, "profile clear failed")
            if (ProfileQuarantine.isBlocked(profileName)) {
              // A destructive provider call is still outstanding (or the last
              // verification predates it): the site is refused until that is
              // resolved (see SiteOpenGate), so say so here instead of leaving
              // the user with a bare failure.
              val pending = ProfileQuarantine.pendingOperations(profileName).size
              Log.w(LOG_TAG, "profile blocked until its wipe is verified")
              AnimeHubDebugLog.record(
                "profile", "quarantine", "blocked", operationId,
                errorClass = "cookie_unverified", count = pending.toLong(),
              )
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
   * This call NEVER retires the name in [ProfileQuarantine]: retirement is a
   * separate, Rust-sequenced step ([retireProfileQuarantine]) that runs only
   * once the generation bump is durably stored, because only then is the name
   * underivable. A *deferred* deletion keeps every record too — the profile
   * still exists on disk.
   */
  fun deleteProfileData(
    profileName: String,
    operationId: String,
    onResult: (deleted: Boolean, pending: Boolean, error: String?) -> Unit,
  ) {
    AnimeHubDebugLog.record("delete", "provider_delete", "started", operationId)
    if (!profileNameIsValid(profileName)) {
      AnimeHubDebugLog.record("delete", "profile_check", "invalid", operationId, errorClass = "invalid_profile")
      onResult(false, false, "geçersiz profil adı")
      return
    }
    if (active.activeProfile == profileName) {
      AnimeHubDebugLog.record("delete", "profile_check", "failed", operationId, errorClass = "active_site")
      onResult(false, false, "aktif site profili önce kapatılmalı")
      return
    }
    if (!isSupported()) {
      // No profile can be addressed through this API — report the feature
      // gap honestly instead of pretending a deletion was deferred.
      AnimeHubDebugLog.record("profile", "capability", "unsupported_provider", operationId, errorClass = "unsupported")
      onResult(false, false, ERR_UNSUPPORTED_MULTI_PROFILE)
      return
    }
    AnimeHubDebugLog.record("profile", "capability", "supported", operationId)
    // NOTE: no getProfile/getOrCreateProfile here on purpose — merely loading
    // a profile makes the provider refuse deletion for the rest of the
    // process (AwBrowserContextStore::Delete returns in-use).
    try {
      val deleted = ProfileStore.getInstance().deleteProfile(profileName)
      // NO retirement here. Acceptance by the provider is not proof that the
      // name is unreachable: Rust still has to bump and *durably store* the
      // generation, and that write can fail — in which case `remove_site`
      // aborts with the registry intact and the next open derives this very
      // name again. Dropping the ledger here would leave that name both
      // reachable and unblocked, with a `removeAllCookies` deletion possibly
      // still in flight on it. Rust calls [retireProfileQuarantine] once the
      // bump succeeded; the deferred branch below keeps every record as well,
      // because a pending profile still exists on disk.
      AnimeHubDebugLog.record(
        "delete", "provider_delete", if (deleted) "deleted" else "absent", operationId,
      )
      onResult(deleted, false, null)
    } catch (_: IllegalStateException) {
      // Preserve the existing retry-at-next-start decision. Provider wording
      // is not copied into logs or returned through the bridge.
      AnimeHubDebugLog.record("delete", "provider_delete", "deferred", operationId, errorClass = "in_use")
      Log.w(LOG_TAG, "profile delete deferred")
      onResult(false, true, null)
    } catch (_: Exception) {
      AnimeHubDebugLog.record("delete", "provider_delete", "failed", operationId, errorClass = "provider")
      Log.e(LOG_TAG, "profile delete failed")
      onResult(false, false, "profil verisi silinemedi")
    }
  }

  /**
   * Retire one profile name's quarantine ledger — dropping every outstanding
   * operation record with it — and report how many were dropped.
   *
   * THE CALL ORDER IS THE SAFETY PROPERTY: Rust invokes the matching
   * `site_profile_retire` bridge command ONLY after `bump_profile_generation`
   * has stored the new generation (`run_mobile_delete_protocol`), i.e. once no
   * future open can derive this name again. Called earlier — as this controller
   * used to, straight after the provider accepted a deletion — a failed bump or
   * a failed storage write left the name reachable AND unblocked, so a session
   * could reopen a profile whose `removeAllCookies` deletion was still in
   * flight. The refusal cases are decided by the pure [ProfileRetirement]
   * (an invalid name, or a profile a live session is using), never here.
   */
  fun retireProfileQuarantine(
    profileName: String,
    operationId: String,
    onResult: (dropped: Int, error: String?) -> Unit,
  ) {
    val refusal = ProfileRetirement.refusal(profileName, active.activeProfile)
    if (refusal != null) {
      AnimeHubDebugLog.record(
        "delete", "quarantine_retire", "refused", operationId,
        errorClass = if (active.activeProfile == profileName) "active_site" else "invalid_profile",
      )
      onResult(0, refusal)
      return
    }
    val dropped = ProfileQuarantine.retire(profileName)
    AnimeHubDebugLog.record("delete", "quarantine_retire", "retired", operationId, count = dropped.toLong())
    onResult(dropped, null)
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
  fun flush(activity: Activity, operationId: String, onResult: (error: String?) -> Unit) {
    val view = active.activeToken
    if (view == null) {
      onResult(null)
      return
    }
    val profile: Profile =
      try {
        WebViewCompat.getProfile(view)
      } catch (_: Exception) {
        AnimeHubDebugLog.record("flush", "flush", "failed", operationId, errorClass = "provider")
        onResult("profile could not be resolved")
        return
      }
    SiteFlushWorker.submit(activity, "sync", { profile.cookieManager.flush() }, operationId) { error ->
      onResult(if (error == null) null else "profile flush failed")
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

    /**
     * Explicit denial instead of relying on the provider default. Nothing in
     * this app has a camera, microphone or DRM flow, and a silent grant would
     * be invisible to the user. Note this also covers
     * `RESOURCE_PROTECTED_MEDIA_ID`: DRM playback was already impossible
     * without a grant, so denying changes nothing and keeps the boundary
     * explicit. The log line carries no origin and no URL.
     */
    override fun onPermissionRequest(request: PermissionRequest) {
      request.deny()
    }

    /**
     * Geolocation: deny and do not retain the answer (`allow=false`,
     * `retain=false`). Pairs with `SiteWebViewSettings.geolocationEnabled =
     * false`; both together mean a provider default change cannot start
     * granting location to a site.
     */
    override fun onGeolocationPermissionsShowPrompt(
      origin: String,
      callback: GeolocationPermissions.Callback,
    ) {
      callback.invoke(origin, false, false)
    }
  }

  /**
   * The ONLY place `android.webkit.WebSettings` is written for a site WebView:
   * a 1:1 adapter for [SiteSettingsSink], one property per decided setting.
   *
   * The values and their reasons are unit-tested through the real
   * [applySiteSettings]; this class is a straight property mapping and can only
   * be verified on a device (minSdkVersion is 26, so the API-26
   * `safeBrowsingEnabled` setter needs no version guard).
   */
  private class WebSettingsSink(private val s: WebSettings) : SiteSettingsSink {
    override fun javaScriptEnabled(value: Boolean) {
      s.javaScriptEnabled = value
    }

    override fun domStorageEnabled(value: Boolean) {
      s.domStorageEnabled = value
    }

    override fun databaseEnabled(value: Boolean) {
      s.databaseEnabled = value
    }

    override fun geolocationEnabled(value: Boolean) {
      s.setGeolocationEnabled(value)
    }

    override fun mediaPlaybackRequiresUserGesture(value: Boolean) {
      s.mediaPlaybackRequiresUserGesture = value
    }

    override fun javaScriptCanOpenWindowsAutomatically(value: Boolean) {
      s.javaScriptCanOpenWindowsAutomatically = value
    }

    override fun supportMultipleWindows(value: Boolean) {
      s.setSupportMultipleWindows(value)
    }

    override fun allowFileAccess(value: Boolean) {
      s.allowFileAccess = value
    }

    override fun allowContentAccess(value: Boolean) {
      s.allowContentAccess = value
    }

    // Both setters are deprecated from API 30 (they are no-ops once file access
    // is off, which it is). They are still written explicitly so an older
    // provider and any future re-enabling of `allowFileAccess` cannot quietly
    // reintroduce the file-URL escape hatches.
    @Suppress("DEPRECATION")
    override fun allowFileAccessFromFileURLs(value: Boolean) {
      s.allowFileAccessFromFileURLs = value
    }

    @Suppress("DEPRECATION")
    override fun allowUniversalAccessFromFileURLs(value: Boolean) {
      s.allowUniversalAccessFromFileURLs = value
    }

    override fun mixedContentMode(value: Int) {
      s.mixedContentMode = value
    }

    override fun safeBrowsingEnabled(value: Boolean) {
      s.safeBrowsingEnabled = value
    }

    override fun cacheMode(value: Int) {
      s.cacheMode = value
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
          // Page-start callbacks are high-volume; do not emit per-navigation logs.
        }
      }
      super.onPageStarted(view, url, favicon)
    }
  }

  private const val LOG_TAG = "AnimeHubSite"
}
