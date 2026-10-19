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

  /** The call itself failed; [reason] is a safe class code, never provider text. */
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
  const val NO_PROFILE_REASON: String = "profile_unavailable"

  /** The provider's raw `ValueCallback<Boolean>` delivery, mapped null-safely. */
  fun answered(deleted: Boolean?): CookieWipe = CookieWipe.Answered(deleted == true)

  /** A call that threw never reached the cookie store: a typed safe failure. */
  fun failed(@Suppress("UNUSED_PARAMETER") error: Throwable): CookieWipe =
    CookieWipe.Failed("provider")

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
  fun scheduleTimeout(millis: Long, onTimeout: () -> Unit): Watchdog
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

  /** Safe user-facing reason for a cookie call that never reached the store. */
  const val COOKIE_ACCESS_FAILED_PREFIX: String = "çerez deposuna erişilemedi"
  const val PROVIDER_FAILURE: String = "provider"

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
   *  * [CookieWipe.Failed] → a retryable failure with a safe provider code.
   *    Never a success: an unreachable store deleted nothing, and this is the
   *    case the old boolean shape silently confused with an empty jar.
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
      is CookieWipe.Failed -> CookieStep(false, "$COOKIE_ACCESS_FAILED_PREFIX: $PROVIDER_FAILURE")
    }

  fun run(
    supported: Boolean,
    profileName: String,
    port: SiteClearPort,
    operationId: String = AnimeHubDebugLog.newOperationId(),
    onDone: (String?) -> Unit,
  ) {
    val safeOperationId = AnimeHubDebugLog.safeOperationId(operationId) ?: AnimeHubDebugLog.newOperationId()
    val flowStartedAt = System.nanoTime()
    val once = AtomicBoolean(false)
    var firstError: String? = null
    var firstErrorClass: String? = null
    AnimeHubDebugLog.record(
      "clear", "start", "started", safeOperationId,
    )
    fun done(error: String?) {
      if (once.compareAndSet(false, true)) {
        val duration = ((System.nanoTime() - flowStartedAt) / 1_000_000).coerceAtLeast(0)
        if (error == null) {
          // Fully verified wipe: every step of THIS attempt answered. That is
          // recorded as a verification, not as a release — an earlier attempt's
          // timed-out `removeAllCookies` may still be outstanding, and its
          // record belongs to that operation alone (see ProfileQuarantine).
          ProfileQuarantine.verified(profileName)
          AnimeHubDebugLog.record("clear", "verified", "verified", safeOperationId)
        }
        val completion = when {
          error == null -> "ok"
          firstErrorClass == "unsupported" -> "unsupported"
          else -> "failed"
        }
        AnimeHubDebugLog.record(
          "clear", "complete", completion, safeOperationId,
          durationMs = duration,
          errorClass = if (error == null) null else firstErrorClass ?: "provider",
        )
        onDone(error)
      }
    }
    if (!supported) {
      AnimeHubDebugLog.record("clear", "capability", "unsupported_provider", safeOperationId, errorClass = "unsupported")
      firstErrorClass = "unsupported"
      done(SiteProfileErrors.UNSUPPORTED)
      return
    }
    AnimeHubDebugLog.record("clear", "profile_check", "started", safeOperationId)
    val exists =
      runCatching { port.profileExists() }.getOrElse {
        AnimeHubDebugLog.record("clear", "profile_check", "failed", safeOperationId, errorClass = "provider")
        firstErrorClass = "provider"
        done("site verisi temizlenemedi: provider")
        return
      }
    AnimeHubDebugLog.record("clear", "profile_check", if (exists) "present" else "absent", safeOperationId)
    if (!exists) {
      done(null)
      return
    }

    fun record(message: String, errorClass: String = "provider") {
      if (firstError == null) {
        firstError = message
        firstErrorClass = errorClass
      }
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
      phase: String,
      stepName: String,
      timeoutMessage: String,
      onTimeout: (StepGuard) -> Unit = {},
      onLateAnswer: (StepGuard) -> Unit = {},
      arm: (StepGuard, (Boolean) -> Unit) -> Unit,
      next: (Boolean) -> Unit,
    ) {
      val settled = AtomicBoolean(false)
      val guard = StepGuards.create()
      val stepStartedAt = System.nanoTime()
      AnimeHubDebugLog.record(
        "clear", phase, "started", safeOperationId, stepId = guard.operationId,
      )
      val watchdog = port.scheduleTimeout(STEP_TIMEOUT_MS) {
        // Close the mutation window before deciding the result: a callback
        // racing this watchdog on another thread must see the guard shut even
        // if it arrives between the two atomic steps.
        guard.claim()
        if (settled.compareAndSet(false, true)) {
          record("$stepName: $timeoutMessage", "timeout")
          AnimeHubDebugLog.record(
            "clear", phase, "timeout", safeOperationId,
            durationMs = ((System.nanoTime() - stepStartedAt) / 1_000_000).coerceAtLeast(0),
            watchdog = "fired", errorClass = "timeout", stepId = guard.operationId,
          )
          // The hook receives THIS step's guard, so anything it registers is
          // owned by this operation id and nothing else's.
          onTimeout(guard)
          next(false)
        }
      }
      if (watchdog is Watchdog.Refused) {
        // The scheduler refused, so no provider call is issued and there is no
        // destructive operation to quarantine.
        if (settled.compareAndSet(false, true)) {
          record("$stepName: ${SiteProfileErrors.WATCHDOG_REFUSED}", "watchdog_refused")
          AnimeHubDebugLog.record(
            "clear", phase, "failed", safeOperationId,
            durationMs = ((System.nanoTime() - stepStartedAt) / 1_000_000).coerceAtLeast(0),
            watchdog = "refused", errorClass = "watchdog_refused", stepId = guard.operationId,
          )
          next(false)
        }
        return
      }
      AnimeHubDebugLog.record(
        "clear", "watchdog_arm", "armed", safeOperationId, stepId = guard.operationId,
      )
      val cancelWatchdog = (watchdog as Watchdog.Armed).cancel
      arm(guard) { ok ->
        if (settled.compareAndSet(false, true)) {
          // This step is settled: cancel its watchdog instead of leaving a
          // delayed runnable queued — it would keep this port, and through it
          // the WebView and the Activity, reachable for the rest of the window.
          cancelWatchdog()
          AnimeHubDebugLog.record(
            "clear", phase, if (ok) "ok" else "failed", safeOperationId,
            durationMs = ((System.nanoTime() - stepStartedAt) / 1_000_000).coerceAtLeast(0),
            errorClass = if (ok) null else "provider", stepId = guard.operationId,
          )
          next(ok)
        } else {
          AnimeHubDebugLog.record(
            "clear", "late_callback", "late", safeOperationId,
            watchdog = "late", errorClass = "timeout", stepId = guard.operationId,
          )
          onLateAnswer(guard)
        }
      }
    }

    // Reason from the typed cookie delivery, for the aggregate error below.
    var cookieError: String? = null
    awaited(
      "cookie_wipe",
      "çerez temizliği",
      "zaman aşımı",
      onTimeout = { guard ->
        // Ungated AND unverified: `removeAllCookies` was issued before the
        // await, offers no cancellation, and may still delete cookies after
        // this flow reported failure. Keep THIS operation quarantined until
        // its own provider callback settles.
        ProfileQuarantine.quarantine(profileName, guard.operationId, COOKIE_UNVERIFIED_REASON)
        AnimeHubDebugLog.record(
          "profile", "quarantine", "quarantined", safeOperationId,
          errorClass = "cookie_unverified", count = ProfileQuarantine.pendingOperations(profileName).size.toLong(),
          stepId = guard.operationId,
        )
      },
      onLateAnswer = { guard ->
        // The provider answered after losing the race, so this operation is no
        // longer in flight — and that is all this callback may conclude. It
        // closes its OWN record; whether the profile is usable again is the
        // registry's condition (nothing outstanding AND a verification that
        // postdates the last settle). A duplicate answer closes nothing.
        if (ProfileQuarantine.settle(profileName, guard.operationId)) {
          AnimeHubDebugLog.record(
            "profile", "quarantine", "settled", safeOperationId,
            count = ProfileQuarantine.pendingOperations(profileName).size.toLong(),
            stepId = guard.operationId,
          )
        }
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
        "geolocation_wipe",
        "geolocation izin temizliği",
        "zaman aşımı",
        arm = { guard, cb -> port.clearGeolocationPermissions(guard, cb) },
      ) { geoOk ->
        if (!geoOk) record("geolocation izinleri temizlenemedi")
        AnimeHubDebugLog.record("clear", "storage", "started", safeOperationId)
        runCatching { port.deleteAllStorage() }.fold(
          onSuccess = { AnimeHubDebugLog.record("clear", "storage", "ok", safeOperationId) },
          onFailure = {
            record("site deposu temizlenemedi: provider")
            AnimeHubDebugLog.record("clear", "storage", "failed", safeOperationId, errorClass = "provider")
          },
        )
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
        val flushStartedAt = System.nanoTime()
        val flushStepId = StepGuards.create().operationId
        AnimeHubDebugLog.record(
          "clear", "flush", "started", safeOperationId, stepId = flushStepId,
        )
        val flushWatchdog = port.scheduleTimeout(FLUSH_TIMEOUT_MS) {
          if (flushSettled.compareAndSet(false, true)) {
            record("çerez kalıcılığı: zaman aşımı", "timeout")
            AnimeHubDebugLog.record(
              "clear", "flush", "timeout", safeOperationId,
              durationMs = ((System.nanoTime() - flushStartedAt) / 1_000_000).coerceAtLeast(0),
              watchdog = "fired", errorClass = "timeout", stepId = flushStepId,
            )
            done(firstError)
          }
        }
        if (flushWatchdog is Watchdog.Refused) {
          // Unbounded ⇒ the flush is never started, and the wipe is reported as
          // the failure it is: Rust keeps the encrypted session blobs for a retry.
          if (flushSettled.compareAndSet(false, true)) {
            record("çerez kalıcılığı: ${SiteProfileErrors.WATCHDOG_REFUSED}", "watchdog_refused")
            AnimeHubDebugLog.record(
              "clear", "flush", "failed", safeOperationId,
              durationMs = ((System.nanoTime() - flushStartedAt) / 1_000_000).coerceAtLeast(0),
              watchdog = "refused", errorClass = "watchdog_refused", stepId = flushStepId,
            )
            done(firstError)
          }
        } else {
          AnimeHubDebugLog.record(
            "clear", "watchdog_arm", "armed", safeOperationId, stepId = flushStepId,
          )
          val cancelFlushWatchdog = (flushWatchdog as Watchdog.Armed).cancel
          port.flushCookies { flushError ->
            if (!flushSettled.compareAndSet(false, true)) {
              AnimeHubDebugLog.record(
                "clear", "late_callback", "late", safeOperationId,
                watchdog = "late", errorClass = "timeout", stepId = flushStepId,
              )
              return@flushCookies
            }
            // Settled: drop the queued watchdog (see [SiteClearPort.scheduleTimeout]).
            cancelFlushWatchdog()
            if (flushError != null) {
              record("çerez kalıcılığı başarısız: provider", "provider")
            }
            AnimeHubDebugLog.record(
              "clear", "flush", if (flushError == null) "ok" else "failed", safeOperationId,
              durationMs = ((System.nanoTime() - flushStartedAt) / 1_000_000).coerceAtLeast(0),
              errorClass = if (flushError == null) null else "provider", stepId = flushStepId,
            )
            done(firstError)
          }
        }
      }
    }
  }
}
