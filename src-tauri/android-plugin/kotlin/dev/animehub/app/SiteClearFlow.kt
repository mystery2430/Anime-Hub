// SPDX-License-Identifier: MIT
//
// Clear flow: the verified, retryable, aggregate-error in-place wipe of ONE site
// profile, including the typed reading of a cookie wipe delivery.
//
// Pure decision/sequencing code: this file must NOT import `android.*` or any
// Tauri class, so the CI unit-test source set compiles and runs it against the
// generated Android project (see SiteProfileOps.kt for the full rationale and
// the one-thread-per-flow rule). Split out of SiteProfileOps.kt so each
// responsibility — and what may change it — is visible at a glance.

package dev.animehub.app

import java.util.concurrent.atomic.AtomicBoolean

// ---------------------------------------------------------------------------
// Clear: verified, retryable, aggregate-error wipe of ONE site profile
// ---------------------------------------------------------------------------

/**
 * What one `removeAllCookies` delivery actually says.
 *
 * WHY NOT A [Boolean]: Chromium's contract for `removeAllCookies(callback)` is
 * that the callback receives whether **any cookies were deleted** — an outcome
 * report, not an error flag. An already-empty jar therefore answers `false`
 * for a wipe that fully succeeded, while a provider that could not be reached
 * at all produced `false` too in the old shape. Collapsing the two made "clear
 * site data" on a clean profile fail the whole wipe: Rust then kept the
 * encrypted session blobs and never reached `delete_site_session`, so a site
 * the user had already cleaned could not have its stored jar dropped.
 *
 * The real cases are now distinguishable by TYPE, and exactly one place
 * interprets them ([SiteClearFlow.interpretCookieWipe]):
 *  * [Answered] — the provider ran and reported; [Answered.deleted] is its raw
 *    boolean, `false` included (which may simply mean "there was nothing");
 *  * [Failed] — the call never reached the cookie store (no profile access at
 *    this step, or the provider threw). That is an ERROR and is never read as
 *    "nothing to delete", so a genuine failure cannot become a success.
 */
sealed interface CookieWipe {
  /** The provider answered; [deleted] is its raw "were any cookies removed". */
  data class Answered(val deleted: Boolean) : CookieWipe

  /** The call itself failed; [reason] is aggregated into the reported error. */
  data class Failed(val reason: String) : CookieWipe
}

/** One cookie step's verdict: [ok], plus the reason to aggregate when not. */
data class CookieStep(val ok: Boolean, val error: String?)

/**
 * The adapter-side mapping between what the provider hands back and
 * [CookieWipe] — pure, so the CI unit tests can pin it.
 *
 * WHY THIS IS NOT LEFT INLINE IN THE ADAPTER: the real adapter needs a live
 * `Activity`, a `ProfileStore` profile and the provider's own `CookieManager`,
 * and no unit test can build those without new test dependencies. Everything
 * decidable without them lives here instead — in particular the two rules
 * whose inversion would silently manufacture a FALSE SUCCESS:
 *
 *  * `ValueCallback<Boolean>` may deliver `null`. Null is NOT "cookies were
 *    deleted", so it maps to `Answered(false)` and lets the emptiness probe
 *    decide, instead of being read as a confirmed deletion;
 *  * `CookieManager.hasCookies()` answers the OPPOSITE question the flow asks
 *    ("are there cookies?" vs "is the jar empty?"). The negation therefore
 *    lives here, in tested code, and a throwing probe stays `null` =
 *    indeterminate — which [SiteClearFlow.interpretCookieWipe] reports as a
 *    retryable failure rather than laundering into "empty, so success".
 */
object CookieWipes {
  /** Reason used when the profile is gone by the time the cookie step runs. */
  const val NO_PROFILE_REASON: String = "profil bu adımda erişilebilir değil"

  /** The provider's raw `ValueCallback<Boolean>` delivery, mapped null-safely. */
  fun answered(deleted: Boolean?): CookieWipe = CookieWipe.Answered(deleted == true)

  /** A call that threw never reached the cookie store: a typed failure. */
  fun failed(error: Throwable): CookieWipe =
    CookieWipe.Failed("${error.javaClass.simpleName}: ${error.message}")

  /** No profile access at this step — an error, never "nothing to delete". */
  fun noProfile(): CookieWipe = CookieWipe.Failed(NO_PROFILE_REASON)

  /**
   * Turn the provider's `hasCookies()` into the flow's "is the jar empty?":
   * `true` empty, `false` cookies remain, `null` when the probe itself could
   * not answer (no profile, a throw) — indeterminate, never "empty".
   */
  fun emptinessOf(hasCookies: () -> Boolean): Boolean? =
    runCatching { !hasCookies() }.getOrNull()
}

/** Operations [SiteClearFlow] sequences for an in-place profile wipe. */
interface SiteClearPort {
  /** `ProfileStore.getProfile(name) != null` (false = already clean). */
  fun profileExists(): Boolean

  /**
   * `removeAllCookies { … }`, delivered as a typed [CookieWipe].
   *
   * NO [StepGuard] here on purpose: this mutation is the provider's own, is
   * issued before the await and offers no cancellation, so there is nothing a
   * port could gate. A timeout therefore leaves the operation possibly in
   * flight, which [SiteClearFlow] answers with [ProfileQuarantine] rather
   * than by pretending the late callback was the whole problem. The port MUST
   * still deliver [onDone] whenever the provider answers — late or not — so
   * the flow can close THAT operation's record: it is the only evidence that
   * the deletion is no longer in flight, and no other attempt may substitute
   * for it.
   *
   * The provider's boolean is passed through as [CookieWipe.Answered] —
   * `false` included, which per Chromium means "no cookies were deleted" (an
   * empty jar answers that way for a fully successful wipe) and NOT "the call
   * failed". Anything that stopped the call from reaching the store (no
   * profile access at this step, a thrown exception) MUST arrive as
   * [CookieWipe.Failed] instead: the flow can only tell a clean profile from a
   * broken provider by that type, and it never turns [CookieWipe.Failed] into
   * a success.
   */
  fun removeAllCookies(onDone: (CookieWipe) -> Unit)

  /**
   * Whether the profile's cookie jar is provably empty: `true` empty, `false`
   * cookies still present, `null` when the answer cannot be obtained at all
   * (no profile access, the provider threw).
   *
   * Consulted only after a [CookieWipe.Answered] with `deleted == false` —
   * i.e. when nothing was just deleted — so it reads a stable store instead of
   * racing the wipe it is checking. Synchronous on purpose: an indeterminate
   * answer must not be awaited into a guess, and `null` is a retryable failure
   * in the flow, never a silent success.
   */
  fun cookiesCleared(): Boolean?

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
  fun scheduleTimeout(millis: Long, onTimeout: () -> Unit): () -> Unit
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
 *    is registered in [ProfileQuarantine] **under its own operation id** —
 *    the flow never claims a wipe it could not watch finish;
 *  * the cookie step is judged from a TYPED delivery ([CookieWipe]) because
 *    the provider's boolean only says whether anything was deleted: an
 *    already-empty jar answers `false` for a fully successful wipe (see
 *    [interpretCookieWipe]), while a call that never reached the store arrives
 *    as [CookieWipe.Failed] and stays a failure;
 *  * failures are aggregated: every step is still attempted so one broken
 *    callback cannot mask the rest, and ANY failure (including the final
 *    `flush`) fails the whole call — Rust then keeps the encrypted session
 *    blobs and the user can retry;
 *  * every step is idempotent, so a half-finished attempt followed by a
 *    retry is safe. A fully verified success records [ProfileQuarantine.verified]
 *    for [profileName] — it does NOT erase an earlier attempt's still
 *    outstanding operation, so the site reopens only once every destructive
 *    call this process issued for that profile has settled AND a wipe was
 *    verified after the last of them settled.
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

  /** The provider deleted nothing and the profile still holds cookies. */
  const val COOKIE_STILL_PRESENT_REASON: String =
    "sağlayıcı hiçbir çerez silmediğini bildirdi ve profilde hâlâ çerez var"

  /** The provider deleted nothing and emptiness could not be established. */
  const val COOKIE_STATE_UNKNOWN_REASON: String =
    "sağlayıcı hiçbir çerez silmediğini bildirdi, çerez deposunun boş olduğu doğrulanamadı"

  /** Prefix for a cookie call that never reached the store. */
  const val COOKIE_ACCESS_FAILED_PREFIX: String = "çerez deposuna erişilemedi"

  /**
   * Decide one cookie step from its TYPED delivery — pure, CI-tested, and the
   * only place the provider's boolean is interpreted:
   *
   *  * [CookieWipe.Answered] with `deleted == true` → success: the store
   *    reported removals;
   *  * `deleted == false` → INDETERMINATE, not a failure. Ask the store
   *    itself: provably empty ([SiteClearPort.cookiesCleared] == `true`) means
   *    there was nothing to delete, so the wipe succeeded — the
   *    already-clean-profile case that used to fail the whole clear and leave
   *    the encrypted session jar behind. Cookies still present is a real
   *    failure, and so is a store whose emptiness cannot be read: the
   *    dangerous direction is claiming a wipe we cannot see, never reporting
   *    one more failure the user can retry;
   *  * [CookieWipe.Failed] → failure carrying the provider's reason. Never a
   *    success: an unreachable store deleted nothing, and this is the case the
   *    old boolean shape silently confused with an empty jar.
   *
   * The emptiness probe runs ONLY in the `deleted == false` branch, so it
   * never races the deletion it would otherwise be checking: nothing was just
   * removed and the store state is stable.
   */
  fun interpretCookieWipe(wipe: CookieWipe, cookiesCleared: () -> Boolean?): CookieStep =
    when (wipe) {
      is CookieWipe.Answered ->
        if (wipe.deleted) {
          CookieStep(true, null)
        } else {
          when (cookiesCleared()) {
            true -> CookieStep(true, null)
            false -> CookieStep(false, COOKIE_STILL_PRESENT_REASON)
            null -> CookieStep(false, COOKIE_STATE_UNKNOWN_REASON)
          }
        }
      is CookieWipe.Failed -> CookieStep(false, "$COOKIE_ACCESS_FAILED_PREFIX: ${wipe.reason}")
    }

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
          // Fully verified wipe: every step of THIS attempt answered. That is
          // recorded as a verification, not as a release — an earlier attempt's
          // timed-out `removeAllCookies` may still be outstanding, and its
          // record belongs to that operation alone (see ProfileQuarantine).
          ProfileQuarantine.verified(profileName)
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
      onTimeout: (StepGuard) -> Unit = {},
      onLateAnswer: (StepGuard) -> Unit = {},
      arm: (StepGuard, (Boolean) -> Unit) -> Unit,
      next: (Boolean) -> Unit,
    ) {
      val settled = AtomicBoolean(false)
      val guard = StepGuards.create()
      var cancelWatchdog: () -> Unit = {}
      cancelWatchdog = port.scheduleTimeout(STEP_TIMEOUT_MS) {
        // Close the mutation window before deciding the result: a callback
        // racing this watchdog on another thread must see the guard shut even
        // if it arrives between the two atomic steps.
        guard.claim()
        if (settled.compareAndSet(false, true)) {
          record("$stepName: $timeoutMessage")
          // The hook receives THIS step's guard, so anything it registers is
          // owned by this operation id and nothing else's.
          onTimeout(guard)
          next(false)
        }
      }
      arm(guard) { ok ->
        if (settled.compareAndSet(false, true)) {
          // This step is settled: cancel its watchdog instead of leaving a
          // delayed runnable queued — it would keep this port, and through it
          // the WebView and the Activity, reachable for the rest of the window.
          cancelWatchdog()
          next(ok)
        } else {
          onLateAnswer(guard)
        }
      }
    }

    // Reason from the typed cookie delivery, for the aggregate error below.
    var cookieError: String? = null
    awaited(
      "çerez temizliği",
      "zaman aşımı",
      onTimeout = { guard ->
        // Ungated AND unverified: `removeAllCookies` was issued before the
        // await, offers no cancellation, and may still delete cookies after
        // this flow reported failure — including cookies a reopened session
        // just created. Ignoring the late callback result would not stop
        // that, so THIS operation is registered as outstanding under its own
        // id: another attempt's success cannot clear it, and only its own
        // callback can.
        ProfileQuarantine.quarantine(profileName, guard.operationId, COOKIE_UNVERIFIED_REASON)
      },
      onLateAnswer = { guard ->
        // The provider answered after losing the race, so this operation is no
        // longer in flight — and that is all this callback may conclude. It
        // closes its OWN record; whether the profile is usable again is the
        // registry's condition (nothing outstanding AND a verification that
        // postdates the last settle). A duplicate answer closes nothing.
        ProfileQuarantine.settle(profileName, guard.operationId)
      },
      arm = { _, cb ->
        port.removeAllCookies { wipe ->
          // The provider's boolean is NOT an error flag (see
          // [interpretCookieWipe]): decide the step from the typed delivery
          // and keep the precise reason for the aggregate below. Written
          // before [cb], so the once-latch's CAS publishes it to whichever
          // thread runs [next].
          val step = interpretCookieWipe(wipe) { port.cookiesCleared() }
          cookieError = step.error
          cb(step.ok)
        }
      },
    ) { cookiesOk ->
      if (!cookiesOk) record(cookieError ?: "çerezler temizlenemedi")
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
        var cancelFlushWatchdog: () -> Unit = {}
        cancelFlushWatchdog = port.scheduleTimeout(FLUSH_TIMEOUT_MS) {
          if (flushSettled.compareAndSet(false, true)) {
            record("çerez kalıcılığı: zaman aşımı")
            done(firstError)
          }
        }
        port.flushCookies { flushError ->
          if (!flushSettled.compareAndSet(false, true)) return@flushCookies
          // Settled: drop the queued watchdog (see [SiteClearPort.scheduleTimeout]).
          cancelFlushWatchdog()
          if (flushError != null) record("çerez kalıcılığı başarısız: ${flushError.message}")
          done(firstError)
        }
      }
    }
  }
}
