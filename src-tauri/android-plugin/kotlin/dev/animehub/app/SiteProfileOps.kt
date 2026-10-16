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
//
// A once-latch bounds the RESULT only, not the DATA. A step whose mutation
// the port performs itself after an await (the geolocation wipe) therefore
// also runs behind a [StepGuard] that the flow's watchdog closes: a callback
// which lost the race provably performs no `clear` at all. Steps whose
// mutation is the provider's own and cannot be cancelled (a native
// `removeAllCookies` already in flight when the watchdog fired) are handled
// by [ProfileQuarantine] instead — the profile is refused for a new session
// until that wipe is verified, because ignoring the late result would still
// leave the late mutation to land on the new session's data.

package dev.animehub.app

import java.lang.ref.WeakReference
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicInteger
import java.util.concurrent.atomic.AtomicLong
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

  /**
   * A previous in-place wipe of this profile timed out on a destructive
   * provider call that cannot be cancelled, so its completion is UNVERIFIED:
   * the operation may still mutate the profile after the flow gave up. Kotlin
   * refuses to open the profile for a new session until the wipe is verified
   * (see [ProfileQuarantine]). Message format `profile-quarantined: <reason>`;
   * Rust maps the sentinel onto user-facing guidance instead of showing it raw.
   */
  const val QUARANTINED: String = "profile-quarantined"
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

  /**
   * One-shot delayed callback used as the watchdog for the flush await —
   * the same bound [SiteClearFlow] and [SiteMigrationFlow] put on their
   * awaited steps. A flush that never calls back must not hang the close.
   */
  fun scheduleTimeout(millis: Long, onTimeout: () -> Unit)

  /** Remove + destroy the active view and re-bind PiP/focus to the launcher. */
  fun destroyActiveView()
}

/**
 * Normal close (`flush=true`): resolve profile → flush → only then destroy
 * and report success. **A flush or resolve failure keeps the view and the
 * session ownership** so the Rust `current_site` state and the Kotlin slot
 * stay identical, and the user can retry (or the back ladder falls back).
 *
 * The flush await is bounded by [FLUSH_TIMEOUT_MS]: the callback and the
 * watchdog race, exactly one of them wins, and a timeout answers the caller
 * with a failure while leaving everything untouched — no destroy, no
 * session release. A flush callback arriving AFTER the timeout is ignored:
 * it can neither flip the delivered failure back to success nor destroy the
 * view underneath the still-claimed session (the view may by then belong to
 * a retried close or to a newer open).
 *
 * Wipe close (`flush=false`, clear/delete paths): destroy without flushing —
 * deliberately a different branch, never a subset of the durable path.
 *
 * No active view → success (idempotent; a stale Rust session stays harmless).
 */
object SiteCloseFlow {
  /**
   * Bound for the flush await. Same magnitude as the migration flow's flush
   * watchdog: `CookieManager.flush()` is the one blocking provider call
   * behind a single-threaded executor, so every flow that waits on it gets
   * the same ceiling.
   */
  const val FLUSH_TIMEOUT_MS: Long = 15_000

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
    // Watchdog + callback race, settled exactly once. Timeout = failure with
    // the view AND the session ownership untouched (retryable); a late flush
    // callback loses the race and is ignored — it must not destroy a view
    // whose ownership was never released.
    val settled = AtomicBoolean(false)
    port.scheduleTimeout(FLUSH_TIMEOUT_MS) {
      if (settled.compareAndSet(false, true)) {
        done("çerez kalıcılığı zaman aşımına uğradı: görünüm açık kaldı")
      }
    }
    port.flushCookies(profileName) { error ->
      if (!settled.compareAndSet(false, true)) return@flushCookies
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
// Awaited steps: validity token, gated geolocation wipe, quarantine
// ---------------------------------------------------------------------------

/**
 * Validity token for ONE awaited step: the cancellation flag a port must
 * consult before it mutates profile data.
 *
 * WHY A RESULT LATCH IS NOT ENOUGH: a flow's `AtomicBoolean` decides who
 * *reports* — it cannot un-run a side effect. The awaited geolocation wipe
 * enumerates origins asynchronously and performs the `clear` calls itself, so
 * a callback arriving after the watchdog fired used to wipe permissions
 * belonging to whatever session the profile backs by then. [claim] turns that
 * into a single winner for the DATA as well as for the result.
 */
interface StepGuard {
  /**
   * Correlation id for the in-app debug log ("operation id"): a device log
   * line about a stale callback can be tied to the step that owned it.
   */
  val operationId: Long

  /**
   * Atomically win this step. Returns `true` for EXACTLY ONE caller — the
   * flow's watchdog or the port's callback — and `false` for every later one.
   *
   * Contract for ports that mutate: claim FIRST, mutate only on `true`, and
   * report the result in the same serialised block. A caller that got `false`
   * must return without touching provider state and without reporting — the
   * winner's outcome already stands.
   */
  fun claim(): Boolean

  /** Diagnostics only; never a check-then-act substitute for [claim]. */
  val open: Boolean
}

/** Creates [StepGuard]s with process-unique, monotonic [StepGuard.operationId]s. */
object StepGuards {
  private val counter = AtomicLong(0)

  fun create(): StepGuard = Guard(counter.incrementAndGet())

  private class Guard(override val operationId: Long) : StepGuard {
    private val claimed = AtomicBoolean(false)

    override fun claim(): Boolean = claimed.compareAndSet(false, true)

    override val open: Boolean
      get() = !claimed.get()
  }
}

/**
 * The two provider calls the geolocation wipe needs. The real implementation
 * is `profile.geolocationPermissions` (`android.webkit.GeolocationPermissions`);
 * tests implement it over a recording fake, so the side effect the production
 * adapter triggers is measurable in CI.
 */
interface GeolocationApi {
  /** `getOrigins { … }` — asynchronous and read-only; may answer late or never. */
  fun getOrigins(onResult: (Set<String>?) -> Unit)

  /** `clear(origin)` — THE mutation this step exists to gate. */
  fun clear(origin: String)
}

/**
 * Production sequencing of the awaited geolocation wipe, extracted from the
 * controller adapter so CI executes the ordering it must guarantee:
 * await `getOrigins` → **[StepGuard.claim]** → only then `clear` each origin →
 * report.
 *
 * `GeolocationPermissions` has no removeAll-with-callback, which is why this
 * step awaits at all — and awaiting is exactly what makes the mutation
 * cancellable by us: unlike a native `removeAllCookies`, the `clear` calls
 * have not happened yet when the watchdog fires, so refusing them is a real
 * cancellation rather than a shrug. A callback that lost the claim performs
 * ZERO `clear` calls and reports nothing, which keeps a timed-out wipe from
 * revoking permissions a newer session on the same profile just granted.
 *
 * Threading: the controller delivers [GeolocationApi.getOrigins]' callback on
 * the UI thread and posts the watchdog to the UI thread as well, so claim and
 * mutate are one serialised block; the CAS keeps the guarantee even if a port
 * ever answered from another thread.
 *
 * The claim winner always reports (the `clear` loop is wrapped), so a won
 * step can never strand the flow; if `getOrigins` itself throws, the step is
 * claimed and reported as a failure immediately.
 */
object GeolocationWipe {
  fun start(guard: StepGuard, api: GeolocationApi, onDone: (Boolean) -> Unit) {
    if (!guard.open) return // Already settled before we even armed: nothing to do.
    val started =
      try {
        api.getOrigins { origins ->
          if (!guard.claim()) return@getOrigins // Watchdog won: no side effect, no result.
          onDone(clearOrigins(api, origins))
        }
        true
      } catch (_: Throwable) {
        false
      }
    if (!started && guard.claim()) onDone(false)
  }

  private fun clearOrigins(api: GeolocationApi, origins: Set<String>?): Boolean =
    try {
      if (origins != null) {
        for (origin in origins) api.clear(origin)
      }
      true
    } catch (_: Throwable) {
      false
    }
}

/**
 * Process-scoped registry of profiles whose in-place wipe timed out on a
 * destructive provider call that could NOT be cancelled or gated, so its
 * completion is unverified and it may still mutate the profile later.
 *
 * WHY A NEW SESSION MUST NOT OPEN ON SUCH A PROFILE (the proof this object
 * exists for): the watchdog answers the CALLER — it never stops the provider
 * operation underneath. `CookieManager.removeAllCookies` offers no
 * cancellation, so after a timeout the deletion may still be in flight and
 * can complete *after* the site was reopened, silently removing the new
 * session's cookies (a fresh login, or the one-time jar import). Ignoring the
 * late callback result does not help: the data change happens regardless.
 * Refusing the open until the operation is known to have finished is the only
 * safe resolution, and it is recoverable — the user retries the wipe (which
 * verifies it) or removes the site (which retires the name).
 *
 * WHAT IS AND IS NOT QUARANTINED:
 *  * `removeAllCookies` timeout → QUARANTINED (ungated destructive, unverified).
 *  * geolocation timeout → NOT quarantined: the mutation is ours and gated by
 *    [StepGuard]/[GeolocationWipe], so a late callback provably clears
 *    nothing; `getOrigins` is read-only.
 *  * `WebStorage.deleteAllData()` → synchronous: it returned or threw, so the
 *    outcome is verified by construction.
 *  * final `flush()` timeout → NOT quarantined: `flush()` only persists the
 *    cookie store that already exists, so a late flush cannot delete or grant
 *    anything a new session created (worst case it durably writes that
 *    session's own cookies, which its own close flush would do anyway). It
 *    still fails the wipe, so Rust keeps the encrypted blobs for a retry.
 *
 * WHY IN-MEMORY IS THE RIGHT SCOPE: the hazard is a late callback in THIS
 * process. Those callbacks belong to the app's browser process — a restart
 * destroys them, and the registry starts empty by construction. Persisting
 * the quarantine would lock the user out of a site across restarts for a
 * hazard that no longer exists. (The migration flow faces the same late-write
 * problem and answers it differently, by RETIRING the name — see
 * [SiteMigrationFlow]: a virgin import profile is disposable, a profile the
 * user clears in place is not.)
 *
 * LIFTED BY: a fully verified clear of the same profile, the native callback
 * finally answering (the operation is then no longer in flight — [SiteClearFlow]
 * reports that as a late answer), a confirmed profile deletion (the name is
 * retired and its generation bumped, so it is never reopened), or a restart.
 */
object ProfileQuarantine {
  private val reasons = ConcurrentHashMap<String, String>()

  /** Block [profileName] from backing a new session, recording why. */
  fun quarantine(profileName: String, reason: String) {
    reasons[profileName] = reason
  }

  /** The recorded reason, or `null` when the profile may be used. */
  fun reasonFor(profileName: String): String? = reasons[profileName]

  fun isQuarantined(profileName: String): Boolean = reasons.containsKey(profileName)

  /** Lifts the block; returns the reason that was recorded, if any. */
  fun release(profileName: String): String? = reasons.remove(profileName)

  /** Every blocked name (diagnostics/tests). */
  fun names(): Set<String> = reasons.keys.toSet()

  /** Forget every entry — test isolation and Activity teardown. */
  fun reset() {
    reasons.clear()
  }
}

/**
 * The open-time half of the quarantine: BOTH phases of the two-phase open
 * consult it before they touch the provider, so a profile with an unverified
 * wipe in flight is never bound to a WebView and never receives the one-time
 * jar import.
 */
object SiteOpenGate {
  /**
   * `null` when [profileName] may be opened; otherwise the refusal carrying
   * [SiteProfileErrors.QUARANTINED] plus the recorded reason, which Rust
   * classifies into user-facing guidance.
   */
  fun check(profileName: String): String? =
    ProfileQuarantine.reasonFor(profileName)?.let { reason ->
      "${SiteProfileErrors.QUARANTINED}: $reason"
    }
}

// ---------------------------------------------------------------------------
// Clear: verified, retryable, aggregate-error wipe of ONE site profile
// ---------------------------------------------------------------------------

/** Operations [SiteClearFlow] sequences for an in-place profile wipe. */
interface SiteClearPort {
  /** `ProfileStore.getProfile(name) != null` (false = already clean). */
  fun profileExists(): Boolean

  /**
   * `removeAllCookies { ok -> … }`; [Boolean] is the callback's result.
   *
   * NO [StepGuard] here on purpose: this mutation is the provider's own, is
   * issued before the await and offers no cancellation, so there is nothing a
   * port could gate. A timeout therefore leaves the operation possibly in
   * flight, which [SiteClearFlow] answers with [ProfileQuarantine] rather
   * than by pretending the late callback was the whole problem. The port MUST
   * still deliver [onDone] whenever the provider answers — late or not — so
   * the flow can learn that the operation finished and lift the block.
   */
  fun removeAllCookies(onDone: (Boolean) -> Unit)

  /** `WebStorage.deleteAllData()`; throw = failure. */
  fun deleteAllStorage()

  /**
   * Clear every stored geolocation permission of the profile and call
   * [onDone] with whether that completed. (`GeolocationPermissions` has no
   * removeAll-with-callback: the step awaits `getOrigins` and clears each
   * origin; [onDone] fires only after that chain settled.)
   *
   * [guard] MUST be claimed before the first `clear` — delegate to
   * [GeolocationWipe], which is the production sequencing: a callback that
   * lost to the watchdog then performs no side effect on a profile that may
   * already back a new session, and reports nothing.
   */
  fun clearGeolocationPermissions(guard: StepGuard, onDone: (Boolean) -> Unit)

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
 *  * every awaited callback is actually awaited (with a watchdog —
 *    [STEP_TIMEOUT_MS] per step, [FLUSH_TIMEOUT_MS] for the final durability
 *    flush), and the result is reported exactly once;
 *  * a step that timed out also loses its SIDE EFFECTS: the geolocation wipe
 *    runs behind a [StepGuard] the watchdog closes, so its late callback
 *    performs no `clear` on a profile that may by then back a new session,
 *    and the one destructive call that cannot be gated (`removeAllCookies`)
 *    puts the profile in [ProfileQuarantine] until it is verified — the flow
 *    never claims a wipe it could not watch finish;
 *  * failures are aggregated: every step is still attempted so one broken
 *    callback cannot mask the rest, and ANY failure (including the final
 *    `flush`) fails the whole call — Rust then keeps the encrypted session
 *    blobs and the user can retry;
 *  * every step is idempotent, so a half-finished attempt followed by a
 *    retry is safe, and a fully verified success lifts any quarantine on
 *    [profileName] so the site can be opened again.
 *
 * Out of scope (documented in docs/android-storage-isolation.md): HTTP cache
 * and service-worker registrations — no profile-scoped API clears those;
 * full removal happens with profile *deletion*.
 */
object SiteClearFlow {
  const val STEP_TIMEOUT_MS: Long = 10_000

  /** Bound for the final durability flush (see [SiteCloseFlow]). */
  const val FLUSH_TIMEOUT_MS: Long = 15_000

  /** Reason recorded when the cookie wipe's completion cannot be verified. */
  const val COOKIE_UNVERIFIED_REASON: String =
    "çerez temizliği zaman aşımına uğradı, işlem doğrulanamadı"

  fun run(
    supported: Boolean,
    profileName: String,
    port: SiteClearPort,
    onDone: (String?) -> Unit,
  ) {
    val once = AtomicBoolean(false)
    fun done(error: String?) {
      if (once.compareAndSet(false, true)) {
        if (error == null) {
          // Fully verified wipe: every destructive step answered, nothing is
          // in flight, so the profile may back a new session again.
          ProfileQuarantine.release(profileName)
        }
        onDone(error)
      }
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

    /**
     * Awaited step helper: the callback and the watchdog race and EXACTLY ONE
     * of them settles the result ([next] runs at most once).
     *
     * The watchdog closes [StepGuard] first, so a port that mutates behind the
     * guard (the geolocation wipe) provably performs no side effect after this
     * point — that is the difference between "the late result was ignored" and
     * "the late mutation was prevented". [onTimeout] then records what the
     * timeout leaves unverified, and [onLateAnswer] runs when the provider
     * answers anyway after losing the race: the result stands, but the flow
     * learns the operation finished.
     *
     * THE UI-THREAD RACE, explicitly: the controller posts the watchdog with
     * `mainHandler.postDelayed` and delivers every provider callback with
     * `runOnUiThread`, so both sides run on the UI thread and cannot
     * interleave — claim, mutate and report happen inside one serialised task.
     * Should a port ever answer from another thread, the two CAS operations
     * still leave one winner for the data and one for the result; the only
     * mixed outcome is a port that claimed the guard but reported *after* the
     * watchdog settled, which is answered with the watchdog's failure. That is
     * the conservative direction: the wipe is reported incomplete, Rust keeps
     * the encrypted blobs and the user retries — every step is idempotent, so
     * a retry over an already-applied mutation is harmless.
     */
    fun awaited(
      stepName: String,
      timeoutMessage: String,
      onTimeout: () -> Unit = {},
      onLateAnswer: () -> Unit = {},
      arm: (StepGuard, (Boolean) -> Unit) -> Unit,
      next: (Boolean) -> Unit,
    ) {
      val settled = AtomicBoolean(false)
      val guard = StepGuards.create()
      port.scheduleTimeout(STEP_TIMEOUT_MS) {
        // Close the mutation window before deciding the result: a callback
        // racing this watchdog on another thread must see the guard shut even
        // if it arrives between the two atomic steps.
        guard.claim()
        if (settled.compareAndSet(false, true)) {
          record("$stepName: $timeoutMessage")
          onTimeout()
          next(false)
        }
      }
      arm(guard) { ok ->
        if (settled.compareAndSet(false, true)) next(ok) else onLateAnswer()
      }
    }

    awaited(
      "çerez temizliği",
      "zaman aşımı",
      onTimeout = {
        // Ungated AND unverified: `removeAllCookies` was issued before the
        // await, offers no cancellation, and may still delete cookies after
        // this flow reported failure — including cookies a reopened session
        // just created. Ignoring the late callback result would not stop
        // that, so the profile is blocked until the wipe is verified.
        ProfileQuarantine.quarantine(profileName, COOKIE_UNVERIFIED_REASON)
      },
      onLateAnswer = {
        // The provider answered after losing the race: the operation is
        // finished, nothing is in flight any more, so the block is lifted.
        // The delivered failure stands — a late answer never re-reports.
        ProfileQuarantine.release(profileName)
      },
      arm = { _, cb -> port.removeAllCookies(cb) },
    ) { cookiesOk ->
      if (!cookiesOk) record("çerezler temizlenemedi")
      // No quarantine here: this step's mutation is ours and gated by the
      // [StepGuard] the watchdog closes, so a late callback provably clears
      // nothing (GeolocationWipe) and `getOrigins` is read-only.
      awaited(
        "geolocation izin temizliği",
        "zaman aşımı",
        arm = { guard, cb -> port.clearGeolocationPermissions(guard, cb) },
      ) { geoOk ->
        if (!geoOk) record("geolocation izinleri temizlenemedi")
        runCatching { port.deleteAllStorage() }.getOrElse { e ->
          record("site deposu temizlenemedi: ${e.message}")
        }
        // Durability last: a failed flush must never be reported as success —
        // and a flush whose callback never arrives must never hang the wipe
        // either. Watchdog and callback race, settled exactly once; a timeout
        // records itself as the aggregate failure, so Rust sees an error,
        // keeps the encrypted session blobs for a retry and never reports a
        // half-finished clear as success. A late flush callback cannot flip
        // that delivered failure back to success. A flush timeout also
        // quarantines nothing: `flush()` only persists the cookie store that
        // already exists, so a late one cannot delete or grant anything a new
        // session created (see [ProfileQuarantine] for the full argument).
        val flushSettled = AtomicBoolean(false)
        port.scheduleTimeout(FLUSH_TIMEOUT_MS) {
          if (flushSettled.compareAndSet(false, true)) {
            record("çerez kalıcılığı: zaman aşımı")
            done(firstError)
          }
        }
        port.flushCookies { flushError ->
          if (!flushSettled.compareAndSet(false, true)) return@flushCookies
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
