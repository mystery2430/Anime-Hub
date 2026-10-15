// SPDX-License-Identifier: MIT
//
// Pure sequencing/decision logic for the per-site profile flows: close
// (flush durability), clear (verified wipe), one-time cookie migration
// (strict success criteria), the active-view slot, and the launcher
// back-close handshake.
//
// WHY IT IS PURE
// --------------
// This file must NOT import `android.*` or any Tauri class: the CI unit-test
// source set compiles it with the generated project and runs
// `SiteProfileOpsTest` against it, driving every success/error/timeout path
// with fake ports. `AnimeHubSiteController` wires the real Android
// implementations (WebView/Profile/ProfileStore) into the ports. A behaviour
// that lives here is therefore actually executed in CI; a behaviour that
// stays inside the controller is compile-checked only (no device run yet).
//
// THREADING CONTRACT
// ------------------
// Each flow runs its steps on ONE thread at a time: the controller delivers
// every port callback back on the Android UI thread before it reaches a flow.
// Flows guard their completion with once-latches, so a late/duplicate
// callback can never produce a second result — or a success after a failure.

package dev.animehub.app

import java.lang.ref.WeakReference
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicInteger
import java.util.concurrent.atomic.AtomicReference

/**
 * Sentinels shared with Rust (`animehub_android`). Rust matches these
 * substrings verbatim — changing them requires changing both sides together
 * (the plugin crate asserts the literals against this source).
 */
object SiteProfileErrors {
  /** Provider cannot isolate sites — open/clear refuse, no silent fallback. */
  const val UNSUPPORTED: String = "unsupported-multi-profile"

  /**
   * The pending one-time cookie migration could not complete against a
   * virgin profile this attempt: the profile name must be retired (queued
   * for deletion + generation bump) and the migration retried later. The
   * message format is `migration-partial: <detail>` so Rust can surface the
   * underlying cause while still recognising the sentinel.
   */
  const val MIGRATION_RETRY: String = "migration-partial"
}

// ---------------------------------------------------------------------------
// Active site session slot (failed-open rollback, concurrent-view safety)
// ---------------------------------------------------------------------------

/**
 * The single active site WebView slot: view token + its profile name, kept
 * together so a partial open can never strand one of the two.
 *
 * Identity matters: rollback and stale callbacks release ONLY the token they
 * were working with (`releaseIf`), so a failure racing a newer open can never
 * clear the newer session — and a failed open always leaves the slot empty,
 * which is what keeps the next open from hitting a phantom `ERR_ACTIVE_SITE`.
 */
class SiteSessionState<T : Any> {

  private class Entry<T : Any>(val token: WeakReference<T>, val profileName: String)

  @Volatile private var entry: Entry<T>? = null

  /** The active token, or `null` when free (or garbage-collected). */
  val activeToken: T?
    get() = entry?.token?.get()

  /** Profile name paired with the active token (never set on its own). */
  val activeProfile: String?
    get() = if (entry?.token?.get() != null) entry?.profileName else null

  /** `true` when a live token occupies the slot. */
  fun hasActive(): Boolean = entry?.token?.get() != null

  /** Occupy the slot. Fails (and leaves the old value) when occupied. */
  fun acquire(token: T, profileName: String): Boolean {
    if (hasActive()) return false
    entry = Entry(WeakReference(token), profileName)
    return true
  }

  /** Clear the slot only when it still holds [token]. */
  fun releaseIf(token: T): Boolean {
    val current = entry?.token?.get() ?: return if (entry == null) false else releaseStale()
    if (current !== token) return false
    entry = null
    return true
  }

  /** Clear unconditionally (Activity teardown). Returns the released profile. */
  fun clear(): String? {
    val name = activeProfile
    entry = null
    return name
  }

  private fun releaseStale(): Boolean {
    entry = null
    return true
  }
}

// ---------------------------------------------------------------------------
// Close: normal close flushes first and keeps the view on failure
// ---------------------------------------------------------------------------

/**
 * The Android operations [SiteCloseFlow] needs. The controller implements
 * these over the active WebView/Profile; tests implement them over fakes.
 */
interface SiteClosePort {
  /** Whether a site view is currently active (idempotent-close check). */
  fun hasActiveView(): Boolean

  /**
   * Resolve the profile backing the active view via
   * `WebViewCompat.getProfile(view).name`. Errors MUST surface: a close that
   * cannot resolve its profile cannot claim cookie durability.
   */
  fun resolveProfileName(): Result<String>

  /** Blocking `Profile.getCookieManager().flush()`, off the caller thread. */
  fun flushCookies(profileName: String, onDone: (Throwable?) -> Unit)

  /** Remove + destroy the active view and re-bind PiP/focus to the launcher. */
  fun destroyActiveView()
}

/**
 * Normal close (`flush=true`): resolve profile → flush → only then destroy
 * and report success. **A flush or resolve failure keeps the view and the
 * session ownership** so the Rust `current_site` state and the Kotlin slot
 * stay identical, and the user can retry (or the back ladder falls back).
 *
 * Wipe close (`flush=false`, clear/delete paths): destroy without flushing —
 * deliberately a different branch, never a subset of the durable path.
 *
 * No active view → success (idempotent; a stale Rust session stays harmless).
 */
object SiteCloseFlow {
  fun run(port: SiteClosePort, flush: Boolean, onDone: (String?) -> Unit) {
    val once = AtomicBoolean(false)
    fun done(error: String?) {
      if (once.compareAndSet(false, true)) onDone(error)
    }
    if (!port.hasActiveView()) {
      done(null)
      return
    }
    if (!flush) {
      port.destroyActiveView()
      done(null)
      return
    }
    val profileName =
      port.resolveProfileName().getOrElse { e ->
        // Keep the view: durability is unverified, so the session stays claimed.
        done("profil çözümlenemedi: ${e.message}")
        return
      }
    port.flushCookies(profileName) { error ->
      if (error != null) {
        done("çerez kalıcılığı başarısız: ${error.message}")
        return@flushCookies
      }
      port.destroyActiveView()
      done(null)
    }
  }
}

// ---------------------------------------------------------------------------
// Clear: verified, retryable, aggregate-error wipe of ONE site profile
// ---------------------------------------------------------------------------

/** Operations [SiteClearFlow] sequences for an in-place profile wipe. */
interface SiteClearPort {
  /** `ProfileStore.getProfile(name) != null` (false = already clean). */
  fun profileExists(): Boolean

  /** `removeAllCookies { ok -> … }`; [Boolean] is the callback's result. */
  fun removeAllCookies(onDone: (Boolean) -> Unit)

  /** `WebStorage.deleteAllData()`; throw = failure. */
  fun deleteAllStorage()

  /**
   * Clear every stored geolocation permission of the profile and call
   * [onDone] with whether that completed. (`GeolocationPermissions` has no
   * removeAll-with-callback: the flow awaits `getOrigins` and clears each
   * origin; `onDone` fires only after that chain settled.)
   */
  fun clearGeolocationPermissions(onDone: (Boolean) -> Unit)

  /** Blocking `flush()`, off the caller thread; `null` = durable. */
  fun flushCookies(onDone: (Throwable?) -> Unit)

  /** One-shot delayed callback used as the watchdog for awaited steps. */
  fun scheduleTimeout(millis: Long, onTimeout: () -> Unit)
}

/**
 * In-place clear of one profile. Guarantees the caller can rely on:
 *
 *  * unsupported provider → [SiteProfileErrors.UNSUPPORTED], no port calls
 *    (never a silent success over uncleanable data);
 *  * absent profile → success (nothing to do);
 *  * every awaited callback is actually awaited (with a watchdog), and the
 *    result is reported exactly once;
 *  * failures are aggregated: every step is still attempted so one broken
 *    callback cannot mask the rest, and ANY failure (including the final
 *    `flush`) fails the whole call — Rust then keeps the encrypted session
 *    blobs and the user can retry;
 *  * every step is idempotent, so a half-finished attempt followed by a
 *    retry is safe.
 *
 * Out of scope (documented in docs/android-storage-isolation.md): HTTP cache
 * and service-worker registrations — no profile-scoped API clears those;
 * full removal happens with profile *deletion*.
 */
object SiteClearFlow {
  const val STEP_TIMEOUT_MS: Long = 10_000

  fun run(supported: Boolean, port: SiteClearPort, onDone: (String?) -> Unit) {
    val once = AtomicBoolean(false)
    fun done(error: String?) {
      if (once.compareAndSet(false, true)) onDone(error)
    }
    if (!supported) {
      done(SiteProfileErrors.UNSUPPORTED)
      return
    }
    val exists =
      runCatching { port.profileExists() }.getOrElse { e ->
        done("site verisi temizlenemedi: ${e.message}")
        return
      }
    if (!exists) {
      done(null)
      return
    }

    var firstError: String? = null
    fun record(message: String) {
      if (firstError == null) firstError = message
    }

    // Awaited step helper: settles exactly once via callback OR watchdog.
    fun awaited(stepName: String, timeoutMessage: String, arm: ((Boolean) -> Unit) -> Unit, next: (Boolean) -> Unit) {
      val settled = AtomicBoolean(false)
      port.scheduleTimeout(STEP_TIMEOUT_MS) {
        if (settled.compareAndSet(false, true)) {
          record("$stepName: $timeoutMessage")
          next(false)
        }
      }
      arm { ok ->
        if (settled.compareAndSet(false, true)) next(ok)
      }
    }

    awaited("çerez temizliği", "zaman aşımı", { cb -> port.removeAllCookies(cb) }) { cookiesOk ->
      if (!cookiesOk) record("çerezler temizlenemedi")
      awaited("geolocation izin temizliği", "zaman aşımı", { cb -> port.clearGeolocationPermissions(cb) }) { geoOk ->
        if (!geoOk) record("geolocation izinleri temizlenemedi")
        runCatching { port.deleteAllStorage() }.getOrElse { e ->
          record("site deposu temizlenemedi: ${e.message}")
        }
        // Durability last: a failed flush must never be reported as success.
        port.flushCookies { flushError ->
          if (flushError != null) record("çerez kalıcılığı başarısız: ${flushError.message}")
          done(firstError)
        }
      }
    }
  }
}

// ---------------------------------------------------------------------------
// One-time 0.3.4 cookie migration: strict success, retryable failure
// ---------------------------------------------------------------------------

/** Operations [SiteMigrationFlow] sequences into a virgin profile. */
interface SiteMigrationPort {
  /** Whether the target profile already exists (live/partial — never touch). */
  fun profileExists(): Boolean

  /** `getOrCreateProfile(name)`; throw = failure. */
  fun createProfile()

  /** One `setCookie(url, cookie) { accepted -> … }` write. */
  fun setCookie(cookie: String, onAccepted: (Boolean) -> Unit)

  /** Blocking `flush()`; `null` = durable. */
  fun flushCookies(onDone: (Throwable?) -> Unit)

  /** One-shot watchdog for the awaited steps. */
  fun scheduleTimeout(millis: Long, onTimeout: () -> Unit)
}

/** Result of the migration attempt, with the strict success criterion. */
sealed class MigrationOutcome {
  /** Every cookie accepted AND flush durable → Rust may write the flag. */
  data class Complete(val accepted: Int) : MigrationOutcome()

  /** The profile already existed: its state is authoritative — never write. */
  object ProfileAlreadyExists : MigrationOutcome()

  /**
   * Something failed midway (profile created, cookie rejected/thrown, flush
   * failed, callback lost): the profile is dirty and must be retired.
   * [SiteProfileErrors.MIGRATION_RETRY] prepended by the controller.
   */
  data class Failed(val reason: String) : MigrationOutcome()
}

/**
 * One-time import of the legacy encrypted cookie jar into a *virgin* profile.
 *
 * Success criterion (testable, no interpretation): the profile did not
 * exist, every `setCookie` callback returned accepted, and the flush
 * completed — only then may Rust write `profilemig-<id>` and treat the jar
 * as consumed. Anything else is [MigrationOutcome.Failed] (or
 * [MigrationOutcome.ProfileAlreadyExists]) and the caller maps both to the
 * `migration-partial` sentinel so Rust retires the profile name and retries
 * on a fresh generation — a half-imported profile can never be mistaken for
 * a complete one on the next open, and the legacy snapshot survives until a
 * real success.
 */
object SiteMigrationFlow {
  const val COOKIE_TIMEOUT_MS: Long = 10_000
  const val FLUSH_TIMEOUT_MS: Long = 15_000

  fun run(cookies: List<String>, port: SiteMigrationPort, onDone: (MigrationOutcome) -> Unit) {
    val once = AtomicBoolean(false)
    fun finish(outcome: MigrationOutcome) {
      if (once.compareAndSet(false, true)) onDone(outcome)
    }
    val exists =
      runCatching { port.profileExists() }.getOrElse { e ->
        finish(MigrationOutcome.Failed("mevcut profili okunamadı: ${e.message}"))
        return
      }
    if (exists) {
      finish(MigrationOutcome.ProfileAlreadyExists)
      return
    }
    runCatching { port.createProfile() }.getOrElse { e ->
      finish(MigrationOutcome.Failed("profil oluşturulamadı: ${e.message}"))
      return
    }

    val accepted = AtomicInteger(0)
    val index = AtomicInteger(0)

    fun setNext() {
      val i = index.get()
      if (i >= cookies.size) {
        val settled = AtomicBoolean(false)
        port.scheduleTimeout(FLUSH_TIMEOUT_MS) {
          if (settled.compareAndSet(false, true)) {
            finish(MigrationOutcome.Failed("çerez aktarımı kalıcı hale getirilemedi: zaman aşımı"))
          }
        }
        port.flushCookies { error ->
          if (!settled.compareAndSet(false, true)) return@flushCookies
          if (error != null) {
            finish(MigrationOutcome.Failed("çerez aktarımı kalıcı hale getirilemedi: ${error.message}"))
          } else {
            finish(MigrationOutcome.Complete(accepted.get()))
          }
        }
        return
      }
      val perCookie = AtomicBoolean(false)
      port.scheduleTimeout(COOKIE_TIMEOUT_MS) {
        if (perCookie.compareAndSet(false, true)) {
          finish(MigrationOutcome.Failed("çerez aktarımı zaman aşımına uğradı"))
        }
      }
      try {
        port.setCookie(cookies[i]) { ok ->
          if (!perCookie.compareAndSet(false, true)) return@setCookie
          if (!ok) {
            // Strict criterion: a rejected cookie fails the migration and
            // stops the chain — the doomed profile gets retired, and writing
            // more of the jar into it would only muddy the retry.
            finish(MigrationOutcome.Failed("çerez reddedildi (${i + 1}/${cookies.size})"))
            return@setCookie
          }
          accepted.incrementAndGet()
          index.incrementAndGet()
          setNext()
        }
      } catch (e: Exception) {
        if (perCookie.compareAndSet(false, true)) {
          finish(MigrationOutcome.Failed("çerez aktarımı başarısız: ${e.message}"))
        }
      }
    }
    setNext()
  }
}

// ---------------------------------------------------------------------------
// Launcher back-close handshake (Kotlin side of window.__animehubBack*)
// ---------------------------------------------------------------------------

/** What the launcher's `__animehubBackStatus()` answer means for Kotlin. */
enum class BackCloseDecision {
  /** A close is in flight — poll again later. */
  POLL,

  /** Rust completed the close (view already destroyed by the chain). */
  CLOSED,

  /** The launcher reported a failed close — surface it, keep everything. */
  FAILED,

  /** No usable answer — fall back to the native close path. */
  FALLBACK,

  /** Still pending after the poll budget — leave the in-flight close alone. */
  GIVE_UP,
}

/**
 * Decisions for polling `window.__animehubBackStatus()` after the back hook
 * was accepted. `null`/`idle`/unknown answers fall back to a native close
 * (the launcher JS may be on an older contract); `pending` polls up to
 * [maxPolls] then gives up without touching state (a flush may simply be
 * slow, and a second close must not race the first).
 */
object SiteBackStatus {
  const val PENDING: String = "pending"
  const val CLOSED: String = "closed"
  const val IDLE: String = "idle"
  const val FAILED_PREFIX: String = "failed:"

  fun decide(status: String?, attempt: Int, maxPolls: Int): BackCloseDecision =
    when {
      status == null || status == IDLE -> BackCloseDecision.FALLBACK
      status == CLOSED -> BackCloseDecision.CLOSED
      status == PENDING ->
        if (attempt < maxPolls) BackCloseDecision.POLL else BackCloseDecision.GIVE_UP
      status.startsWith(FAILED_PREFIX) -> BackCloseDecision.FAILED
      else -> BackCloseDecision.FALLBACK
    }

  /** The message inside a `failed:<msg>` status (empty when absent). */
  fun failureMessage(status: String?): String =
    status?.removePrefix(FAILED_PREFIX)?.takeIf { status.startsWith(FAILED_PREFIX) } ?: ""
}
